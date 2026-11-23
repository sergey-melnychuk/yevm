//! Simulation latency, silt vs reth's database read-only (silt PLAN M6,
//! T6.3: sides A and B1 in one process).
//!
//! B1 is [`RethDb`] on the live node's datadir, opened read-only. It can
//! only read state exactly at its state frontier F, which trails the head
//! (by up to ~35 blocks with `--engine.persistence-threshold 50` and
//! 30 state-masking blocks). So both sides simulate block F+1 on state F:
//! - A: a silt-cache `Store` fed from the silt feed and held at F (diffs
//!   past F wait), misses over the lookup fast path pinned at F, JSON-RPC
//!   as the fallback;
//! - B1: `RethDb` pinned at F.
//!
//! Each time the frontier moves, block F+1 runs: every transaction alone
//! (A and B alternate, swapping which goes first), then the whole block
//! as a bundle on each side. Both sides run the same transactions (read
//! once from the datadir) through silt-cache's `sim` module, so the timing
//! code is identical. One JSON line per transaction pair and per bundle
//! pair goes to `--out`.
//!
//! An example rather than a bin so it builds with the dev-dependency feature
//! set the `eest`-profile test builds already compiled (a bin would rebuild
//! much of reth's tree with different features).
//!
//! Usage (`cargo build --profile eest -p yevm-reth --example sim-bench`):
//!   sim-bench <datadir> [--feed 127.0.0.1:8585] [--lookup 127.0.0.1:8586]
//!             [--rpc http://127.0.0.1:8545] [--steps 12] [--out sim-b1.jsonl]

use std::collections::VecDeque;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use silt_cache::Cache;
use silt_cache::client::{Continuity, continuity};
use silt_cache::fallback::{Fallback, HttpFallback};
use silt_cache::lookup::{LookupClientConfig, LookupFallback};
use silt_cache::sim::{Counted, Outcome, Reads, Sim, sim_bundle, sim_tx};
use silt_cache::store::Store;
use silt_wire::{BlockDiff, Frame, read_frame, write_frame};
use yevm_core::{call::Block, chain::Chain};
use yevm_reth::RethDb;

struct Args {
    datadir: String,
    feed: String,
    lookup: String,
    rpc: String,
    steps: u64,
    out: String,
}

fn args() -> eyre::Result<Args> {
    let mut it = std::env::args().skip(1);
    let datadir = it
        .next()
        .ok_or_else(|| eyre::eyre!("usage: sim-bench <datadir> [flags]"))?;
    let mut a = Args {
        datadir,
        feed: "127.0.0.1:8585".into(),
        lookup: "127.0.0.1:8586".into(),
        rpc: "http://127.0.0.1:8545".into(),
        steps: 12,
        out: "sim-b1.jsonl".into(),
    };
    while let Some(flag) = it.next() {
        let mut v = || it.next().ok_or_else(|| eyre::eyre!("{flag} needs a value"));
        match flag.as_str() {
            "--feed" => a.feed = v()?,
            "--lookup" => a.lookup = v()?,
            "--rpc" => a.rpc = v()?,
            "--steps" => a.steps = v()?.parse()?,
            "--out" => a.out = v()?,
            other => eyre::bail!("unknown flag {other}"),
        }
    }
    Ok(a)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// Wall-clock UTC time of day, for matching against the node's log.
fn utc_now() -> String {
    let s = now_ms() / 1000;
    format!("{:02}:{:02}:{:02}", s / 3600 % 24, s / 60 % 60, s % 60)
}

fn reads_json(r: &Reads) -> serde_json::Value {
    serde_json::json!({"acc": r.accounts, "slot": r.slots, "other": r.other, "us": r.nanos as f64 / 1e3})
}

fn store_counts(store: &Store) -> (u64, u64, u64) {
    (
        store.stats.hits.load(Ordering::Relaxed),
        store.stats.misses.load(Ordering::Relaxed),
        store.stats.prefetched.load(Ordering::Relaxed),
    )
}

/// What the feed task hands the main loop.
enum Feed {
    Diff(Box<BlockDiff>),
    Reorg(u64, silt_wire::Word),
    Gap,
}

/// Reads frames as they come so the feed connection never backs up while
/// the main loop holds diffs past the frontier.
async fn feed_task(
    addr: String,
    from: u64,
    tx: tokio::sync::mpsc::UnboundedSender<Feed>,
) -> eyre::Result<()> {
    let mut stream = tokio::net::TcpStream::connect(&addr).await?;
    let Some(Frame::Hello {
        chain_id,
        genesis_hash,
        tip,
        ..
    }) = read_frame(&mut stream).await?
    else {
        eyre::bail!("expected Hello");
    };
    write_frame(&mut stream, &silt_wire::hello(chain_id, genesis_hash, 0)).await?;
    // Stay inside the spool (128 blocks on the node).
    let from = from.max(tip.saturating_sub(120));
    println!("feed: tip {tip}, subscribing from {from}");
    write_frame(
        &mut stream,
        &Frame::Subscribe {
            from: Some(from),
            parent_hash: None,
        },
    )
    .await?;
    loop {
        let item = match read_frame(&mut stream).await? {
            Some(Frame::Diff(diff)) => Feed::Diff(Box::new(diff)),
            Some(Frame::Reorg {
                fork_number,
                fork_hash,
            }) => Feed::Reorg(fork_number, fork_hash),
            Some(Frame::GapTooOld { .. }) => Feed::Gap,
            Some(Frame::Heartbeat { .. }) => continue,
            Some(other) => eyre::bail!("unexpected frame {other:?}"),
            None => eyre::bail!("feed closed"),
        };
        if tx.send(item).is_err() {
            return Ok(());
        }
    }
}

/// Applies held and incoming diffs up to `frontier`; later ones wait in
/// `held`. Returns false if the feed is gone.
async fn advance_to(
    frontier: u64,
    store: &Store,
    held: &mut VecDeque<BlockDiff>,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Feed>,
) -> bool {
    loop {
        while let Some(diff) = held.front() {
            if diff.number > frontier {
                return true;
            }
            let diff = held.pop_front().expect("front");
            match continuity(store, &diff) {
                Continuity::Stale => {}
                Continuity::Break => {
                    println!("break at {}: store dropped to cold", diff.number);
                    store.clear();
                    store.apply(&diff);
                }
                Continuity::Extends | Continuity::Overlaps => store.apply(&diff),
            }
            if diff.number.is_multiple_of(8) {
                store.evict(diff.number, silt_cache::DEFAULT_EVICT_AFTER_BLOCKS);
            }
        }
        if store.tip().is_some_and(|(n, _)| n >= frontier) {
            return true;
        }
        match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
            Ok(Some(Feed::Diff(diff))) => held.push_back(*diff),
            Ok(Some(Feed::Reorg(n, hash))) => {
                held.retain(|d| d.number <= n);
                if !store.rollback(n, hash) {
                    println!("reorg to {n}: can't roll back; store dropped to cold");
                    store.clear();
                }
            }
            Ok(Some(Feed::Gap)) => {
                println!("gap: store dropped to cold");
                store.clear();
            }
            Ok(None) => return false,
            Err(_) => {
                println!("feed silent for 30 s while waiting for block {frontier}");
                return true;
            }
        }
    }
}

fn sim_json(sim: &Sim) -> serde_json::Value {
    serde_json::json!({
        "us": sim.elapsed.as_secs_f64() * 1e6,
        "outcome": sim.outcome.to_string(),
        "reads": reads_json(&sim.reads),
    })
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let args = args()?;
    // Opened once for the whole run: the read-only open replays the node's
    // RocksDB WAL (GBs of reads), which the node feels.
    println!("open start {} UTC (unix ms {})", utc_now(), now_ms());
    let reth = Counted::new(Arc::new(RethDb::latest(&args.datadir)?));
    println!("open end {} UTC (unix ms {})", utc_now(), now_ms());
    let rpc: Arc<dyn Fallback> = Arc::new(HttpFallback::new(args.rpc.clone()));
    let fallback: Arc<dyn Fallback> = Arc::new(LookupFallback::new(
        LookupClientConfig::new(&args.lookup),
        rpc,
    ));
    let store = Arc::new(Store::new(256));
    let silt = Counted::new(Cache::from_store(store.clone(), fallback));
    let mut out = std::io::BufWriter::new(std::fs::File::create(&args.out)?);
    let chain_id = Chain::chain_id(&*reth.inner).await?;

    // Warm the store with the 64 diffs before the frontier, as a consumer
    // that had been following would have it.
    let f0 = reth.inner.state_frontier()?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let feed = tokio::spawn(feed_task(args.feed.clone(), f0.saturating_sub(63), tx));
    println!(
        "sim-bench: datadir {} frontier {f0}, feed {} lookup {} rpc {}, {} steps, out {}",
        args.datadir, args.feed, args.lookup, args.rpc, args.steps, args.out
    );

    let mut held = VecDeque::new();
    let (mut last, mut step) = (0u64, 0u64);
    while step < args.steps {
        reth.inner.refresh_provider()?;
        let f = reth.inner.state_frontier()?;
        if f == last || reth.inner.best_block_number()? <= f {
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        if !advance_to(f, &store, &mut held, &mut rx).await {
            eyre::bail!("feed ended: {:?}", feed.await);
        }
        if store.tip().map(|(n, _)| n) != Some(f) {
            println!("store at {:?}, not at frontier {f}: skipping", store.tip());
            last = f;
            continue;
        }
        last = f;
        step += 1;
        let n = f + 1;
        reth.inner.pin(n)?;
        let block: Block = Chain::block(&*reth.inner, n).await?;
        let head = &block.head;
        let behind = store.tip().map(|(t, _)| t).unwrap_or(f);
        println!(
            "step {step}: block {n} on state {f} ({} txs, {} diffs held, store at {behind})",
            block.txs.len(),
            held.len()
        );

        let (mut same, mut differ) = (0usize, 0usize);
        for (i, tx) in block.txs.iter().enumerate() {
            let silt_first = (step as usize + i).is_multiple_of(2);
            let t_ms = now_ms();
            let (h0, m0, p0) = store_counts(&store);
            let (a, b) = if silt_first {
                let a = sim_tx(tx, head, chain_id, &silt).await?;
                let b = sim_tx(tx, head, chain_id, &reth).await?;
                (a, b)
            } else {
                let b = sim_tx(tx, head, chain_id, &reth).await?;
                let a = sim_tx(tx, head, chain_id, &silt).await?;
                (a, b)
            };
            let (h1, m1, p1) = store_counts(&store);
            let equal = a.outcome == b.outcome;
            if equal {
                same += 1;
            } else {
                differ += 1;
                println!(
                    "DIFFER block {n} tx {i}: silt {} | reth {}",
                    a.outcome, b.outcome
                );
            }
            let line = serde_json::json!({
                "kind": "tx", "block": n, "step": step, "i": i, "hash": tx.tx.hash.to_string(),
                "order": if silt_first { "ab" } else { "ba" }, "t_ms": t_ms, "same": equal,
                "failed": matches!(a.outcome, Outcome::Failed(_)),
                "silt": sim_json(&a), "reth": sim_json(&b), "hits": h1 - h0, "misses": m1 - m0, "prefetched": p1 - p0,
            });
            writeln!(out, "{line}")?;
        }

        let silt_first = step.is_multiple_of(2);
        let t_ms = now_ms();
        let (h0, m0, p0) = store_counts(&store);
        let (a, b) = if silt_first {
            let a = sim_bundle(&block.txs, head, chain_id, &silt).await?;
            let b = sim_bundle(&block.txs, head, chain_id, &reth).await?;
            (a, b)
        } else {
            let b = sim_bundle(&block.txs, head, chain_id, &reth).await?;
            let a = sim_bundle(&block.txs, head, chain_id, &silt).await?;
            (a, b)
        };
        let (h1, m1, p1) = store_counts(&store);
        let line = serde_json::json!({
            "kind": "bundle", "block": n, "step": step, "txs": block.txs.len(),
            "order": if silt_first { "ab" } else { "ba" }, "t_ms": t_ms, "same": a.0 == b.0,
            "silt": {"us": a.1.as_secs_f64() * 1e6, "reads": reads_json(&a.2)},
            "reth": {"us": b.1.as_secs_f64() * 1e6, "reads": reads_json(&b.2)},
            "hits": h1 - h0, "misses": m1 - m0, "prefetched": p1 - p0,
        });
        writeln!(out, "{line}")?;
        out.flush()?;
        // Don't hold the pinned state's MDBX read transaction while waiting
        // for the next frontier (minutes).
        reth.inner.unpin();
        println!(
            "step {step} ({} UTC): {same} same, {differ} differ | bundle silt {:.1} ms ({} reads) vs reth {:.1} ms ({} reads), same {}",
            utc_now(),
            a.1.as_secs_f64() * 1e3,
            a.2.total(),
            b.1.as_secs_f64() * 1e3,
            b.2.total(),
            a.0 == b.0
        );
    }
    feed.abort();
    println!("done: {step} steps");
    Ok(())
}
