//! [`YevmBlockExecutor`]: Ethereum block execution with every EVM call run by yevm.

use super::evm::{YevmError, YevmEvm, yevm_spec};
use alloy_consensus::{Transaction, TxType};
use alloy_eip7928::{BlockAccessIndex, BlockAccessList};
use alloy_eips::{
    eip2935::HISTORY_STORAGE_ADDRESS,
    eip4788::BEACON_ROOTS_ADDRESS,
    eip7002::{WITHDRAWAL_REQUEST_PREDEPLOY_ADDRESS, WITHDRAWAL_REQUEST_TYPE},
    eip7251::{CONSOLIDATION_REQUEST_PREDEPLOY_ADDRESS, CONSOLIDATION_REQUEST_TYPE},
    eip7685::Requests,
};
use alloy_primitives::{Address, Bytes, U256, map::AddressMap};
use evm2::{
    BaseEvmTypes, SpecId, TxResult, TxResultWithState,
    block::ethereum::execution::{BUILDER_DEPOSIT_REQUEST_TYPE, BUILDER_EXIT_REQUEST_TYPE},
    block::ethereum::{
        EthTransactionResultWithState,
        execution::{
            BlockExecutionContext, BlockSystemCalls, block_requests_from_receipts,
            map_transaction_error,
        },
    },
    evm::{
        AccountInfo, BUILDER_DEPOSIT_REQUEST_ADDRESS, BUILDER_EXIT_REQUEST_ADDRESS, Bal,
        PendingState,
    },
};
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use reth_ethereum_primitives::{Receipt, TransactionSigned};
use reth_evm::{
    BlockExecutionError, BlockExecutionOutput, BlockExecutor, BlockValidationError, Evm,
    ExecutorTx, GasOutput, ReceiptBuilder, ReceiptBuilderCtx, RecoveredTx, StateUpdate,
    execute::map_database_error,
};
use reth_evm_ethereum::{EthBlockExecutionCtx, RethReceiptBuilder};
use reth_execution_types::{BlockExecutionResult, BlockState};
use std::sync::Arc;

type StateHook = Box<dyn FnMut(StateUpdate) + Send>;

/// Ethereum block executor that runs transactions and system calls with yevm.
///
/// It follows evm2's Ethereum block executor step by step, but only for the specifications yevm
/// implements ([`yevm_spec`]); every other block fails with [`YevmError::UnsupportedSpec`].
pub struct YevmBlockExecutor<'a, C> {
    evm: YevmEvm<'a>,
    ctx: EthBlockExecutionCtx<'a>,
    chain_spec: &'a C,
    block_state: BlockState,
    state_hook: Option<StateHook>,
    receipts: Vec<Receipt>,
    cumulative_gas_used: u64,
    /// Amsterdam (EIP-7778, EIP-8037): the block's execution and state gas, counted apart.
    block_regular_gas_used: u64,
    block_state_gas_used: u64,
    blob_gas_used: u64,
    /// The block access list being built (EIP-7928), when the caller enabled the builder.
    bal: Option<Bal>,
    /// The access list entries of a transaction executed but not yet committed.
    pending_access: Option<PendingState>,
    /// Where to report the block's loaded keys when it finishes (silt D13).
    reads: Option<&'a silt_node::reads::ReadTouchSink>,
    /// Hashes of the transactions executed, in order, while `reads` is set:
    /// the sink keeps the keys only if they match a block the engine
    /// announced.
    tx_hashes: Vec<[u8; 32]>,
}

impl<C> core::fmt::Debug for YevmBlockExecutor<'_, C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("YevmBlockExecutor")
            .field("evm", &self.evm)
            .field("receipts", &self.receipts.len())
            .field("cumulative_gas_used", &self.cumulative_gas_used)
            .finish_non_exhaustive()
    }
}

impl<'a, C: EthChainSpec + EthereumHardforks> YevmBlockExecutor<'a, C> {
    /// Creates an executor for the block in `evm`'s environment.
    pub fn new(evm: YevmEvm<'a>, ctx: EthBlockExecutionCtx<'a>, chain_spec: &'a C) -> Self {
        let receipts = Vec::with_capacity(ctx.tx_count_hint.unwrap_or_default());
        Self {
            evm,
            ctx,
            chain_spec,
            block_state: BlockState::new(),
            state_hook: None,
            receipts,
            cumulative_gas_used: 0,
            block_regular_gas_used: 0,
            block_state_gas_used: 0,
            blob_gas_used: 0,
            bal: None,
            pending_access: None,
            reads: None,
            tx_hashes: Vec::new(),
        }
    }

    /// Reports the block's loaded keys to `reads` when it finishes.
    pub fn with_reads(mut self, reads: Option<&'a silt_node::reads::ReadTouchSink>) -> Self {
        self.reads = reads;
        self
    }

    fn spec(&self) -> Result<SpecId, BlockExecutionError> {
        let spec = self.evm.env().spec;
        if yevm_spec(spec).is_none() {
            return Err(BlockExecutionError::other(YevmError::UnsupportedSpec(spec)));
        }
        Ok(spec)
    }

    /// Whether the block counts execution and state gas apart (Amsterdam).
    fn separate_block_gas(&self) -> bool {
        self.evm.env().spec.enables(SpecId::AMSTERDAM)
    }

    /// The block access index of the next change: 0 before the transactions, `i + 1` for the
    /// `i`th transaction, and one past the last after them (EIP-7928).
    fn bal_index(&self, after_transactions: bool) -> BlockAccessIndex {
        let index =
            self.receipts.len() as u64 + u64::from(after_transactions || !self.receipts.is_empty());
        BlockAccessIndex::new(index)
    }

    /// Records `pending` in the block access list, when it is being built.
    fn record_access(&mut self, index: BlockAccessIndex, pending: &PendingState) {
        if let Some(bal) = self.bal.as_mut() {
            bal.commit(index, pending.clone());
        }
    }

    /// Records the last execution's changes in the block access list, when it is being built.
    fn record_access_in_place(
        &mut self,
        index: BlockAccessIndex,
    ) -> Result<(), BlockExecutionError> {
        if self.bal.is_some() {
            let pending = self.evm.access_list_state().map_err(map_database_error)?;
            self.record_access(index, &pending);
        }
        Ok(())
    }

    fn block_number(&self) -> u64 {
        self.evm.env().block.number.saturating_to()
    }

    /// Accepts the state changes of the EVM's last execution into its overlay and the block
    /// state, streaming them to the state hook.
    fn commit_in_place(&mut self) -> Result<(), BlockExecutionError> {
        let mut update = self.state_hook.is_some().then(StateUpdate::default);
        let block_state = &mut self.block_state;
        self.evm
            .commit_in_place(|changes| block_state.commit_account_changes(changes, update.as_mut()))
            .map_err(map_database_error)?;
        if let (Some(hook), Some(update)) = (self.state_hook.as_mut(), update)
            && !update.is_empty()
        {
            hook(update);
        }
        Ok(())
    }

    /// Accepts `pending` into the overlay and the block state, streaming it to the state hook.
    fn commit_pending(&mut self, pending: &PendingState) {
        let mut update = self.state_hook.is_some().then(StateUpdate::default);
        for changes in pending.account_changes() {
            self.block_state
                .commit_account_changes(changes, update.as_mut());
        }
        if let (Some(hook), Some(update)) = (self.state_hook.as_mut(), update)
            && !update.is_empty()
        {
            hook(update);
        }
        self.evm.commit_state(pending);
    }

    /// Runs a system call, commits it and returns its output. Calls to the history and beacon
    /// root contracts may fail; a failed request contract call invalidates the block.
    fn system_call(
        &mut self,
        address: Address,
        data: Bytes,
        index: BlockAccessIndex,
    ) -> Result<Bytes, BlockExecutionError> {
        let result = self
            .evm
            .system_call_in_place(address, data)
            .map_err(reth_evm::execute::map_handler_error)?;
        if !result.is_success()
            && !matches!(address, BEACON_ROOTS_ADDRESS | HISTORY_STORAGE_ADDRESS)
        {
            let message = format!("{:?}", result.stop);
            return Err(match address {
                WITHDRAWAL_REQUEST_PREDEPLOY_ADDRESS => {
                    BlockValidationError::WithdrawalRequestsContractCall { message }.into()
                }
                CONSOLIDATION_REQUEST_PREDEPLOY_ADDRESS => {
                    BlockValidationError::ConsolidationRequestsContractCall { message }.into()
                }
                _ => {
                    BlockExecutionError::msg(format!("system call to {address} failed: {message}"))
                }
            });
        }
        self.record_access_in_place(index)?;
        self.commit_in_place()?;
        Ok(result.output)
    }

    /// Credits the block's withdrawals. A zero withdrawal to an empty account only touches it,
    /// which EIP-161 turns into a no-op.
    fn apply_withdrawals(&mut self) -> Result<(), BlockExecutionError> {
        let mut increments = AddressMap::<U256>::default();
        for withdrawal in self
            .ctx
            .withdrawals
            .iter()
            .flat_map(|withdrawals| withdrawals.iter())
        {
            *increments.entry(withdrawal.address).or_default() += withdrawal.amount_wei();
        }
        let mut pending = PendingState::default();
        // Every withdrawal's account is read, even when its balance does not change (EIP-7928).
        let mut reads = PendingState::default();
        let index = self.bal_index(true);
        for (address, increment) in increments {
            let original = self.evm.account_info(&address)?;
            let current =
                if increment.is_zero() && original.as_ref().is_none_or(AccountInfo::is_empty) {
                    None
                } else {
                    let mut current = original.clone().unwrap_or_else(AccountInfo::empty);
                    current.balance = current.balance.saturating_add(increment);
                    Some(current)
                };
            if original != current {
                pending.insert_account(address, original.clone(), current);
            } else {
                reads.insert_account(address, original.clone(), original);
            }
        }
        self.record_access(index, &pending);
        self.record_access(index, &reads);
        self.commit_pending(&pending);
        Ok(())
    }

    fn record_receipt(
        &mut self,
        tx_type: TxType,
        blob_gas_used: u64,
        result: TxResult,
    ) -> GasOutput {
        let tx_gas_used = result.tx_gas_used();
        let regular_gas_used = result.execution_gas_spent();
        let state_gas_used = result.state_gas_spent();
        self.cumulative_gas_used += tx_gas_used;
        self.block_regular_gas_used = self.block_regular_gas_used.saturating_add(regular_gas_used);
        self.block_state_gas_used = self.block_state_gas_used.saturating_add(state_gas_used);
        self.blob_gas_used += blob_gas_used;
        self.receipts
            .push(
                RethReceiptBuilder::default().build_receipt::<BaseEvmTypes>(ReceiptBuilderCtx {
                    tx_type,
                    result,
                    cumulative_gas_used: self.cumulative_gas_used,
                }),
            );
        if self.separate_block_gas() {
            GasOutput::new_with_regular(tx_gas_used, regular_gas_used, state_gas_used)
        } else {
            GasOutput::new_with_regular(tx_gas_used, tx_gas_used, 0)
        }
    }
}

impl<'a, C: EthChainSpec + EthereumHardforks> BlockExecutor for YevmBlockExecutor<'a, C> {
    type Transaction = TransactionSigned;
    type Receipt = Receipt;
    type Evm = YevmEvm<'a>;
    type TransactionResultWithState = EthTransactionResultWithState<BaseEvmTypes, TxType>;
    type BlockAccessList = Bal;

    fn evm(&self) -> &Self::Evm {
        &self.evm
    }

    fn evm_mut(&mut self) -> &mut Self::Evm {
        &mut self.evm
    }

    fn set_state_hook(&mut self, hook: impl FnMut(StateUpdate) + Send + 'static) -> bool {
        self.state_hook = Some(Box::new(hook));
        true
    }

    fn convert_block_access_list(
        block_access_list: &BlockAccessList,
    ) -> Result<Self::BlockAccessList, BlockExecutionError> {
        Bal::try_from(block_access_list.as_slice()).map_err(BlockExecutionError::other)
    }

    // A block access list given in advance is not needed: yevm executes transactions in order.
    fn set_block_access_list(&mut self, _block_access_list: Arc<Self::BlockAccessList>) {}

    // Indices follow the block's own order, see `bal_index`.
    fn set_block_access_index(&mut self, _index: BlockAccessIndex) {}

    fn enable_block_access_list_builder(&mut self) {
        self.bal = Some(Bal::new());
    }

    fn take_block_access_list(&mut self) -> Option<BlockAccessList> {
        self.bal.take().map(|bal| bal.to_alloy_bal())
    }

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        let spec = self.spec()?;
        let block_number = self.block_number();
        if spec.enables(SpecId::CANCUN) {
            let root = self
                .ctx
                .parent_beacon_block_root
                .ok_or(BlockValidationError::MissingParentBeaconBlockRoot)?;
            if block_number == 0 {
                if !root.is_zero() {
                    return Err(
                        BlockValidationError::CancunGenesisParentBeaconBlockRootNotZero {
                            parent_beacon_block_root: root,
                        }
                        .into(),
                    );
                }
            } else {
                let index = self.bal_index(false);
                self.system_call(BEACON_ROOTS_ADDRESS, root.0.into(), index)?;
            }
        }
        if spec.enables(SpecId::PRAGUE) && block_number != 0 {
            let index = self.bal_index(false);
            self.system_call(
                HISTORY_STORAGE_ADDRESS,
                self.ctx.parent_hash.0.into(),
                index,
            )?;
        }
        Ok(())
    }

    fn into_state(self) -> reth_evm::BundleState {
        self.block_state.into_bundle()
    }

    fn validate_transaction_gas_limit(
        &mut self,
        gas_limit: u64,
    ) -> Result<(), BlockExecutionError> {
        let block_gas_limit = self.evm.env().block.gas_limit.saturating_to::<u64>();
        let regular_limit = gas_limit.min(self.evm.env().version.tx_gas_limit_cap);
        if self.separate_block_gas() {
            let regular_available = block_gas_limit.saturating_sub(self.block_regular_gas_used);
            let state_available = block_gas_limit.saturating_sub(self.block_state_gas_used);
            let available = if regular_limit > regular_available {
                Some(regular_available)
            } else {
                (gas_limit > state_available).then_some(state_available)
            };
            if let Some(block_available_gas) = available {
                return Err(
                    BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                        transaction_gas_limit: gas_limit,
                        block_available_gas,
                    }
                    .into(),
                );
            }
            return Ok(());
        }
        let block_available_gas = block_gas_limit.saturating_sub(self.cumulative_gas_used);
        if regular_limit > block_available_gas {
            return Err(
                BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                    transaction_gas_limit: gas_limit,
                    block_available_gas,
                }
                .into(),
            );
        }
        Ok(())
    }

    fn execute_transaction_without_commit(
        &mut self,
        transaction: impl ExecutorTx<Self>,
    ) -> Result<Self::TransactionResultWithState, BlockExecutionError> {
        self.spec()?;
        let (tx_env, tx) = transaction.into_parts();
        let tx = tx.tx();
        self.validate_transaction_gas_limit(tx.gas_limit())?;
        if self.reads.is_some() {
            self.tx_hashes.push(tx.tx_hash().0);
        }
        let result = self
            .evm
            .execute(&tx_env)
            .map_err(|error| map_transaction_error(error, *tx.tx_hash()))?;
        if self.bal.is_some() {
            self.pending_access = Some(self.evm.access_list_state().map_err(map_database_error)?);
        }
        Ok(EthTransactionResultWithState::new(
            result,
            tx.tx_type(),
            tx.blob_gas_used().unwrap_or_default(),
        ))
    }

    fn commit_transaction(
        &mut self,
        output: Self::TransactionResultWithState,
    ) -> Result<GasOutput, BlockExecutionError> {
        let (
            TxResultWithState {
                result,
                pending_state,
                ..
            },
            tx_type,
            blob_gas_used,
        ) = output.into_parts();
        let index = self.bal_index(true);
        if let Some(access) = self.pending_access.take() {
            self.record_access(index, &access);
        }
        self.commit_pending(&pending_state);
        Ok(self.record_receipt(tx_type, blob_gas_used, result))
    }

    // `f` cannot see the state changes, so they are committed straight from yevm's journal
    // instead of being detached into a pending state first.
    fn execute_transaction_with_result_closure(
        &mut self,
        transaction: impl ExecutorTx<Self>,
        f: impl FnOnce(&TxResult),
    ) -> Result<GasOutput, BlockExecutionError> {
        self.spec()?;
        let (tx_env, tx) = transaction.into_parts();
        let tx = tx.tx();
        self.validate_transaction_gas_limit(tx.gas_limit())?;
        if self.reads.is_some() {
            self.tx_hashes.push(tx.tx_hash().0);
        }
        let result = self
            .evm
            .execute_in_place(&tx_env)
            .map_err(|error| map_transaction_error(error, *tx.tx_hash()))?;
        f(&result);
        let index = self.bal_index(true);
        self.record_access_in_place(index)?;
        self.commit_in_place()?;
        Ok(self.record_receipt(tx.tx_type(), tx.blob_gas_used().unwrap_or_default(), result))
    }

    fn receipts(&self) -> &[Self::Receipt] {
        &self.receipts
    }

    fn finish_with_block_access_list(
        mut self,
    ) -> Result<(BlockExecutionOutput<Self::Receipt>, Option<BlockAccessList>), BlockExecutionError>
    {
        let spec = self.spec()?;
        let context = BlockExecutionContext {
            system_calls: Some(BlockSystemCalls {
                parent_hash: self.ctx.parent_hash,
                parent_beacon_block_root: self.ctx.parent_beacon_block_root,
            }),
            ommers: Some(self.ctx.ommers),
            withdrawals: None,
            deposit_contract_address: self
                .chain_spec
                .deposit_contract()
                .map(|contract| contract.address),
        };
        let mut requests: Requests = block_requests_from_receipts(spec, context, &self.receipts)?;
        if spec.enables(SpecId::SHANGHAI) {
            self.apply_withdrawals()?;
        }
        let index = self.bal_index(true);
        if spec.enables(SpecId::PRAGUE) {
            let withdrawals =
                self.system_call(WITHDRAWAL_REQUEST_PREDEPLOY_ADDRESS, Bytes::new(), index)?;
            requests.push_request_with_type(WITHDRAWAL_REQUEST_TYPE, withdrawals.iter().copied());
            let consolidations =
                self.system_call(CONSOLIDATION_REQUEST_PREDEPLOY_ADDRESS, Bytes::new(), index)?;
            requests
                .push_request_with_type(CONSOLIDATION_REQUEST_TYPE, consolidations.iter().copied());
        }
        // EIP-8282: builder deposit and exit requests.
        if spec.enables(SpecId::AMSTERDAM) {
            let deposits =
                self.system_call(BUILDER_DEPOSIT_REQUEST_ADDRESS, Bytes::new(), index)?;
            requests.push_request_with_type(BUILDER_DEPOSIT_REQUEST_TYPE, deposits.iter().copied());
            let exits = self.system_call(BUILDER_EXIT_REQUEST_ADDRESS, Bytes::new(), index)?;
            requests.push_request_with_type(BUILDER_EXIT_REQUEST_TYPE, exits.iter().copied());
        }
        let gas_used = if self.separate_block_gas() {
            self.block_regular_gas_used.max(self.block_state_gas_used)
        } else {
            self.cumulative_gas_used
        };
        if let Some(sink) = self.reads {
            let started = std::time::Instant::now();
            let keys = self.evm.loaded_keys();
            let txs = silt_node::reads::tx_digest(&self.tx_hashes);
            sink.put(
                self.block_number(),
                self.ctx.parent_hash.0,
                txs,
                keys,
                started.elapsed().as_micros() as u64,
            );
        }
        let output = BlockExecutionOutput::new(
            BlockExecutionResult {
                receipts: self.receipts,
                requests,
                gas_used,
                blob_gas_used: self.blob_gas_used,
            },
            self.block_state.into_bundle(),
        );
        let block_access_list = self.bal.take().map(|bal| bal.to_alloy_bal());
        // EIP-7928: every account and every distinct slot counts as an item, and the items may
        // not cost more than the block's gas limit at 2000 gas each.
        if let Some(list) = &block_access_list {
            let items: u64 = list
                .iter()
                .map(|account| {
                    let mut slots: Vec<_> = account
                        .storage_changes
                        .iter()
                        .map(|change| change.slot)
                        .chain(account.storage_reads.iter().copied())
                        .collect();
                    slots.sort_unstable();
                    slots.dedup();
                    1 + slots.len() as u64
                })
                .sum();
            let gas_limit = self.evm.env().block.gas_limit.saturating_to::<u64>();
            if items > gas_limit / 2000 {
                return Err(BlockExecutionError::msg(format!(
                    "block access list has {items} items, more than the gas limit allows"
                )));
            }
        }
        Ok((output, block_access_list))
    }
}
