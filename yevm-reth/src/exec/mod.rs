//! Ethereum EVM configuration that executes transactions with [yevm].
//!
//! [`YevmEvmConfig`] is a [`ConfigureEvm`] for Ethereum nodes: it builds block environments and
//! assembles blocks exactly like [`EthEvmConfig`], but every EVM call — transactions, EIP-4788 and
//! EIP-2935 pre-block system calls, and EIP-7002/EIP-7251 request system calls — runs on the yevm
//! interpreter instead of evm2. evm2 remains reth's state model: yevm reads through an evm2
//! [`CacheDB`](evm2::evm::CacheDB) overlay and its results are converted into evm2 transaction
//! results and pending state.
//!
//! # Limitations
//!
//! - yevm implements only the Osaka rules ([`YEVM_SPEC`]) and has no fork schedule; blocks of any
//!   other fork fail with [`YevmError::UnsupportedSpec`] instead of silently diverging.
//! - evm2 inspectors cannot run on yevm, so tracing APIs fail with
//!   [`YevmError::UnsupportedInspector`].
//! - Engine prewarming loads transaction state into the shared cache, but sends no state root
//!   hints, since those need evm2's in-place transaction view; every prewarmed transaction counts
//!   in prewarming's transaction error metric.
//! - Precompiles cannot be moved (`eth_simulateV1`'s `movePrecompileToAddress`).
//! - Detached transaction state ([`YevmEvm::execute`], and block execution that commits only on a
//!   condition, such as payload building) reports created accounts as changed ones, since evm2's
//!   public [`PendingState`](evm2::evm::PendingState) API cannot mark them as created. Regular
//!   block execution commits from yevm's journal and reports them like evm2.
//!
//! [yevm]: https://github.com/sergey-melnychuk/yevm

use alloy_consensus::Header;
use alloy_eips::Decodable2718;
use alloy_primitives::Bytes;
use alloy_rpc_types_engine::ExecutionData;
use core::convert::Infallible;
use evm2::{BaseEvmTypes, evm::DynDatabase};
use reth_chainspec::{ChainSpec, EthChainSpec, EthereumHardforks};
use reth_ethereum_forks::Hardforks;
use reth_ethereum_primitives::{Block, EthPrimitives, Receipt, TransactionSigned};
use reth_evm::{
    BlockExecutorFactory, ConfigureEngineEvm, ConfigureEvm, EvmEnvFor, ExecutableTxIterator,
    ExecutionCtxFor, NextBlockEnvAttributes,
};
use reth_evm_ethereum::{
    EthBlockAssembler, EthBlockExecutionCtx, EthEvmConfig, EthEvmEnv, ExecutableRecoveredTx,
};
use reth_primitives_traits::{SealedBlock, SealedHeader, SignedTransaction};
use reth_storage_errors::any::AnyError;
use std::sync::Arc;

mod convert;
mod state;

mod evm;
pub use evm::{YEVM_SPEC, YevmError, YevmEvm};

mod executor;
pub use executor::YevmBlockExecutor;

/// Creates [`YevmBlockExecutor`]s and [`YevmEvm`]s.
#[derive(Debug)]
pub struct YevmBlockExecutorFactory<C = ChainSpec> {
    chain_spec: Arc<C>,
}

impl<C> Clone for YevmBlockExecutorFactory<C> {
    fn clone(&self) -> Self {
        Self {
            chain_spec: self.chain_spec.clone(),
        }
    }
}

impl<C> YevmBlockExecutorFactory<C> {
    /// Creates a factory for the given chain.
    pub const fn new(chain_spec: Arc<C>) -> Self {
        Self { chain_spec }
    }
}

impl<C> BlockExecutorFactory for YevmBlockExecutorFactory<C>
where
    C: EthChainSpec + EthereumHardforks,
{
    /// yevm needs no EVM factory state.
    type EvmFactory = ();
    type EvmTypes = BaseEvmTypes;
    type Transaction = TransactionSigned;
    type Receipt = Receipt;
    type Evm<'a> = YevmEvm<'a>;
    type EvmEnv = EthEvmEnv;
    type ExecutionCtx<'a>
        = EthBlockExecutionCtx<'a>
    where
        Self: 'a;
    type Executor<'a>
        = YevmBlockExecutor<'a, C>
    where
        Self: 'a;

    fn create_executor<'a>(
        &'a self,
        evm: Self::Evm<'a>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> Self::Executor<'a>
    where
        Self: 'a,
    {
        YevmBlockExecutor::new(evm, ctx, &self.chain_spec)
    }

    fn evm_factory(&self) -> &Self::EvmFactory {
        &()
    }

    fn evm_with_env<'a, DB>(&self, db: DB, evm_env: Self::EvmEnv) -> Self::Evm<'a>
    where
        DB: DynDatabase + 'a,
    {
        YevmEvm::new(db, evm_env)
    }
}

/// Ethereum EVM configuration that executes with yevm.
///
/// Environments, execution contexts and blocks are built by an inner [`EthEvmConfig`], so they
/// are identical to the default Ethereum configuration.
#[derive(Debug, Clone)]
pub struct YevmEvmConfig<C = ChainSpec> {
    /// Builds environments, execution contexts and blocks.
    pub eth: EthEvmConfig<C>,
    /// Creates yevm-backed executors.
    pub executor_factory: YevmBlockExecutorFactory<C>,
}

impl<C> YevmEvmConfig<C> {
    /// Creates a configuration for the given chain.
    pub fn new(chain_spec: Arc<C>) -> Self {
        Self {
            executor_factory: YevmBlockExecutorFactory::new(chain_spec.clone()),
            eth: EthEvmConfig::new(chain_spec),
        }
    }
}

impl<C> ConfigureEvm for YevmEvmConfig<C>
where
    C: EthChainSpec<Header = Header> + EthereumHardforks + Hardforks + 'static,
{
    type Primitives = EthPrimitives;
    type Error = Infallible;
    type NextBlockEnvCtx = NextBlockEnvAttributes;
    type BlockExecutorFactory = YevmBlockExecutorFactory<C>;
    type BlockAssembler = EthBlockAssembler<C>;

    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        &self.executor_factory
    }

    fn block_assembler(&self) -> &Self::BlockAssembler {
        &self.eth.block_assembler
    }

    fn evm_env(&self, header: &Header) -> Result<EvmEnvFor<Self>, Self::Error> {
        self.eth.evm_env(header)
    }

    fn next_evm_env(
        &self,
        parent: &Header,
        attributes: &NextBlockEnvAttributes,
    ) -> Result<EvmEnvFor<Self>, Self::Error> {
        self.eth.next_evm_env(parent, attributes)
    }

    fn context_for_block<'a>(
        &self,
        block: &'a SealedBlock<Block>,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error>
    where
        Self: 'a,
    {
        self.eth.context_for_block(block)
    }

    fn context_for_next_block(
        &self,
        parent: &SealedHeader,
        attributes: Self::NextBlockEnvCtx,
    ) -> Result<ExecutionCtxFor<'_, Self>, Self::Error> {
        self.eth.context_for_next_block(parent, attributes)
    }
}

impl<C> ConfigureEngineEvm<ExecutionData> for YevmEvmConfig<C>
where
    C: EthChainSpec<Header = Header> + EthereumHardforks + Hardforks + 'static,
{
    fn evm_env_for_payload(&self, payload: &ExecutionData) -> Result<EvmEnvFor<Self>, Self::Error> {
        self.eth.evm_env_for_payload(payload)
    }

    fn context_for_payload<'a>(
        &self,
        payload: &'a ExecutionData,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        self.eth.context_for_payload(payload)
    }

    fn tx_iterator_for_payload(
        &self,
        payload: &ExecutionData,
    ) -> Result<impl ExecutableTxIterator<Self>, Self::Error> {
        let txs = payload.payload.transactions().clone();
        let sender_recovery_cache = self.eth.sender_recovery_cache.clone();
        let convert = move |tx: Bytes| {
            let tx = TransactionSigned::decode_2718_exact(tx.as_ref()).map_err(AnyError::new)?;
            let signer = if let Some(cache) = &sender_recovery_cache {
                cache.recover(&tx)
            } else {
                tx.try_recover()
            }
            .map_err(AnyError::new)?;
            Ok::<_, AnyError>(ExecutableRecoveredTx::new(tx.with_signer(signer)))
        };
        Ok((txs, convert))
    }
}
