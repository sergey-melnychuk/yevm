//! Ethereum node that executes blocks with yevm.
//!
//! This is the standard Ethereum node composed with [`YevmEvmConfig`] as its EVM configuration,
//! so block validation, payload building and `eth_call` style RPCs execute on yevm. See
//! [`yevm_reth::exec`] for the limitations, most importantly that only Osaka blocks can be executed:
//! on an existing chain, sync with `--snap.v2` so that only blocks after the snap target are
//! executed.
//!
//! silt is installed alongside (two builder calls): the "silt" ExEx publishes a state diff per
//! committed block on the feed (`silt.toml` `bind`, default 127.0.0.1:8585) and serves binary
//! state lookups and trie proofs (`lookup_bind`, default 127.0.0.1:8586); `silt_multiproof` is
//! merged into the RPC transports `--http`/`--ws`/`--ipc` enable. Config: `./silt.toml`,
//! defaults if absent. With `read_touches` (the default) the executors report each block's
//! loaded keys, and the feed's diffs carry them, valued at the block (silt PLAN D13). With
//! `pending_reads` too (the default), the "yevm-pending" ExEx feeds each new head to a worker
//! that runs the pool's best transactions on it and hands what they read to the same sink, so
//! the next diff carries those as well (silt PLAN D14, [`yevm_reth::pending`]).
//!
//! Opt-in simulation baseline (silt PLAN M6, T6.4): with `YEVM_SIM_OUT=<file>` set, a third
//! builder call installs the "yevm-sim" ExEx. It hands each newly committed block N+1 to one
//! worker thread, which simulates the block's transactions on state N straight from the node's
//! provider (in-memory blocks over the database): each transaction alone, then the block as a
//! bundle, timed by silt-cache's `sim` module. One JSON line per simulation goes to the file. The
//! worker takes one block at a time; a block arriving while it is busy is skipped, never queued,
//! so the ExEx acks at once and never holds reth's WAL. The worker thread runs at nice
//! `YEVM_SIM_NICE` (default 10). Unset, nothing changes.

use reth_ethereum::{
    EthPrimitives,
    chainspec::ChainSpec,
    cli::interface::Cli,
    node::{
        EthereumNode,
        api::{FullNodeTypes, NodeTypes},
        builder::{BuilderContext, components::ExecutorBuilder},
        node::EthereumAddOns,
        snap::EthereumBackfill,
    },
};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use alloy_consensus::BlockHeader as _;
use alloy_eips::eip1559::BaseFeeParams;
use futures::TryStreamExt;
use reth_ethereum::pool::{PoolTransaction, TransactionPool};
use reth_ethereum::{
    chainspec::EthChainSpec,
    exex::{ExExContext, ExExEvent},
    node::api::FullNodeComponents,
    provider::{HeaderProvider, StateProviderFactory},
};
use reth_ethereum_primitives::TransactionSigned;
use silt_cache::sim::{Counted, Outcome, Reads, sim_bundle, sim_tx};
use tracing::{info, warn};
use yevm_base::Int;
use yevm_core::call::Block;
use yevm_reth::pending::NewHead;
use yevm_reth::{StateChain, exec::YevmEvmConfig, yevm_block, yevm_head};

/// Builds the yevm EVM configuration for an Ethereum node; with a sink, its executors report
/// each block's loaded keys for silt's read touches (silt PLAN D13).
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
struct YevmExecutorBuilder {
    reads: Option<Arc<silt_node::reads::ReadTouchSink>>,
}

impl<Node> ExecutorBuilder<Node> for YevmExecutorBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec = ChainSpec, Primitives = EthPrimitives>>,
{
    type EVM = YevmEvmConfig;

    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        let config = YevmEvmConfig::new(ctx.chain_spec());
        Ok(match self.reads {
            Some(sink) => config.with_read_touches(sink),
            None => config,
        })
    }
}

/// One committed block for the sim worker: block N+1 and its parent N.
struct SimJob {
    block: Block,
    number: u64,
    parent: alloy_primitives::B256,
    committed_ms: u64,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn reads_json(r: &Reads) -> serde_json::Value {
    serde_json::json!({"acc": r.accounts, "slot": r.slots, "other": r.other, "us": r.nanos as f64 / 1e3})
}

/// The "yevm-sim" ExEx (see the module docs): hands blocks to the worker and acks at once.
async fn run_sim_exex<Node>(mut ctx: ExExContext<Node>, out: PathBuf) -> eyre::Result<()>
where
    Node: FullNodeComponents<Types: NodeTypes<Primitives = EthPrimitives, ChainSpec = ChainSpec>>,
{
    let chain_id = ctx.config.chain.chain().id();
    let nice: i32 = std::env::var("YEVM_SIM_NICE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let (jobs, rx) = std::sync::mpsc::sync_channel::<SimJob>(0);
    let provider = ctx.provider().clone();
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&out)?;
    std::thread::Builder::new()
        .name("yevm-sim".into())
        .spawn(move || sim_worker(rx, provider, chain_id, nice, file))?;
    info!(target: "yevm::sim", out = %out.display(), nice, "yevm-sim: simulating each new block");

    let mut skipped = 0u64;
    while let Some(notification) = ctx.notifications.try_next().await? {
        if let Some(chain) = notification.committed_chain() {
            for block in chain.blocks_iter() {
                let job = SimJob {
                    block: yevm_block(block),
                    number: block.header().number(),
                    parent: block.header().parent_hash(),
                    committed_ms: now_ms(),
                };
                if jobs.try_send(job).is_err() {
                    skipped += 1;
                    info!(target: "yevm::sim", number = block.header().number(), skipped, "yevm-sim: worker busy, block skipped");
                }
            }
            ctx.events
                .send(ExExEvent::FinishedHeight(chain.tip().num_hash()))?;
        }
    }
    Ok(())
}

/// Simulates one block at a time on its parent's state from the node's provider.
fn sim_worker<P>(
    rx: std::sync::mpsc::Receiver<SimJob>,
    provider: P,
    chain_id: u64,
    nice: i32,
    file: std::fs::File,
) where
    P: StateProviderFactory + HeaderProvider + Clone + Send + Sync + 'static,
{
    // Thread-level nice: on Linux, PRIO_PROCESS with a thread id sets that thread only.
    // Elsewhere (a development machine) the worker keeps the default priority.
    #[cfg(not(target_os = "linux"))]
    let _ = nice;
    #[cfg(target_os = "linux")]
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, nice);
    }
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            warn!(target: "yevm::sim", %err, "yevm-sim: no runtime, worker stopped");
            return;
        }
    };
    let mut out = std::io::BufWriter::new(file);
    while let Ok(job) = rx.recv() {
        if let Err(err) = rt.block_on(sim_block(&job, &provider, chain_id, &mut out)) {
            warn!(target: "yevm::sim", number = job.number, %err, "yevm-sim: block failed");
        }
    }
}

async fn sim_block<P>(
    job: &SimJob,
    provider: &P,
    chain_id: u64,
    out: &mut impl Write,
) -> eyre::Result<()>
where
    P: StateProviderFactory + HeaderProvider + Clone + Send + Sync + 'static,
{
    let started_ms = now_ms();
    let state = provider.state_by_block_hash(job.parent)?;
    let chain = Counted::new(Arc::new(StateChain::new(state, provider.clone(), chain_id)));
    let head = &job.block.head;
    for (i, tx) in job.block.txs.iter().enumerate() {
        let sim = sim_tx(tx, head, chain_id, &chain).await?;
        let line = serde_json::json!({
            "side": "reth-node", "kind": "tx", "block": job.number, "i": i,
            "hash": tx.tx.hash.to_string(), "us": sim.elapsed.as_secs_f64() * 1e6,
            "outcome": sim.outcome.to_string(), "failed": matches!(sim.outcome, Outcome::Failed(_)),
            "reads": reads_json(&sim.reads),
        });
        writeln!(out, "{line}")?;
    }
    let (outcomes, elapsed, reads) = sim_bundle(&job.block.txs, head, chain_id, &chain).await?;
    let failed = outcomes
        .iter()
        .filter(|o| matches!(o, Outcome::Failed(_)))
        .count();
    let digest = silt_cache::sim::fnv1a(
        outcomes
            .iter()
            .map(|o| o.to_string())
            .collect::<Vec<_>>()
            .join("|")
            .as_bytes(),
    );
    let line = serde_json::json!({
        "side": "reth-node", "kind": "bundle", "block": job.number, "txs": job.block.txs.len(),
        "us": elapsed.as_secs_f64() * 1e6, "failed": failed, "digest": format!("{digest:016x}"),
        "reads": reads_json(&reads), "committed_ms": job.committed_ms, "started_ms": started_ms,
    });
    writeln!(out, "{line}")?;
    out.flush()?;
    Ok(())
}

/// The "yevm-pending" ExEx (silt D14): hands each new head, with the next block's environment,
/// to the pool worker ([`yevm_reth::pending::run_worker`]) and acks at once.
async fn run_pending_exex<Node>(
    mut ctx: ExExContext<Node>,
    sink: Arc<silt_node::reads::ReadTouchSink>,
) -> eyre::Result<()>
where
    Node: FullNodeComponents<Types: NodeTypes<Primitives = EthPrimitives, ChainSpec = ChainSpec>>,
    Node::Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>>,
{
    let chain_id = ctx.config.chain.chain().id();
    let (heads, rx) = std::sync::mpsc::channel::<NewHead>();
    let provider = ctx.provider().clone();
    let pool = ctx.pool().clone();
    std::thread::Builder::new()
        .name("yevm-pending".into())
        .spawn(move || yevm_reth::pending::run_worker(rx, provider, pool, sink, chain_id))?;
    info!(target: "yevm::pending", "yevm-pending: running pool transactions on each new head");
    while let Some(notification) = ctx.notifications.try_next().await? {
        if let Some(chain) = notification.committed_chain() {
            let tip = chain.tip();
            let header = tip.header();
            let mut next = yevm_head(header, tip.hash());
            next.number = (header.number() + 1).into();
            next.timestamp = Int::from(header.timestamp() + 12);
            next.base_fee = Int::from(
                header
                    .next_block_base_fee(BaseFeeParams::ethereum())
                    .unwrap_or_default(),
            );
            next.parent_hash = next.hash;
            next.hash = Int::ZERO;
            // The worker may be gone (no runtime): nothing to do then.
            let _ = heads.send(NewHead {
                number: header.number(),
                hash: tip.hash(),
                next,
            });
            ctx.events
                .send(ExExEvent::FinishedHeight(chain.tip().num_hash()))?;
        }
    }
    Ok(())
}

fn main() {
    if let Err(err) = Cli::parse_args().run(async move |builder, _| {
        info!(target: "reth::cli", "Launching node with the yevm executor");
        let backfill = EthereumBackfill::new(builder.config());
        let silt = silt_node::SiltConfig::load()?;
        // D13: executors report each block's loaded keys; the silt ExEx values and publishes
        // them. 256 blocks: keys of blocks that never commit age out.
        let reads = silt
            .read_touches
            .then(|| Arc::new(silt_node::reads::ReadTouchSink::new(256)));
        let silt_reads = reads
            .clone()
            .map(|sink| sink as Arc<dyn silt_node::reads::ReadTouches>);
        // D14: the pool worker feeds the same sink.
        let pending_sink = reads.clone().filter(|_| silt.pending_reads);
        let sim_out = std::env::var_os("YEVM_SIM_OUT").map(PathBuf::from);
        let handle = builder
            .with_types::<EthereumNode>()
            .with_components(EthereumNode::components().executor(YevmExecutorBuilder { reads }))
            .with_add_ons(EthereumAddOns::default())
            .install_exex("silt", async move |ctx| {
                Ok(silt_node::run_exex_with_reads(ctx, silt, silt_reads))
            })
            .install_exex_if(pending_sink.is_some(), "yevm-pending", async move |ctx| {
                Ok(run_pending_exex(
                    ctx,
                    pending_sink.expect("checked by install_exex_if"),
                ))
            })
            .install_exex_if(sim_out.is_some(), "yevm-sim", async move |ctx| {
                Ok(run_sim_exex(
                    ctx,
                    sim_out.expect("checked by install_exex_if"),
                ))
            })
            .extend_rpc_modules(|ctx| {
                ctx.modules
                    .merge_configured(silt_node::rpc_module(ctx.provider().clone()))?;
                Ok(())
            })
            .launch_with_debug_capabilities_and_backfill(backfill)
            .await?;

        handle.wait_for_node_exit().await
    }) {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
