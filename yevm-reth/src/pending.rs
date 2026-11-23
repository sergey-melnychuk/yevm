//! Pending read touches (silt PLAN D14): what the pool's pending transactions read on the head.
//!
//! One worker thread, off the import path: on each new head it opens that block's state once,
//! runs the pool's best transactions on it with yevm (each on a fresh overlay, in the next
//! block's environment), and records every account and slot they read together with its value
//! there. The pass goes to silt's [`ReadTouchSink`]; the next diff publishes the keys that block
//! didn't write, so consumers hold them before they simulate those transactions.
//!
//! The thread runs at nice 10 with the idle I/O class (its reads are mostly first touches, page
//! faults on the node's disk, which must not queue ahead of block import), and a pass stops after
//! [`MAX_TXS_PER_PASS`] transactions or [`PASS_BUDGET`], whichever comes first. The budget is
//! enforced inside the reads too: once it's spent, the transaction running is abandoned at its next
//! read (an idle-class read can wait seconds behind import I/O; one did, for 21 s).

use std::collections::HashMap;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use alloy_primitives::B256;
use reth_ethereum::pool::{PoolTransaction, TransactionPool};
use reth_ethereum::provider::{HeaderProvider, StateProviderFactory};
use reth_ethereum_primitives::TransactionSigned;
use silt_node::reads::{ReadTouchSink, ReadTouches, ReadValues};
use silt_wire::{AccountChange, Addr, KECCAK_EMPTY, Word};
use yevm_base::{Acc, Int};
use yevm_core::{
    call::{Block, Head},
    chain::Chain,
    exe::Executor,
    state::{Account, State},
};
use yevm_misc::buf::Buf;

use crate::{StateChain, yevm_tx};

/// Most pool transactions run per head.
pub const MAX_TXS_PER_PASS: usize = 2_000;

/// Most time one pass spends on one head.
pub const PASS_BUDGET: Duration = Duration::from_millis(500);

/// A new canonical head for the worker: its number, hash, and the environment of the block
/// that would follow it (pool transactions run as if in that block).
#[derive(Clone, Debug)]
pub struct NewHead {
    pub number: u64,
    pub hash: B256,
    pub next: Head,
}

/// A `Chain` that records every account and slot read through it, with the value it returned,
/// and refuses reads after `deadline` (the pass budget).
struct Recording<C> {
    inner: C,
    deadline: Instant,
    accounts: Mutex<HashMap<Addr, AccountChange>>,
    slots: Mutex<HashMap<(Addr, Word), Word>>,
}

fn addr(acc: &Acc) -> Addr {
    let mut out = [0u8; 20];
    out.copy_from_slice(acc.as_ref());
    out
}

fn word(int: &Int) -> Word {
    let mut out = [0u8; 32];
    out.copy_from_slice(int.as_ref());
    out
}

impl<C> Recording<C> {
    fn new(inner: C, deadline: Instant) -> Self {
        Self {
            inner,
            deadline,
            accounts: Mutex::default(),
            slots: Mutex::default(),
        }
    }

    fn check(&self) -> eyre::Result<()> {
        eyre::ensure!(Instant::now() < self.deadline, "pending pass budget spent");
        Ok(())
    }

    fn into_values(self) -> ReadValues {
        ReadValues {
            accounts: self
                .accounts
                .into_inner()
                .expect("accounts lock")
                .into_iter()
                .collect(),
            slots: self
                .slots
                .into_inner()
                .expect("slots lock")
                .into_iter()
                .map(|((a, s), v)| (a, s, v))
                .collect(),
        }
    }
}

#[async_trait::async_trait]
impl<C: Chain + Send + Sync> Chain for Recording<C> {
    async fn get(&self, acc: &Acc, key: &Int) -> eyre::Result<Int> {
        self.check()?;
        let value = self.inner.get(acc, key).await?;
        self.slots
            .lock()
            .expect("slots lock")
            .insert((addr(acc), word(key)), word(&value));
        Ok(value)
    }

    async fn acc(&self, acc: &Acc) -> eyre::Result<Account> {
        self.check()?;
        let account = self.inner.acc(acc).await?;
        let code_hash = word(&account.code.1);
        self.accounts.lock().expect("accounts lock").insert(
            addr(acc),
            AccountChange {
                balance: word(&account.value),
                nonce: account.nonce.as_u64(),
                // `RethDb`/`StateChain` report no code as a zero hash; silt uses keccak("").
                code_hash: if code_hash == [0; 32] {
                    KECCAK_EMPTY
                } else {
                    code_hash
                },
            },
        );
        Ok(account)
    }

    async fn code(&self, acc: &Acc) -> eyre::Result<(Buf, Int)> {
        Ok(self.acc(acc).await?.code)
    }

    async fn nonce(&self, acc: &Acc) -> eyre::Result<u64> {
        Ok(self.acc(acc).await?.nonce.as_u64())
    }

    async fn balance(&self, acc: &Acc) -> eyre::Result<Int> {
        Ok(self.acc(acc).await?.value)
    }

    async fn head(&self, number: u64) -> eyre::Result<Head> {
        self.inner.head(number).await
    }

    async fn block(&self, number: u64) -> eyre::Result<Block> {
        self.inner.block(number).await
    }

    async fn chain_id(&self) -> eyre::Result<u64> {
        self.inner.chain_id().await
    }
}

/// Lowers the calling thread's priority: CPU nice 10, idle I/O class (Linux: per thread).
/// Elsewhere (a development machine) the worker keeps the default priority.
#[cfg(not(target_os = "linux"))]
fn lower_priority() {}

/// Lowers the calling thread's priority: CPU nice 10, idle I/O class (Linux: per thread).
#[cfg(target_os = "linux")]
fn lower_priority() {
    // SAFETY: plain syscalls on the calling thread's own id.
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, 10);
        const IOPRIO_WHO_PROCESS: libc::c_long = 1;
        const IOPRIO_CLASS_IDLE: libc::c_long = 3;
        libc::syscall(
            libc::SYS_ioprio_set,
            IOPRIO_WHO_PROCESS,
            tid as libc::c_long,
            IOPRIO_CLASS_IDLE << 13,
        );
    }
}

/// The worker loop (see the module docs): blocks on `heads`, always taking the newest one
/// waiting, until the sender is dropped.
pub fn run_worker<P, Pool>(
    heads: Receiver<NewHead>,
    provider: P,
    pool: Pool,
    sink: Arc<ReadTouchSink>,
    chain_id: u64,
) where
    P: StateProviderFactory + HeaderProvider + Clone + Send + Sync + 'static,
    Pool: TransactionPool + 'static,
    Pool::Transaction: PoolTransaction<Consensus = TransactionSigned>,
{
    lower_priority();
    let rt = match tokio::runtime::Builder::new_current_thread().build() {
        Ok(rt) => rt,
        Err(err) => {
            tracing::warn!(target: "yevm::pending", %err, "no runtime: pending read touches off");
            return;
        }
    };
    while let Ok(mut head) = heads.recv() {
        while let Ok(newer) = heads.try_recv() {
            head = newer;
        }
        let started = Instant::now();
        let state = match provider.state_by_block_hash(head.hash) {
            Ok(state) => state,
            Err(err) => {
                tracing::debug!(target: "yevm::pending", number = head.number, %err, "no state for head");
                continue;
            }
        };
        let chain = Recording::new(
            StateChain::new(state, provider.clone(), chain_id),
            started + PASS_BUDGET,
        );
        let mut txs = 0u64;
        for pooled in pool.best_transactions().take(MAX_TXS_PER_PASS) {
            if started.elapsed() >= PASS_BUDGET {
                break;
            }
            let recovered = pooled.transaction.clone_into_consensus();
            let tx = yevm_tx(recovered.inner(), recovered.signer(), 0);
            let mut overlay = yevm_core::cache::Cache::new();
            overlay.set_chain_id(chain_id);
            let mut exe = Executor::new(tx.call.clone().into());
            // Failures (stale nonce, too little balance, ...) still read what they read.
            let _ = rt.block_on(exe.run(&tx.tx, &head.next, &mut overlay, &chain));
            txs += 1;
        }
        // The state view (a read transaction) ends here.
        let values = chain.into_values();
        sink.add_pending(
            (head.number, head.hash.0),
            values,
            txs,
            started.elapsed().as_micros() as u64,
        );
    }
}
