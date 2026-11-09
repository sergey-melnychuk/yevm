//! [`YevmEvm`]: reth's [`Evm`] interface implemented with the yevm interpreter.

use super::{
    convert,
    state::{DbState, NoChain, load_account},
};
use alloy_consensus::{Transaction, transaction::Recovered};
use alloy_eips::{
    eip2718::Typed2718, eip4844::VERSIONED_HASH_VERSION_KZG, eip7002::SYSTEM_ADDRESS,
};
use alloy_primitives::{Address, B256, Bytes, U256, map::AddressMap};
use evm2::{
    AnyError, BaseEvmTypes, DatabaseError, EvmFeatures, ExecutedTx, Inspector, Precompiles, SpecId,
    TxResult, TxResultWithState,
    bytecode::Bytecode,
    ethereum::TxEnvelope,
    evm::{
        AccountChanges, AccountInfo, CacheDB, DynDatabase, PendingState, SYSTEM_CALL_GAS_LIMIT,
        StorageOverlay, StorageSlot, Tracked, precompile::PrecompileProvider,
    },
    precompiles::{MovePrecompileError, PrecompileId},
    registry::HandlerError,
};
use reth_evm::{
    BlockExecutionError, Evm, EvmEnv,
    execute::{map_database_error, map_handler_error},
};
use reth_evm_ethereum::EthEvmEnv;
use yevm_core::{
    Call, Int, Tx,
    cache::Cache,
    exe::{CallResult, Executor},
    misc::is_precompile,
    state::{Account, State},
};
use yevm_misc::buf::Buf;

/// The only specification yevm implements.
///
/// yevm has no fork schedule: it applies the latest mainnet rules (Osaka) to every block, so
/// executing any other fork with it would silently diverge.
pub const YEVM_SPEC: SpecId = SpecId::OSAKA;

/// Errors specific to executing with yevm.
#[derive(Debug, thiserror::Error)]
pub enum YevmError {
    /// The active specification is not [`YEVM_SPEC`].
    #[error("yevm only implements {YEVM_SPEC:?}, cannot execute {0:?}")]
    UnsupportedSpec(SpecId),
    /// An evm2 inspector is installed; yevm cannot drive evm2 inspectors.
    #[error("yevm cannot run evm2 inspectors (tracing is not supported)")]
    UnsupportedInspector,
    /// The caller needs evm2's in-place transaction view, which yevm does not produce.
    #[error("yevm does not produce evm2 executed-transaction views")]
    UnsupportedExecutedTx,
    /// yevm rejected or failed a transaction that passed [`YevmEvm::execute`]'s validation.
    #[error("yevm failed a validated transaction: {0}")]
    Execution(#[source] yevm_core::Error),
    /// yevm suspended execution without requesting state.
    #[error("yevm suspended execution without requesting state")]
    Stalled,
}

impl From<YevmError> for HandlerError {
    fn from(error: YevmError) -> Self {
        Self::Fatal(AnyError::new(error))
    }
}

/// Polls `future` once; yevm's executor completes in one poll when its state never misses.
fn poll_once<F: core::future::Future>(future: F) -> Result<F::Output, YevmError> {
    let mut future = core::pin::pin!(future);
    match future.as_mut().poll(&mut core::task::Context::from_waker(
        core::task::Waker::noop(),
    )) {
        core::task::Poll::Ready(output) => Ok(output),
        core::task::Poll::Pending => Err(YevmError::Stalled),
    }
}

/// A transaction that passed [`YevmEvm::validate`].
struct Validated {
    /// EIP-7623 floor gas.
    floor_gas: u64,
    sender: Sender,
}

/// A transaction's sender as validation read it, loaded into yevm's journal before execution so
/// that it is read once.
struct Sender {
    address: Address,
    /// The account with its code, or `None` if it does not exist.
    info: Option<AccountInfo>,
    /// Balance the sender is credited up to before execution, for a sender that cannot pay its
    /// upfront cost when the balance check is disabled and the top-up enabled.
    top_up: Option<U256>,
}

/// An EVM that executes transactions with yevm over an evm2 state overlay.
///
/// Reads go through a [`CacheDB`] overlay of the wrapped database; committed transactions are
/// written to the overlay, exactly like evm2's own EVM.
pub struct YevmEvm<'a> {
    db: CacheDB<Box<dyn DynDatabase + 'a>>,
    /// yevm's journal, cleared and reused by every transaction to keep its allocations.
    cache: Cache,
    /// The accounts a transaction loaded with their original info, reused like `cache`.
    originals: AddressMap<Option<AccountInfo>>,
    /// An account's changed storage while [`Self::commit_in_place`] commits it, reused to keep
    /// its allocation.
    storage: StorageOverlay,
    env: EthEvmEnv,
    /// Inspector installed by callers; its presence makes execution fail since yevm cannot run
    /// it.
    inspector: Option<Box<dyn Inspector<BaseEvmTypes> + 'a>>,
}

impl core::fmt::Debug for YevmEvm<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("YevmEvm")
            .field("env", &self.env)
            .finish_non_exhaustive()
    }
}

impl<'a> YevmEvm<'a> {
    /// Creates an EVM over `db` in the environment `env`.
    pub fn new(db: impl DynDatabase + 'a, env: EthEvmEnv) -> Self {
        let mut cache = Cache::new();
        cache.set_journal_only(true);
        cache.set_chain_id(env.version.chain_id);
        Self {
            db: CacheDB::new(Box::new(db)),
            cache,
            originals: AddressMap::default(),
            storage: StorageOverlay::default(),
            env,
            inspector: None,
        }
    }

    /// Returns the environment the EVM executes in.
    pub const fn env(&self) -> &EthEvmEnv {
        &self.env
    }

    /// Executes a transaction and returns its result with its detached state changes.
    ///
    /// The overlay is not modified; commit the state with [`Evm::commit_state`].
    pub fn execute(
        &mut self,
        tx: &Recovered<TxEnvelope>,
    ) -> Result<TxResultWithState<BaseEvmTypes>, HandlerError> {
        let result = self.execute_in_place(tx)?;
        Ok(TxResultWithState {
            result,
            pending_state: self.pending_state()?,
            ..Default::default()
        })
    }

    /// Executes a transaction and keeps its state changes in yevm's journal, for
    /// [`Self::commit_in_place`] or [`Self::pending_state`].
    pub(crate) fn execute_in_place(
        &mut self,
        tx: &Recovered<TxEnvelope>,
    ) -> Result<TxResult<BaseEvmTypes>, HandlerError> {
        self.ensure_supported()?;
        let Validated { floor_gas, sender } = self.validate(tx)?;
        let features = self.env.version.features;
        let charge_fees = features.contains(EvmFeatures::FEE_CHARGE);
        let (call, ytx) = convert::call_and_tx(tx, self.env.version.chain_id, charge_fees);
        // Like geth, calls that pay no fees run against a zero base fee so that the fee checks
        // yevm applies unconditionally pass.
        let (base_fee, blob_base_fee) = if charge_fees {
            (self.env.block.basefee, self.env.block.blob_basefee)
        } else {
            (U256::ZERO, U256::ZERO)
        };
        let mut head = convert::head(&self.env.block, base_fee, blob_base_fee);
        if !features.contains(EvmFeatures::BLOCK_GAS_LIMIT_CHECK) {
            // yevm always rejects transactions above the block gas limit.
            head.gas_limit = head.gas_limit.max(Int::from(call.gas));
        }
        self.run(
            Executor::new(call).with_max_steps(None),
            &ytx,
            &head,
            floor_gas,
            Some(sender),
        )
    }

    /// Executes an EIP-4788 style system call from the system address to `to`.
    ///
    /// The overlay is not modified; commit the state with [`Evm::commit_state`].
    pub fn system_call(
        &mut self,
        to: Address,
        data: Bytes,
    ) -> Result<TxResultWithState<BaseEvmTypes>, HandlerError> {
        let result = self.system_call_in_place(to, data)?;
        Ok(TxResultWithState {
            result,
            pending_state: self.pending_state()?,
            ..Default::default()
        })
    }

    /// Executes a system call like [`Self::system_call`], keeping its state changes like
    /// [`Self::execute_in_place`].
    pub(crate) fn system_call_in_place(
        &mut self,
        to: Address,
        data: Bytes,
    ) -> Result<TxResult<BaseEvmTypes>, HandlerError> {
        self.ensure_supported()?;
        let call = Call {
            by: convert::acc(SYSTEM_ADDRESS),
            to: Some(convert::acc(to)),
            gas: SYSTEM_CALL_GAS_LIMIT,
            eth: Int::ZERO,
            data: Buf(data.to_vec()),
        };
        let tx = Tx {
            chain_id: self.env.version.chain_id.into(),
            nonce: Int::ZERO,
            gas_price: Int::ZERO,
            max_fee_per_gas: Int::ZERO,
            max_priority_fee_per_gas: Int::ZERO,
            access_list: Vec::new(),
            authorization_list: Vec::new(),
            blob_versioned_hashes: Vec::new(),
            max_fee_per_blob_gas: None,
            hash: Int::ZERO,
            index: Int::ZERO,
        };
        let head = convert::head(
            &self.env.block,
            self.env.block.basefee,
            self.env.block.blob_basefee,
        );
        self.run(
            Executor::system(call).with_max_steps(None),
            &tx,
            &head,
            0,
            None,
        )
    }

    fn ensure_supported(&self) -> Result<(), YevmError> {
        if self.inspector.is_some() {
            return Err(YevmError::UnsupportedInspector);
        }
        if self.env.spec != YEVM_SPEC {
            return Err(YevmError::UnsupportedSpec(self.env.spec));
        }
        Ok(())
    }

    /// Runs `executor` and returns its result as an evm2 result, keeping the state changes in
    /// yevm's journal.
    fn run(
        &mut self,
        mut executor: Executor,
        tx: &Tx,
        head: &yevm_core::Head,
        floor_gas: u64,
        sender: Option<Sender>,
    ) -> Result<TxResult<BaseEvmTypes>, HandlerError> {
        let cache = &mut self.cache;
        cache.clear();
        self.originals.clear();
        if let Some(Sender {
            address,
            info,
            top_up,
        }) = sender
        {
            let account = match top_up {
                // The credit is applied in yevm's journal, so it is part of the transaction's
                // state changes, as in evm2.
                Some(balance) => {
                    let mut credited = info.clone().unwrap_or_else(AccountInfo::empty);
                    credited.balance = balance;
                    convert::account(Some(&credited), credited.code.as_ref())
                }
                None => {
                    convert::account(info.as_ref(), info.as_ref().and_then(|i| i.code.as_ref()))
                }
            };
            cache.merge(&convert::acc(address), account);
            self.originals.insert(address, info);
        }
        let mut state = DbState::new(cache, &mut self.db, &mut self.originals);
        // `DbState` never misses, so the executor completes without suspending.
        let outcome = poll_once(executor.run(tx, head, &mut state, &NoChain))?;
        let error = state.into_error();
        if let Some(error) = error {
            return Err(HandlerError::Database(error));
        }
        let outcome = outcome.map_err(YevmError::Execution)?;

        let (status, output, created_address) = match &outcome {
            CallResult::Done { status, ret, .. } => (
                !status.is_zero(),
                Bytes::copy_from_slice(ret.as_slice()),
                None,
            ),
            CallResult::Created { acc, code, .. } => (
                true,
                Bytes::copy_from_slice(code.as_slice()),
                Some(convert::address(acc)),
            ),
        };
        let gas = outcome.gas();
        let gas_used = u64::try_from(gas.finalized).unwrap_or_default();
        let total_gas_spent = u64::try_from(gas.spent).unwrap_or_default().max(gas_used);
        let result = TxResult::<BaseEvmTypes> {
            status,
            total_gas_spent,
            refunded: total_gas_spent - gas_used,
            floor_gas,
            stop: convert::stop_reason(status, executor.outcome(), output.is_empty()),
            output,
            created_address,
            logs: convert::logs(&self.cache.logs),
            ..Default::default()
        };
        Ok(result)
    }

    /// Moves the state changes of the last execution out of yevm's journal into a pending state.
    fn pending_state(&mut self) -> Result<PendingState, DatabaseError> {
        let mut pending_state = PendingState::default();
        for (acc, account, storage) in self.cache.entries() {
            let address = convert::address(acc);
            let (original, current) =
                account_change(&mut self.originals, &mut self.db, &address, account)?;
            let has_current = current.is_some();
            pending_state.insert_account(address, original, current);
            if has_current {
                for (key, slot) in storage {
                    pending_state.insert_storage(
                        address,
                        convert::u256(key),
                        convert::u256(&slot.original),
                        convert::u256(&slot.current),
                    );
                }
            }
        }
        Ok(pending_state)
    }

    /// Commits the state changes of the last execution from yevm's journal to the overlay,
    /// passing each changed account to `commit` first.
    ///
    /// Like committing [`Self::pending_state`] with [`Evm::commit_state`], without building it,
    /// and reporting created accounts as created, which a pending state cannot express.
    pub(crate) fn commit_in_place(
        &mut self,
        mut commit: impl FnMut(AccountChanges<'_>),
    ) -> Result<(), DatabaseError> {
        // Accounts the transaction created; one it also destroyed (EIP-6780) is reported as
        // unchanged, since it does not exist before or after the transaction.
        let created = self.cache.created();
        for (acc, account, slots) in self.cache.entries() {
            let address = convert::address(acc);
            let (original, current) =
                account_change(&mut self.originals, &mut self.db, &address, account)?;
            // Unchanged slots are only reads, which neither the overlay nor the block state keep.
            self.storage.slots.clear();
            if current.is_some() {
                self.storage.slots.extend(
                    slots
                        .iter()
                        .filter(|(_, slot)| slot.original != slot.current)
                        .map(|(key, slot)| {
                            let value = Tracked::from_parts(
                                convert::u256(&slot.original),
                                convert::u256(&slot.current),
                            );
                            (
                                convert::u256(key),
                                StorageSlot {
                                    value,
                                    ..Default::default()
                                },
                            )
                        }),
                );
            }
            let changes = AccountChanges {
                address,
                original: original.as_ref(),
                current: current.as_ref(),
                created: created.contains(acc),
                selfdestructed: false,
                touched: false,
                code: changed_code(original.as_ref(), current.as_ref()),
                storage: &self.storage,
            };
            if !changes.is_changed() {
                continue;
            }
            commit(changes);
            let Ok(()) = changes.visit(&mut self.db);
        }
        Ok(())
    }

    /// Checks the transaction against the environment and the sender's account, like evm2's
    /// Ethereum handlers.
    ///
    /// yevm skips most of these checks (it targets simulation), so they are enforced here.
    fn validate(&mut self, tx: &Recovered<TxEnvelope>) -> Result<Validated, HandlerError> {
        let version = &self.env.version;
        let block = &self.env.block;
        let features = version.features;
        let caller = tx.signer();
        let gas_limit = tx.gas_limit();

        if features.contains(EvmFeatures::TX_CHAIN_ID_CHECK)
            && let Some(got) = tx.chain_id()
            && got != version.chain_id
        {
            return Err(HandlerError::InvalidChainId {
                expected: version.chain_id,
                got,
            });
        }
        if gas_limit > version.tx_gas_limit_cap {
            return Err(HandlerError::TxGasLimitGreaterThanCap {
                gas_limit,
                cap: version.tx_gas_limit_cap,
            });
        }
        if features.contains(EvmFeatures::BLOCK_GAS_LIMIT_CHECK)
            && U256::from(gas_limit) > block.gas_limit
        {
            return Err(HandlerError::GasLimitMoreThanBlock {
                gas_limit,
                block_gas_limit: block.gas_limit,
            });
        }
        let max_fee_per_gas = U256::from(tx.max_fee_per_gas());
        if features.contains(EvmFeatures::BASE_FEE_CHECK) && max_fee_per_gas < block.basefee {
            return Err(HandlerError::FeeCapLessThanBaseFee {
                max_fee_per_gas,
                base_fee: block.basefee,
            });
        }
        if features.contains(EvmFeatures::PRIORITY_FEE_CHECK)
            && tx
                .max_priority_fee_per_gas()
                .is_some_and(|priority| priority > tx.max_fee_per_gas())
        {
            return Err(HandlerError::PriorityFeeGreaterThanMaxFee);
        }
        let blob_gas_cost = if let Some(hashes) = tx.blob_versioned_hashes() {
            if hashes.is_empty() {
                return Err(HandlerError::EmptyBlobs);
            }
            if hashes.len() > version.max_blobs_per_tx {
                return Err(HandlerError::TooManyBlobs {
                    have: hashes.len(),
                    max: version.max_blobs_per_tx,
                });
            }
            if hashes
                .iter()
                .any(|hash| hash[0] != VERSIONED_HASH_VERSION_KZG)
            {
                return Err(HandlerError::BlobVersionNotSupported);
            }
            let max_fee_per_blob_gas = U256::from(tx.max_fee_per_blob_gas().unwrap_or_default());
            if features.contains(EvmFeatures::BLOB_BASE_FEE_CHECK)
                && max_fee_per_blob_gas < block.blob_basefee
            {
                return Err(HandlerError::BlobFeeCapLessThanBlobBaseFee {
                    max_fee_per_blob_gas,
                    blob_base_fee: block.blob_basefee,
                });
            }
            U256::from(tx.blob_gas_used().unwrap_or_default()).saturating_mul(max_fee_per_blob_gas)
        } else {
            U256::ZERO
        };
        if tx.is_eip7702() && tx.authorization_list().is_none_or(<[_]>::is_empty) {
            return Err(HandlerError::EmptyAuthorizationList);
        }
        if tx.kind().is_create() && tx.input().len() > version.max_initcode_size {
            return Err(HandlerError::CreateInitCodeSizeLimit {
                limit: version.max_initcode_size,
                got: tx.input().len(),
            });
        }

        let (access_list_accounts, access_list_storage_keys) =
            tx.access_list().map_or((0, 0), |list| {
                (
                    list.len() as u64,
                    list.iter().map(|item| item.storage_keys.len() as u64).sum(),
                )
            });
        let gas = self.env.transaction_validation_gas_rules().calculate(
            caller,
            tx.kind(),
            tx.value(),
            tx.input(),
            access_list_accounts,
            access_list_storage_keys,
            tx.authorization_list().map_or(0, |list| list.len() as u64),
        );
        if gas_limit < gas.intrinsic_gas {
            return Err(HandlerError::IntrinsicGasTooLow {
                required: gas.intrinsic_gas,
                got: gas_limit,
            });
        }
        if gas_limit < gas.floor_gas {
            return Err(HandlerError::GasLimitBelowFloor {
                required: gas.floor_gas,
                got: gas_limit,
            });
        }

        let info = load_account(&mut self.db, &caller)?;
        let sender = info.clone().unwrap_or_else(AccountInfo::empty);
        if features.contains(EvmFeatures::NONCE_CHECK) && tx.nonce() != sender.nonce {
            return Err(HandlerError::InvalidNonce {
                expected: sender.nonce,
                got: tx.nonce(),
            });
        }
        if sender.nonce == u64::MAX {
            return Err(HandlerError::NonceOverflow);
        }
        if features.contains(EvmFeatures::EIP3607)
            && sender.code.as_ref().is_some_and(|code| !code.is_eip7702())
        {
            return Err(HandlerError::RejectCallerWithCode);
        }
        let max_upfront = U256::from(gas_limit)
            .saturating_mul(max_fee_per_gas)
            .saturating_add(tx.value())
            .saturating_add(blob_gas_cost);
        let mut top_up = None;
        if sender.balance < max_upfront {
            if features.contains(EvmFeatures::BALANCE_CHECK) {
                return Err(HandlerError::InsufficientFunds {
                    fee: max_upfront,
                    balance: sender.balance,
                });
            }
            if features.contains(EvmFeatures::BALANCE_TOP_UP) {
                top_up = Some(max_upfront);
            }
        }
        Ok(Validated {
            floor_gas: gas.floor_gas,
            sender: Sender {
                address: caller,
                info,
                top_up,
            },
        })
    }
}

/// Returns an account's info before and after the last execution, taking the original from
/// `originals`, where the execution recorded the accounts it loaded.
fn account_change(
    originals: &mut AddressMap<Option<AccountInfo>>,
    db: &mut CacheDB<Box<dyn DynDatabase + '_>>,
    address: &Address,
    account: &Account,
) -> Result<(Option<AccountInfo>, Option<AccountInfo>), DatabaseError> {
    let original = match originals.remove(address) {
        Some(original) => original,
        None => db.get_account(address)?,
    };
    let current = convert::account_info(account, original.as_ref());
    Ok((original, current))
}

/// Returns `current`'s code keyed by its hash if the code changed to non-empty code, as evm2
/// reports changed code.
fn changed_code<'a>(
    original: Option<&AccountInfo>,
    current: Option<&'a AccountInfo>,
) -> Option<(B256, &'a Bytecode)> {
    let current = current?;
    let code = current.code.as_ref()?;
    (original.map(|original| original.code_hash) != Some(current.code_hash) && !code.is_empty())
        .then_some((current.code_hash, code))
}

impl<'a> Evm for YevmEvm<'a> {
    type EvmTypes = BaseEvmTypes;
    type Transaction = TxEnvelope;

    fn transact(
        &mut self,
        transaction: &Recovered<Self::Transaction>,
    ) -> Result<TxResultWithState<BaseEvmTypes>, BlockExecutionError> {
        self.execute(transaction).map_err(map_handler_error)
    }

    fn transact_commit(
        &mut self,
        transaction: &Recovered<Self::Transaction>,
    ) -> Result<TxResult<BaseEvmTypes>, BlockExecutionError> {
        let result = self
            .execute_in_place(transaction)
            .map_err(map_handler_error)?;
        self.commit_in_place(|_| {}).map_err(map_database_error)?;
        Ok(result)
    }

    fn transact_result(
        &mut self,
        transaction: &Recovered<Self::Transaction>,
    ) -> Result<TxResult<BaseEvmTypes>, BlockExecutionError> {
        self.execute_in_place(transaction)
            .map_err(map_handler_error)
    }

    fn transact_with_inspector<I>(
        &mut self,
        _transaction: &Recovered<Self::Transaction>,
        _inspector: I,
    ) -> Result<(I, TxResultWithState<BaseEvmTypes>), BlockExecutionError>
    where
        I: Inspector<BaseEvmTypes> + 'static,
    {
        Err(BlockExecutionError::other(YevmError::UnsupportedInspector))
    }

    fn set_inspector<I>(&mut self, inspector: I)
    where
        I: Inspector<BaseEvmTypes> + 'static,
    {
        self.inspector = Some(Box::new(inspector));
    }

    unsafe fn with_inspector<I, R>(&mut self, f: impl FnOnce(&mut I) -> R) -> Option<R>
    where
        I: Inspector<BaseEvmTypes> + 'static,
    {
        let inspector: &mut (dyn evm2::evm::NonStaticAny + 'a) = &mut **self.inspector.as_mut()?;
        if evm2::evm::NonStaticAny::type_id(inspector) != core::any::TypeId::of::<I>() {
            return None;
        }
        // SAFETY: the type ID check shows that the inspector is an `I`, and the caller guarantees
        // that `I` has no lifetime parameters, so no lifetime is erased.
        Some(f(unsafe { inspector.downcast_mut_unchecked::<I>() }))
    }

    fn take_inspector(&mut self) -> Option<Box<dyn Inspector<BaseEvmTypes> + '_>> {
        self.inspector
            .take()
            .map(|inspector| inspector as Box<dyn Inspector<BaseEvmTypes> + '_>)
    }

    fn state_db_mut(&mut self) -> &mut dyn DynDatabase {
        &mut self.db
    }

    fn commit_state(&mut self, state: &PendingState) {
        self.db.commit_pending(state);
    }

    fn take_state_cache(&mut self) -> evm2::evm::Cache {
        core::mem::take(&mut self.db.cache)
    }

    fn precompile_ids(&self) -> Vec<(Address, PrecompileId)> {
        Precompiles::<BaseEvmTypes>::base(self.env.spec).precompile_ids()
    }

    fn has_precompile(&self, address: &Address) -> bool {
        is_precompile(&convert::acc(*address))
    }

    fn account_info(
        &mut self,
        address: &Address,
    ) -> Result<Option<AccountInfo>, BlockExecutionError> {
        self.db.get_account(address).map_err(map_database_error)
    }

    fn move_precompiles(
        &mut self,
        moves: impl IntoIterator<Item = (Address, Address)>,
    ) -> Result<(), MovePrecompileError> {
        // yevm dispatches precompiles by fixed address, so none can be moved.
        match moves.into_iter().next() {
            Some((source, _)) => Err(MovePrecompileError::NotAPrecompile(source)),
            None => Ok(()),
        }
    }

    /// Executes the transaction, then fails with [`YevmError::UnsupportedExecutedTx`], since yevm
    /// cannot produce the evm2 view `observe` takes.
    ///
    /// Engine prewarming, the only caller, executes transactions ahead of the block to load their
    /// state into the shared state cache; the execution here still does that. Prewarming then
    /// counts the error and sends no state root hints for the transaction.
    fn transact_and_discard<R>(
        &mut self,
        transaction: &Recovered<Self::Transaction>,
        _observe: impl FnOnce(&ExecutedTx<'_, '_, BaseEvmTypes>) -> R,
    ) -> Result<R, BlockExecutionError> {
        // Only the loaded state matters. The writes stay in yevm's journal, which the next
        // execution clears.
        let _ = self
            .execute_in_place(transaction)
            .map_err(map_handler_error)?;
        Err(BlockExecutionError::other(YevmError::UnsupportedExecutedTx))
    }
}
