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
    /// Where executors report each block's read touches with their values (silt PLAN D13/D15).
    reads: Option<Arc<silt_node::reads::ReadTouchSink>>,
}

impl<C> Clone for YevmBlockExecutorFactory<C> {
    fn clone(&self) -> Self {
        Self {
            chain_spec: self.chain_spec.clone(),
            reads: self.reads.clone(),
        }
    }
}

impl<C> YevmBlockExecutorFactory<C> {
    /// Creates a factory for the given chain.
    pub const fn new(chain_spec: Arc<C>) -> Self {
        Self {
            chain_spec,
            reads: None,
        }
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
        // silt D15: claim the ticket `context_for_payload` left on this thread (reth creates the
        // engine's executor right after, here), so only the engine's own execution can report
        // read values. Every executor claims, so a ticket never outlives the one it was left for.
        let ticket = silt_node::reads::ReadTouchSink::claim_ticket();
        YevmBlockExecutor::new(evm, ctx, self.chain_spec.as_ref())
            .with_reads(self.reads.as_deref(), ticket)
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

    /// Reports, after every block the engine executes, the accounts and slots the block loaded,
    /// with their values at its end, to `sink` for silt's read touches (silt PLAN D13/D15). Only
    /// the engine's own executions report: `context_for_payload` leaves a ticket the executor
    /// claims, and RPC executions never hold one.
    pub fn with_read_touches(mut self, sink: Arc<silt_node::reads::ReadTouchSink>) -> Self {
        self.executor_factory.reads = Some(sink);
        self
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
        // silt D13/D15: only the engine's own executions report read touches, so the node's RPC
        // (eth_simulateV1 on the same parent, with overrides) can't add any. `expect` also
        // leaves this thread's ticket for the executor reth creates next, right here.
        if let Some(sink) = &self.executor_factory.reads {
            let hashes: Vec<[u8; 32]> = payload
                .payload
                .transactions()
                .iter()
                .map(|raw| alloy_primitives::keccak256(raw).0)
                .collect();
            sink.expect(
                payload.payload.block_number(),
                payload.payload.parent_hash().0,
                silt_node::reads::tx_digest(&hashes),
            );
        }
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
