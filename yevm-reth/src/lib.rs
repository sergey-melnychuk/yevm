//! `Chain` implementation that reads state and blocks directly from a local
//! reth MDBX datadir instead of JSON-RPC.
//!
//! Scope: latest state by default (`factory.latest()`, via `head`/`block`),
//! plus any earlier block still within the node's retained account/storage
//! history via [`RethDb::pin`] (`factory.history_by_block_number()` --
//! see the `account_history`/`storage_history` prune distance in
//! `reth.toml`). No reorg handling, and nothing older than that retained
//! window -- `pin` fails naturally past it rather than guessing.
//!
//! API verified against reth's own `examples/db-access` and
//! `examples/full-contract-state` at tag `v2.5.1`
//! (<https://github.com/paradigmxyz/reth/tree/v2.5.1/examples>); compiles
//! clean against that tag.
//!
//! Deliberately **not** using `tokio::task::spawn_blocking` around these
//! MDBX reads: they're memory-mapped point lookups against a page cache
//! reth's own live node keeps warm, microsecond-scale -- likely cheaper
//! than the thread-hop + wakeup `spawn_blocking` itself costs. The calling
//! pattern (`yevm-core`'s interpreter `.await`s each fetch before it can
//! execute the next opcode) is inherently sequential, so there's no
//! concurrent async work on the same thread to protect from blocking. The
//! one real risk is a cold-page fault stalling the calling task's worker
//! thread -- rare in steady state, and nothing else runnable is waiting
//! on that thread at that moment anyway.

use std::path::PathBuf;
use std::sync::Mutex;

use alloy_consensus::Transaction as _;
use alloy_primitives::{Address, B256, U256};
use eyre::Context;
use reth_db::DatabaseEnv;
use reth_ethereum::{
    TransactionSigned,
    chainspec::ChainSpecBuilder,
    node::EthereumNode,
    provider::{
        AccountReader, BlockNumReader, BlockReader, ChainSpecProvider, HeaderProvider,
        StateProvider, StateProviderBox, TransactionVariant,
        providers::{DatabaseProviderRO, ProviderFactory, ReadOnlyConfig},
    },
    tasks::{RayonConfig, Runtime, RuntimeBuilder, RuntimeConfig, TokioConfig},
};
use reth_node_types::NodeTypesWithDBAdapter;

use yevm_base::{Acc, Int};
use yevm_core::{
    call::{AccessListItem, AuthorizationListItem, Block, Head, Tx, TxCall, TxFull},
    chain::Chain,
    state::Account,
};
use yevm_misc::buf::Buf;

pub type Factory = ProviderFactory<NodeTypesWithDBAdapter<EthereumNode, DatabaseEnv>>;
pub type Provider = DatabaseProviderRO<DatabaseEnv, NodeTypesWithDBAdapter<EthereumNode, DatabaseEnv>>;

/// `open_read_only()` requires a `reth_tasks::Runtime`, and `Runtime::test()`
/// -- reth's own "lightweight" constructor -- still unconditionally builds
/// *eight* separate rayon thread pools (`cpu`, `rpc`, `storage`, two proof
/// worker pools, prewarming, BAL streaming, state-trie-overlay), 2 threads
/// each, sized for full-node workloads (parallel trie computation, block
/// prewarming, proof generation) that a read-only point-lookup `Chain`
/// never touches. `RuntimeBuilder::build()` has no way to skip a pool
/// entirely -- every field in `RayonConfig` picks its thread count, not
/// whether it exists -- so the best available fix is sizing every pool
/// down to 1 thread instead of `test()`'s 2. Confirmed via `strace -c` on
/// `RethDb::latest()`: >90% of traced time was `futex` (thread-pool
/// readiness barriers) and `clone3` (thread creation), not any disk I/O
/// against the datadir itself.
fn minimal_runtime() -> eyre::Result<Runtime> {
    let tokio = match tokio::runtime::Handle::try_current() {
        Ok(handle) => TokioConfig::existing_handle(handle),
        Err(_) => TokioConfig::default(),
    };
    let config = RuntimeConfig::default()
        .with_tokio(tokio)
        .with_rayon(RayonConfig {
            cpu_threads: Some(1),
            reserved_cpu_cores: 0,
            rpc_threads: Some(1),
            storage_threads: Some(1),
            max_blocking_tasks: 16,
            proof_storage_worker_threads: Some(1),
            proof_account_worker_threads: Some(1),
            prewarming_threads: Some(1),
            bal_streaming_threads: Some(1),
            state_trie_overlay_worker_threads: Some(1),
        });
    Ok(RuntimeBuilder::new(config).build()?)
}

pub struct RethDb {
    factory: Factory,
    /// Pinned MDBX read snapshot -- does *not* auto-advance as the node
    /// keeps writing. Call [`RethDb::refresh`] between blocks.
    state: Mutex<StateProviderBox>,
    /// Opened once and reused for `best_block_number`/`head`/`block`.
    /// `factory.provider()` opens a fresh MDBX read transaction *and* calls
    /// reth's internal `sync_providers_if_needed()` (re-syncing against the
    /// live node's static-file/checkpoint state) on every call -- real,
    /// measurable cost if paid once per call instead of once per process.
    /// Re-pinned in [`RethDb::refresh`] alongside `state`.
    provider: Mutex<Provider>,
}

impl RethDb {
    /// Opens `datadir` read-only alongside the running node and pins state
    /// at the current tip. Synchronous like `refresh()` -- see module docs
    /// on why these don't go through `spawn_blocking`.
    pub fn latest(datadir: impl Into<PathBuf>) -> eyre::Result<Self> {
        let datadir = datadir.into();

        let spec = ChainSpecBuilder::mainnet().build();
        let runtime = minimal_runtime()?;
        // Default `ReadOnlyConfig` is the *monitored* variant: it tracks the
        // live node's writes (static files, checkpoints) instead of
        // freezing a view from process start. This is the mode reth's own
        // docs recommend when a node is actively running alongside.
        let factory = EthereumNode::provider_factory_builder().open_read_only(
            spec.into(),
            ReadOnlyConfig::from_datadir(datadir),
            runtime,
        )?;
        let state = factory.latest().context("reth: factory.latest()")?;
        let provider = factory.provider().context("reth: provider")?;

        Ok(Self {
            factory,
            state: Mutex::new(state),
            provider: Mutex::new(provider),
        })
    }

    /// Re-opens the reused provider handle. A `DatabaseProviderRO` is a
    /// single MDBX read transaction -- an MVCC snapshot frozen at the
    /// moment it was opened, "monitored" `ReadOnlyConfig` or not (that
    /// mode keeps the *factory* ready to hand out correct fresh
    /// transactions on request; it does not make an already-open one see
    /// later writes). Call this before `best_block_number()` whenever you
    /// need it to reflect blocks persisted since the last call -- e.g.
    /// once per poll tick when watching for the tip to move.
    pub fn refresh_provider(&self) -> eyre::Result<()> {
        let provider = self.factory.provider().context("reth: provider")?;
        *self.provider.lock().unwrap() = provider;
        Ok(())
    }

    /// Pins state to right before block `target` executes -- i.e. the
    /// state resulting from block `target - 1`. Uses `factory.latest()`
    /// when `target - 1` is the current persisted tip (the common case);
    /// otherwise falls back to `factory.history_by_block_number()`, which
    /// reconstructs state at any block still within the node's retained
    /// account/storage history (`account_history`/`storage_history` prune
    /// distance in `reth.toml` -- 10064 blocks / ~1.4 days by default on a
    /// `--minimal` node). Errors if `target - 1` is older than that.
    ///
    /// This does *not* mean any block within history is safe to replay --
    /// `head`/`block` still read whatever's persisted regardless of this
    /// pin, so the caller is responsible for only requesting a `target`
    /// whose block data actually exists (`target <= best_block_number() +
    /// 1`, after this call -- it refreshes the provider first, so the tip
    /// it checks against is never stale from an earlier call).
    pub fn pin(&self, target: u64) -> eyre::Result<()> {
        self.refresh_provider()?;
        let tip = self.best_block_number()?;
        let state = if target == 0 || target - 1 == tip {
            self.factory.latest().context("reth: factory.latest()")?
        } else {
            self.factory
                .history_by_block_number(target - 1)
                .context("reth: history_by_block_number")?
        };
        *self.state.lock().unwrap() = state;
        Ok(())
    }

    /// The highest block number actually *persisted* to this datadir as of
    /// the last `latest()`/`refresh_provider()` call -- can lag the number
    /// an RPC's `eth_blockNumber`/`"latest"` reports by a few blocks even
    /// when fresh (a live-following node holds recent blocks in an
    /// in-memory canonical chain before flushing them to MDBX/static
    /// files, and RPC serves from that in-memory view), and can lag
    /// further still if `self.provider` itself is stale -- call
    /// `refresh_provider()` first if this needs to reflect blocks
    /// persisted since construction/the last refresh.
    pub fn best_block_number(&self) -> eyre::Result<u64> {
        Ok(self.provider.lock().unwrap().best_block_number()?)
    }

    fn addr(acc: &Acc) -> Address {
        Address::from_slice(acc.as_ref())
    }

    fn acc(addr: Address) -> Acc {
        Acc::from(addr.as_slice())
    }

    fn key(int: &Int) -> B256 {
        B256::from_slice(int.as_ref())
    }

    fn int(u: U256) -> Int {
        Int::from(u.to_be_bytes::<32>().as_slice())
    }

    fn hash(b: B256) -> Int {
        Int::from(b.as_slice())
    }

    /// `BlockHeader` is implemented uniformly for `alloy_consensus::Header`
    /// regardless of wrapper (`SealedHeader`, plain `Header`, ...), so this
    /// works for both `head()` (header-only) and `block()` (via the header
    /// of a `RecoveredBlock`).
    fn head_of(header: &impl alloy_consensus::BlockHeader, hash: B256) -> Head {
        Head {
            number: header.number().into(),
            hash: Self::hash(hash),
            gas_limit: Int::from(header.gas_limit()),
            coinbase: Self::acc(header.beneficiary()),
            timestamp: Int::from(header.timestamp()),
            base_fee: Int::from(header.base_fee_per_gas().unwrap_or_default()),
            excess_blob_gas: header.excess_blob_gas().map(Int::from),
            blobhash: None,
            prevrandao: Self::hash(header.mix_hash().unwrap_or_default()),
            parent_hash: Self::hash(header.parent_hash()),
            parent_beacon_block_root: header.parent_beacon_block_root().map(Self::hash),
        }
    }

    /// Maps one signed transaction + its recovered sender into yevm's
    /// `TxFull`. `alloy_consensus::Transaction` is implemented uniformly
    /// across tx types (legacy/2930/1559/4844/7702), so no per-variant
    /// matching is needed -- except for the legacy/2930 vs. dynamic-fee
    /// split below, which yevm itself keys off `max_fee_per_gas == 0`
    /// (see `exe::intrinsic`), so it must be preserved exactly: a real
    /// legacy tx's `Transaction::max_fee_per_gas()` returns its
    /// `gas_price` (nonzero), not zero, so that trait method alone can't
    /// be used to fill yevm's field directly.
    fn tx_full(tx: &TransactionSigned, sender: Address, index: u64) -> TxFull {
        let (gas_price, max_fee_per_gas, max_priority_fee_per_gas) = match tx.gas_price() {
            // Legacy / EIP-2930: no dynamic fee fields.
            Some(price) => (Int::from(price), Int::ZERO, Int::ZERO),
            // EIP-1559/4844/7702: `gas_price` is unused by yevm once
            // `max_fee_per_gas` is nonzero (see `exe::intrinsic`).
            None => (
                Int::ZERO,
                Int::from(tx.max_fee_per_gas()),
                Int::from(tx.max_priority_fee_per_gas().unwrap_or_default()),
            ),
        };

        let access_list = tx
            .access_list()
            .map(|list| {
                list.iter()
                    .map(|item| AccessListItem {
                        address: Self::acc(item.address),
                        storage_keys: item.storage_keys.iter().map(|k| Self::hash(*k)).collect(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let authorization_list = tx
            .authorization_list()
            .map(|list| {
                list.iter()
                    .map(|auth| {
                        // Stored, already-included authorizations are valid by
                        // construction -- the signature was checked at block
                        // execution time.
                        let sig = auth.signature().expect("valid stored authorization");
                        AuthorizationListItem {
                            address: Self::acc(auth.address),
                            chain_id: Self::int(auth.chain_id),
                            nonce: Int::from(auth.nonce),
                            r: Self::int(sig.r()),
                            s: Self::int(sig.s()),
                            y_parity: Int::from(sig.v() as u64),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        let blob_versioned_hashes = tx
            .blob_versioned_hashes()
            .map(|hashes| hashes.iter().map(|h| Self::hash(*h)).collect())
            .unwrap_or_default();

        TxFull {
            call: TxCall {
                to: tx.to().map(Self::acc),
                from: Self::acc(sender),
                input: Buf(tx.input().to_vec()),
                value: Self::int(tx.value()),
                gas: Int::from(tx.gas_limit()),
            },
            tx: Tx {
                chain_id: tx.chain_id().unwrap_or_default().into(),
                nonce: Int::from(tx.nonce()),
                gas_price,
                max_fee_per_gas,
                max_priority_fee_per_gas,
                access_list,
                authorization_list,
                blob_versioned_hashes,
                max_fee_per_blob_gas: tx.max_fee_per_blob_gas().map(Int::from),
                hash: Self::hash(*tx.tx_hash()),
                index: Int::from(index),
            },
        }
    }

    fn withdrawal(w: &alloy_eips::eip4895::Withdrawal) -> yevm_core::call::Withdrawal {
        yevm_core::call::Withdrawal {
            index: Int::from(w.index),
            validator_index: Int::from(w.validator_index),
            address: Self::acc(w.address),
            amount: Int::from(w.amount),
        }
    }

    pub fn factory(&self) -> &Factory {
        &self.factory
    }
}

#[async_trait::async_trait]
impl Chain for RethDb {
    async fn get(&self, acc: &Acc, key: &Int) -> eyre::Result<Int> {
        let (addr, slot) = (Self::addr(acc), Self::key(key));
        let val = self
            .state
            .lock()
            .unwrap()
            .storage(addr, slot)
            .context("reth: storage")?;
        Ok(Self::int(val.unwrap_or_default()))
    }

    async fn acc(&self, acc: &Acc) -> eyre::Result<Account> {
        let addr = Self::addr(acc);
        let state = self.state.lock().unwrap();
        // One point lookup gets nonce + balance + the code hash together.
        // `StateProvider::account_code()`'s default impl would look this
        // account up *again* internally before fetching the bytecode, and
        // re-hash the full bytecode with keccak256 even though the trie
        // already handed us the correct hash -- go straight to
        // `bytecode_by_hash` with the hash we already have instead.
        let account = state.basic_account(&addr).context("reth: basic_account")?;
        let code_hash = account.as_ref().and_then(|a| a.bytecode_hash);
        let (code, hash) =
            match code_hash.filter(|h| *h != alloy_consensus::constants::KECCAK_EMPTY) {
                Some(code_hash) => {
                    let code = state
                        .bytecode_by_hash(&code_hash)
                        .context("reth: bytecode_by_hash")?;
                    let bytes = code
                        .map(|c| c.original_byte_slice().to_vec())
                        .unwrap_or_default();
                    (Buf(bytes), Self::hash(code_hash))
                }
                None => (Buf::default(), Int::ZERO),
            };
        Ok(Account {
            value: account
                .as_ref()
                .map(|a| Self::int(a.balance))
                .unwrap_or_default(),
            nonce: account
                .as_ref()
                .map(|a| Int::from(a.nonce))
                .unwrap_or_default(),
            code: (code, hash),
        })
    }

    async fn code(&self, acc: &Acc) -> eyre::Result<(Buf, Int)> {
        Ok(self.acc(acc).await?.code)
    }

    async fn nonce(&self, acc: &Acc) -> eyre::Result<u64> {
        let addr = Self::addr(acc);
        let account = self
            .state
            .lock()
            .unwrap()
            .basic_account(&addr)
            .context("reth: basic_account")?;
        Ok(account.map(|a| a.nonce).unwrap_or_default())
    }

    async fn balance(&self, acc: &Acc) -> eyre::Result<Int> {
        let addr = Self::addr(acc);
        let account = self
            .state
            .lock()
            .unwrap()
            .basic_account(&addr)
            .context("reth: basic_account")?;
        Ok(account.map(|a| Self::int(a.balance)).unwrap_or_default())
    }

    async fn head(&self, number: u64) -> eyre::Result<Head> {
        let provider = self.provider.lock().unwrap();
        let sealed = provider
            .sealed_header(number)
            .context("reth: sealed_header")?
            .ok_or_else(|| eyre::eyre!("reth: header {number} not found"))?;
        Ok(Self::head_of(sealed.header(), sealed.hash()))
    }

    async fn block(&self, number: u64) -> eyre::Result<Block> {
        let provider = self.provider.lock().unwrap();
        let recovered = provider
            .sealed_block_with_senders(number.into(), TransactionVariant::WithHash)
            .context("reth: sealed_block_with_senders")?
            .ok_or_else(|| eyre::eyre!("reth: block {number} not found"))?;

        let head = Self::head_of(recovered.header(), recovered.hash());
        let txs = recovered
            .transactions_with_sender()
            .enumerate()
            .map(|(index, (sender, tx))| Self::tx_full(tx, *sender, index as u64))
            .collect();
        let withdrawals = recovered
            .body()
            .withdrawals
            .iter()
            .flat_map(|list| list.iter())
            .map(Self::withdrawal)
            .collect();

        Ok(Block {
            head,
            txs,
            withdrawals,
        })
    }

    async fn chain_id(&self) -> eyre::Result<u64> {
        Ok(self.factory.chain_spec().chain().id())
    }
}
