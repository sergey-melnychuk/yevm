use std::{
    fs::File,
    io::{BufReader, BufWriter, Read, Write},
    path::Path,
    time::Instant,
};

use alloy_provider::ProviderBuilder;
use eyre::OptionExt;
use futures::{StreamExt, channel::mpsc};
use yevm_base::{Acc, Int, int, math::lift};
use yevm_core::{
    cache::Cache,
    call::{Block, Head, Receipt, TxFull},
    chain::{Chain, Fetched},
    exe::{CallResult, Executor, post_block, pre_block},
    rpc::Rpc,
    state::{Account, State},
    trace::filter,
};
use yevm_misc::{buf::Buf, hex::parse_vec};

const YEVM_RPC_URL: &str = "YEVM_RPC_URL";

// ./target/release/replay - replay the latest block
// ./target/release/replay <block> - replay the block number
// ./target/release/replay <block>:<index> | <hash> - replay specific transaction
// ./target/release/replay watch - loop forever, replaying each new block as it
//   lands (polls for the tip to move); ignores any block/tx selection -- always
//   tracks latest. Keeps the same `Rpc`/`RethDb` alive across blocks instead of
//   reopening per run, which is where startup cost (esp. --with-reth) is paid.
// ./target/release/replay ... --skip-check - do not check against revm
// ./target/release/replay ... --skip-cache - ignore cached state
// ./target/release/replay ... --skip-stats - do not print per-tx stats lines
// ./target/release/replay ... --with-reth <datadir> - read state from a local reth
//   MDBX datadir instead of RPC (needs `cargo build --features reth`). Reaches
//   any block within the node's retained account/storage history (see
//   `reth.toml`'s prune distance, ~10064 blocks/1.4 days on a --minimal node by
//   default), not just the latest -- see yevm-reth's `RethDb::pin`. Requesting
//   something older just fails with reth's own error, not a pre-emptive guess.

/// Dispatches `Chain` calls to whichever backend is active, so `pre_block`/`Executor::run`
/// don't need to know or care which one is in use. Borrows both variants (rather than
/// owning `RethDb`) so `watch` mode can rebuild a fresh `AnyChain` each block while still
/// calling `RethDb::refresh()`/`best_block_number()` on the same long-lived instance
/// between iterations.
enum AnyChain<'a> {
    Rpc(&'a Rpc),
    #[cfg(feature = "reth")]
    Reth(&'a yevm_reth::RethDb),
}

#[async_trait::async_trait]
impl Chain for AnyChain<'_> {
    async fn get(&self, acc: &Acc, key: &Int) -> eyre::Result<Int> {
        match self {
            Self::Rpc(c) => c.get(acc, key).await,
            #[cfg(feature = "reth")]
            Self::Reth(c) => c.get(acc, key).await,
        }
    }

    async fn acc(&self, acc: &Acc) -> eyre::Result<Account> {
        match self {
            Self::Rpc(c) => c.acc(acc).await,
            #[cfg(feature = "reth")]
            Self::Reth(c) => c.acc(acc).await,
        }
    }

    async fn code(&self, acc: &Acc) -> eyre::Result<(Buf, Int)> {
        match self {
            Self::Rpc(c) => c.code(acc).await,
            #[cfg(feature = "reth")]
            Self::Reth(c) => c.code(acc).await,
        }
    }

    async fn nonce(&self, acc: &Acc) -> eyre::Result<u64> {
        match self {
            Self::Rpc(c) => c.nonce(acc).await,
            #[cfg(feature = "reth")]
            Self::Reth(c) => c.nonce(acc).await,
        }
    }

    async fn balance(&self, acc: &Acc) -> eyre::Result<Int> {
        match self {
            Self::Rpc(c) => c.balance(acc).await,
            #[cfg(feature = "reth")]
            Self::Reth(c) => c.balance(acc).await,
        }
    }

    async fn head(&self, number: u64) -> eyre::Result<Head> {
        match self {
            Self::Rpc(c) => c.head(number).await,
            #[cfg(feature = "reth")]
            Self::Reth(c) => c.head(number).await,
        }
    }

    async fn block(&self, number: u64) -> eyre::Result<Block> {
        match self {
            Self::Rpc(c) => c.block(number).await,
            #[cfg(feature = "reth")]
            Self::Reth(c) => c.block(number).await,
        }
    }

    async fn chain_id(&self) -> eyre::Result<u64> {
        match self {
            Self::Rpc(c) => c.chain_id().await,
            #[cfg(feature = "reth")]
            Self::Reth(c) => c.chain_id().await,
        }
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        println!("{e}");
        std::process::exit(1);
    }
}

async fn run() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    let Ok(url) = std::env::var(YEVM_RPC_URL) else {
        eyre::bail!("{YEVM_RPC_URL} not set");
    };
    let mut rpc = Rpc::latest(url.clone()).await?;
    let chain_id = rpc.chain_id().await?;

    // `--with-reth <datadir>` takes a value, so strip the flag *and* its
    // value before any other arg is scanned -- otherwise the datadir path
    // would be mistaken for the positional block/hash argument below.
    let raw_args: Vec<String> = std::env::args().collect();
    let with_reth: Option<String> = match raw_args.iter().position(|a| a == "--with-reth") {
        Some(i) => Some(
            raw_args
                .get(i + 1)
                .cloned()
                .ok_or_else(|| eyre::eyre!("--with-reth requires a <datadir> argument"))?,
        ),
        None => None,
    };
    let args: Vec<String> = {
        let mut v = raw_args;
        if let Some(i) = v.iter().position(|a| a == "--with-reth") {
            v.drain(i..=i + 1);
        }
        v
    };

    #[cfg(not(feature = "reth"))]
    if with_reth.is_some() {
        eyre::bail!("--with-reth requires building with `cargo build --features reth`");
    }

    // `RethDb` is pinned to a specific block's pre-state via `pin()` below,
    // right before that block gets replayed -- for "latest", not the tip
    // itself here (see yevm-reth's module docs on `pin`/`history_by_block_
    // number` for what's actually reachable). `reth_latest` stays `None`
    // (and thus a no-op below) when the feature isn't compiled in.
    #[cfg(feature = "reth")]
    let reth_db: Option<yevm_reth::RethDb> = with_reth
        .as_deref()
        .map(yevm_reth::RethDb::latest)
        .transpose()?;
    #[cfg(feature = "reth")]
    let reth_latest: Option<u64> = reth_db
        .as_ref()
        .map(|db| db.best_block_number())
        .transpose()?
        .map(|tip| tip + 1);
    #[cfg(not(feature = "reth"))]
    let reth_latest: Option<u64> = None;

    // `--with-reth` always targets `tip + 1`, a different block on every
    // run -- the on-disk `fetch/<block>.json` cache wouldn't apply run to
    // run, and there is no receipt/revm-comparison path wired up for it
    // (see AnyChain above), so both are forced on rather than left to
    // silently do the wrong thing if the user forgets the flags.
    let skip_check = with_reth.is_some() || args.iter().skip(1).any(|arg| arg == "--skip-check");
    let skip_cache = with_reth.is_some() || args.iter().skip(1).any(|arg| arg == "--skip-cache");
    let skip_stats = args.iter().skip(1).any(|arg| arg == "--skip-stats");

    let arg = args
        .iter()
        .filter(|arg| !arg.starts_with("--"))
        .nth(1)
        .cloned()
        .unwrap_or_else(|| String::from("latest"));
    let watch = arg == "watch";

    let (mut number, mut index) = if watch {
        (0u64, None) // resolved for real at the top of the loop below
    } else {
        let latest = reth_latest.unwrap_or(rpc.block_number);

        if arg.starts_with("0x") {
            if parse_vec(&arg).is_err() {
                eyre::bail!("Invalid hex literal: {arg}");
            }
            let hash = int(&arg);
            let receipt = rpc.receipt(hash).await?;
            let block = receipt.block_number.as_u64();
            let index = receipt.transaction_index.as_u64();
            (block, Some(index as usize))
        } else if arg.contains(":") {
            let mut split = arg.split(":");
            let block = split.next().ok_or_eyre("invalid block:index format")?;
            let block: u64 = if block == "latest" {
                latest
            } else {
                block.parse()?
            };
            let index: usize = split
                .next()
                .ok_or_eyre("invalid block:index format")?
                .parse()?;
            (block, Some(index))
        } else if arg == "latest" {
            (latest, None)
        } else {
            (arg.parse()?, None)
        }
    };

    // No pre-check against what `--with-reth` can or can't replay here --
    // `RethDb::pin` (called per block below) actually tries, via
    // `factory.latest()`/`history_by_block_number()`, and its own error
    // (via `?`) is what reports an unreachable block, rather than a guess
    // made in advance about what "should" work.

    // `watch` processes strictly in order: block N+1 only after N, never
    // skipping ahead to whatever the tip happens to be. `None` means
    // nothing processed yet, in which case the first block is whatever's
    // currently latest -- watch doesn't backfill all the way from genesis.
    let mut last_processed: Option<u64> = None;

    loop {
        if watch {
            // Poll once a second until the next sequential block is
            // available, reusing the same `Rpc`/`RethDb` instead of
            // reopening either -- this is the whole point of `watch`: the
            // ~1s `RethDb::latest()` startup cost (see yevm-reth's module
            // docs) is paid once for the process, not once per block.
            let wait_start = Instant::now();
            loop {
                // `RethDb::pin` (below) checks the tip itself and refreshes
                // as needed, but the readiness check here (`next <= head`)
                // needs its own fresh read to know whether to wait at all
                // -- `self.provider` is a single MDBX snapshot and doesn't
                // see new blocks on its own no matter how it was opened
                // (see `refresh_provider`'s docs).
                #[cfg(feature = "reth")]
                let head = if let Some(db) = &reth_db {
                    db.refresh_provider()?;
                    db.best_block_number()? + 1
                } else {
                    rpc = Rpc::latest(url.clone()).await?;
                    rpc.block_number
                };
                #[cfg(not(feature = "reth"))]
                let head = {
                    rpc = Rpc::latest(url.clone()).await?;
                    rpc.block_number
                };

                let next = last_processed.map(|n| n + 1).unwrap_or(head);

                if next <= head {
                    let elapsed = wait_start.elapsed().as_secs_f64();
                    if elapsed >= 1.0 {
                        println!("\r(new block detected after {elapsed:.3} seconds)");
                    }
                    number = next;
                    break;
                }
                print!(
                    "\r(waiting for next block... {:.0}s)",
                    wait_start.elapsed().as_secs_f64()
                );
                std::io::Write::flush(&mut std::io::stdout())?;
                tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
            }
            index = None;
        }

        // Pins state to right before `number` -- `RethDb::pin` uses
        // `factory.latest()` when it's the current tip + 1, or
        // `history_by_block_number()` to reach a bit further back, as long
        // as it's within the node's retained history
        // (`account_history`/`storage_history` prune distance in
        // `reth.toml`). No pre-check here: this either succeeds or its own
        // error (via `?`) reports why `number` isn't reachable.
        #[cfg(feature = "reth")]
        if let Some(db) = &reth_db {
            db.pin(number)?;
        }
        let block_number = number;

        let (ytx, mut yrx) = mpsc::channel(4 * 1024 * 1024);
        let filter = if !skip_check {
            filter::STEP
        } else {
            filter::NONE
        };
        let mut cache = Cache::with_sender(ytx, filter);

        // TODO: make single-tx also replayable? just save all fetches to block:index.js
        std::fs::create_dir_all("fetch")?;
        let path = format!("fetch/{}.json", block_number);
        let fetches = Path::new(&path);
        let block = if !skip_cache && fetches.exists() && index.is_none() {
            let file = File::open(fetches)?;
            let mut reader = BufReader::new(file);
            let mut content = String::new();
            reader.read_to_string(&mut content)?;
            let fetched: Vec<Fetched> = serde_json::from_str(&content)?;
            let Some(Fetched::ChainId(chain_id)) = fetched.first().cloned() else {
                eyre::bail!("Cannot find fetched chain id");
            };
            cache.set_chain_id(chain_id);
            let Some(Fetched::Block(block)) = fetched.get(1).cloned() else {
                eyre::bail!("Cannot find stored block");
            };
            cache.prefetched(fetched);
            block
        } else {
            // Bootstrap fetch: one-off metadata, stays on RPC regardless of
            // `--with-reth` -- not the per-account/storage cost that matters.
            let chain_id = rpc.chain_id().await?;
            cache.set_chain_id(chain_id);
            cache.save_fetched(Fetched::ChainId(chain_id), 0.0);

            let block = rpc.block(block_number).await?;
            cache.save_fetched(Fetched::Block(block.clone()), 0.0);
            block
        };

        let head = block.head.clone();
        // Only matters for `AnyChain::Rpc` (built below), which reads state at
        // whatever block this pins -- must happen before `chain` borrows `rpc`.
        rpc.reset(head.number.as_u64() - 1, head.parent_hash);

        #[cfg(feature = "reth")]
        let chain = match &reth_db {
            Some(db) => AnyChain::Reth(db),
            None => AnyChain::Rpc(&rpc),
        };
        #[cfg(not(feature = "reth"))]
        let chain = AnyChain::Rpc(&rpc);

        let backend = match &chain {
            AnyChain::Rpc(_) => "rpc",
            #[cfg(feature = "reth")]
            AnyChain::Reth(_) => "reth",
        };
        println!(
            "Begin: {} / {} [{backend}]",
            head.number.as_u64(),
            head.hash
        );

        let (rtx, mut rrx) = tokio::sync::mpsc::channel(4096);
        let handle = tokio::spawn(async move {
            if skip_check {
                return;
            }
            let is_trace = std::env::var("TRACE").is_ok();
            if is_trace {
                println!("---\nSTREAMING OPENED");
            }
            let mut skip = 0;
            loop {
                let y = yrx.next().await;
                let r = rrx.recv().await;
                if let (Some(y_trace), Some(mut r)) = (y, r) {
                    let yevm_core::trace::Event::Step(mut y) = y_trace.event else {
                        continue;
                    };
                    if y != r {
                        println!("===\nSTEP MISMATCH:\nYEVM: {y:#?}\nREVM: {r:#?}\n(skip: {skip})");
                        break;
                    }
                    if is_trace {
                        for line in r.debug.drain(..) {
                            y.debug.push(format!("REVM: {line}"));
                        }
                        println!("{y:#?}");
                    }
                    skip += 1;
                } else {
                    break;
                }
            }
            if is_trace {
                println!("STREAMING CLOSED [{skip} items]\n---");
            }
        });

        let txs = block.txs.clone();
        let pack = (txs.clone(), head.clone(), index, chain_id);

        let (revm_result_tx, mut revm_result_rx) = tokio::sync::mpsc::channel::<RevmResult>(1);

        let provider = ProviderBuilder::new().connect(&url).await?;
        tokio::task::spawn_blocking(move || {
            if skip_check {
                return;
            }
            let (txs, head, index, network_chain_id) = pack;
            if let Some(i) = index {
                let tx = &txs[i];
                let (call, tx) = (tx.call.clone().into(), tx.tx.clone());
                if let Err(e) = live::run_one(
                    call,
                    tx,
                    head,
                    network_chain_id,
                    rtx,
                    revm_result_tx,
                    provider,
                ) {
                    eprintln!("REVM replay error (no trace steps): {e:#}");
                }
            } else if let Err(e) =
                live::run_all(network_chain_id, &txs, head, rtx, revm_result_tx, provider)
            {
                eprintln!("REVM replay error (no trace steps): {e:#}");
            }
        });

        let txs = if let Some(i) = index {
            vec![txs[i].clone()]
        } else {
            txs
        };

        pre_block(&head, &mut cache, &chain).await?;

        let n = txs.len();
        let mut ok = 0;
        let mut gas_total = 0;
        let mut sec_total = 0.0;
        let mut revm_drift: Vec<(Acc, Int)> = Vec::new();
        for (i, tx) in txs.into_iter().enumerate() {
            if std::env::var("TRACE").is_ok() {
                println!("{}", serde_json::to_string_pretty(&tx).unwrap());
            }

            let hash = tx.tx.hash;
            let sender = tx.call.from;
            let TxFull { tx, call } = tx;
            let mut exe = Executor::new(call.into());
            cache.reset();

            let now = Instant::now();
            let result = exe.run(&tx, &head, &mut cache, &chain).await?;
            let ms = now.elapsed().as_micros() as f64 / 1000.0;

            let gas = result.gas().finalized;
            let (fetches, fetching) = cache.fetch_stats();

            let stats = if fetches > 0 {
                format!(
                    "{ms:5.3}ms/{:5.3}ms, F:{fetches}/{fetching:5.3}ms",
                    ms - fetching
                )
            } else {
                format!("{ms:5.3}ms")
            };

            if skip_check {
                ok += 1;
                gas_total += gas;
                sec_total += ms - fetching;
                if !skip_stats {
                    println!("{hash}: [{}/{n}, {gas} gas, {stats}]", i + 1);
                }
                continue;
            }

            let receipt = rpc.receipt(hash).await?;
            let ty = receipt.r#type.as_u8();
            let Some(RevmResult {
                call: revm_call,
                state: revm_state,
                millis,
            }) = revm_result_rx.recv().await
            else {
                eyre::bail!("revm result unavailable");
            };
            let stats = stats + &format!(", R:{millis:5.3}ms");

            let (mut violations, revm_gas_ok) = check_result(result, receipt, Some(revm_call));
            let skip_value = if revm_gas_ok {
                vec![]
            } else {
                vec![sender, head.coinbase]
            };
            let new_drift = check_state(
                revm_state,
                &mut cache,
                &mut violations,
                &skip_value,
                &revm_drift,
            );
            revm_drift.extend(new_drift);

            if violations.is_empty() {
                gas_total += gas;
                sec_total += ms - fetching;
                if !skip_stats {
                    println!("{hash} [type:{ty}]: OK [{}/{n}, {gas} gas, {stats}]", i + 1);
                }
                ok += 1;
            } else {
                println!(
                    "{hash} [type:{ty}]: FAIL={}:{} [{}/{n}, {stats}]\n{}",
                    head.number.as_u64(),
                    index.unwrap_or(i),
                    i + 1,
                    violations.join("\n")
                );
            }
        }

        post_block(&block.withdrawals, &mut cache, &chain).await?;

        if !skip_cache && !fetches.exists() && index.is_none() {
            let fetched = std::mem::take(&mut cache.fetched);
            let file = File::create(fetches)?;
            let mut writer = BufWriter::new(file);
            let content = serde_json::to_vec(&fetched)?;
            writer.write_all(&content)?;
        }

        let ok = if n > 1 {
            format!("Block: {}, {ok}/{n} OK", head.number.as_u64())
        } else {
            String::new()
        };
        let stat = if gas_total > 0 && sec_total > 0.0 {
            format!(
                "{gas_total} gas, {sec_total:5.3}ms: ~{:.2} gas/sec",
                gas_total as f64 * 1000.0 / sec_total as f64
            )
        } else {
            String::new()
        };
        if !ok.is_empty() {
            println!("{ok}, {stat}");
        }

        let _ = cache.sender.take();
        handle.await?;

        last_processed = Some(number);
        if !watch {
            break;
        }
    } // loop

    Ok(())
}

fn fmt_int(v: Int) -> String {
    let bytes = v.as_ref();
    let start = bytes
        .iter()
        .position(|&b| b != 0)
        .unwrap_or(bytes.len() - 1);
    bytes[start..].iter().fold("0x".to_string(), |mut s, b| {
        s.push_str(&format!("{b:02x}"));
        s
    })
}

fn check_result(
    result: CallResult,
    receipt: Receipt,
    revm: Option<CallResult>,
) -> (Vec<String>, bool) {
    let mut violations = Vec::new();
    let mut revm_gas_ok = true;
    let used_gas = receipt.gas_used.as_u64() as i64;
    match result {
        CallResult::Done { status, ret, gas } => {
            if status != receipt.status {
                violations.push(format!(
                    " ok: have {} want {}",
                    status.as_u8(),
                    receipt.status.as_u8()
                ));
            }
            if gas.finalized != used_gas {
                let diff = gas.finalized - used_gas;
                violations.push(format!(
                    "gas: have {} want {used_gas} [{diff:+}]",
                    gas.finalized
                ));
            }

            if let Some(revm) = revm {
                let CallResult::Done {
                    status: revm_status,
                    ret: revm_ret,
                    gas: revm_gas,
                } = revm
                else {
                    violations.push(format!(
                        "revm: call result mismatch\n  have {ret:#?}\n revm {revm:#?}"
                    ));
                    return (violations, revm_gas_ok);
                };
                if revm_status == receipt.status && status != revm_status {
                    violations.push(format!(
                        "revm: status mismatch: have {status} want {revm_status}"
                    ));
                }
                if revm_gas.finalized != used_gas {
                    // let diff = revm_gas.finalized - used_gas;
                    // violations.push(format!(
                    //     "revm: gas != receipt: revm={} receipt={used_gas} [{diff:+}]",
                    //     revm_gas.finalized
                    // ));
                    revm_gas_ok = false;
                } else if gas.finalized != revm_gas.finalized {
                    let diff = gas.finalized - revm_gas.finalized;
                    violations.push(format!(
                        "revm: gas mismatch: have {} want {} [{diff:+}]",
                        gas.finalized, revm_gas.finalized
                    ));
                }
                if ret != revm_ret {
                    violations.push(format!(
                        "revm: ret mismatch: have {} bytes want {} bytes",
                        ret.len(),
                        revm_ret.len()
                    ));
                }
            }
        }
        CallResult::Created { acc, ref code, gas } => {
            if Some(acc) != receipt.contract_address {
                violations.push(format!(
                    "new: have {} want {}",
                    acc,
                    receipt.contract_address.unwrap_or_default()
                ));
            }
            if gas.finalized != used_gas {
                let diff = gas.finalized - used_gas;
                violations.push(format!(
                    "gas: have {} want {used_gas} [{diff:+}]",
                    gas.finalized
                ));
            }

            if let Some(revm) = revm {
                let CallResult::Created {
                    acc: revm_acc,
                    code: revm_code,
                    gas: revm_gas,
                } = revm
                else {
                    violations.push(format!(
                        "revm: call result mismatch\n  have {result:#?}\n revm {revm:#?}"
                    ));
                    return (violations, revm_gas_ok);
                };
                if acc != revm_acc {
                    violations.push(format!(
                        "revm: created mismatch: have {acc} want {revm_acc}"
                    ));
                }
                if code != &revm_code {
                    violations.push(format!(
                        "revm: code mismatch: have {} bytes want {} bytes",
                        code.len(),
                        revm_code.len()
                    ));
                }
                if gas.finalized != revm_gas.finalized {
                    violations.push(format!(
                        "revm: gas mismatch: have {} want {}",
                        gas.finalized, revm_gas.finalized
                    ));
                }
            }
        }
    }
    (violations, revm_gas_ok)
}

pub type Env = Vec<(Acc, Account, Vec<(Int, Int)>)>;

// Returns new drift entries for accounts that were in skip_value but had a value mismatch,
// so callers can accumulate the revm drift across transactions.
fn check_state(
    state: Env,
    cache: &mut Cache,
    violations: &mut Vec<String>,
    skip_value: &[Acc],
    drift: &[(Acc, Int)],
) -> Vec<(Acc, Int)> {
    let wadd = lift(|[a, b]| a.wrapping_add(b));
    let wsub = lift(|[a, b]| a.wrapping_sub(b));
    let mut new_drift = Vec::new();
    for (acc, account, storage) in state {
        let is_empty = account.value.is_zero()
            && account.nonce.is_zero()
            && account.code.0.is_empty()
            && (storage.is_empty() || storage.iter().all(|(_, v)| v.is_zero()));
        if is_empty {
            continue;
        }

        let actual = cache.account(&acc).cloned().unwrap_or_default();
        if actual.code.0 != account.code.0 {
            violations.push(format!(
                "REVM: account {acc} code mismatch\n  want {} bytes\n  have {} bytes",
                account.code.0.len(),
                actual.code.0.len()
            ));
        }

        let acc_drift = drift
            .iter()
            .filter(|(a, _)| *a == acc)
            .fold(Int::ZERO, |s, (_, d)| wadd([s, *d]));
        let revm_value = wadd([account.value, acc_drift]);
        if actual.value != revm_value {
            if skip_value.contains(&acc) {
                new_drift.push((acc, wsub([actual.value, revm_value])));
            } else {
                let (sign, diff) = if actual.value >= revm_value {
                    ('+', wsub([actual.value, revm_value]))
                } else {
                    ('-', wsub([revm_value, actual.value]))
                };
                violations.push(format!(
                    "REVM: account {acc} value mismatch\n  want {}\n  have {} [{sign}{}]",
                    revm_value,
                    actual.value,
                    fmt_int(diff)
                ));
            }
        }

        if actual.nonce != account.nonce {
            violations.push(format!(
                "REVM: account {acc} nonce mismatch\n  want {}\n  have {}",
                account.nonce, actual.nonce
            ));
        }
        for (key, val) in storage {
            let (act, _) = cache.get(&acc, &key).unwrap_or_default();
            if act != val {
                violations.push(format!(
                    "REVM: account {acc} storage [{key}] mismatch:\n  want {val}\n  have {act}"
                ));
            }
        }
    }
    new_drift
}

pub struct RevmResult {
    pub call: CallResult,
    pub state: Env,
    pub millis: f64,
}

// TODO: run embedded database for acc/state storage
// consider: sqlite, leveldb, rocksdb, sled, yakvdb?

mod live {
    use alloy_eip7702::{Authorization, SignedAuthorization};
    use alloy_primitives::map::FbBuildHasher;
    use alloy_primitives::{Address as AlloyAddress, U256 as AlloyU256};
    use alloy_provider::Provider;
    use revm::bytecode::opcode::OpCode;
    use revm::context::result::{ExecutionResult, HaltReason, Output};
    use revm::context::transaction::{AccessList, AccessListItem};
    use revm::context::{ContextTr, TxEnv};
    use revm::context_interface::result::ExecResultAndState;
    use revm::database::{AlloyDB, BlockId, CacheDB, WrapDatabaseAsync};
    use revm::interpreter::interpreter_types::{Immediates, Jumps};
    use revm::interpreter::{CallInputs, CallOutcome, CreateInputs, CreateOutcome};
    use revm::interpreter::{Interpreter, interpreter::EthInterpreter};
    use revm::primitives::{Address, B256, Bytes, TxKind, U256};
    use revm::{Context, ExecuteCommitEvm, InspectEvm, Inspector, MainBuilder, MainContext};

    use tokio::sync::mpsc;
    use yevm_base::{Acc, Int};
    use yevm_core::call::TxFull;
    use yevm_core::evm::Gas;
    use yevm_core::state::Account;
    use yevm_core::trace::Step;
    use yevm_core::{Call, Head, Tx};
    use yevm_misc::buf::Buf;

    use crate::RevmResult;

    fn signed_authorizations(tx: &Tx) -> Vec<SignedAuthorization> {
        tx.authorization_list
            .iter()
            .map(|item| {
                SignedAuthorization::new_unchecked(
                    Authorization {
                        chain_id: AlloyU256::from_be_bytes(
                            <[u8; 32]>::try_from(item.chain_id.as_ref()).unwrap(),
                        ),
                        address: AlloyAddress::from_slice(item.address.as_ref()),
                        nonce: item.nonce.as_u64(),
                    },
                    item.y_parity.as_u8(),
                    AlloyU256::from_be_bytes(<[u8; 32]>::try_from(item.r.as_ref()).unwrap()),
                    AlloyU256::from_be_bytes(<[u8; 32]>::try_from(item.s.as_ref()).unwrap()),
                )
            })
            .collect()
    }

    #[derive(Debug, Default)]
    pub struct Tracer {
        step: Option<Step>,
        refund: i64,
        gas: u64,
        depth: usize,
        tx: Option<mpsc::Sender<Step>>,
    }

    impl<CTX: ContextTr> Inspector<CTX, EthInterpreter> for Tracer {
        fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _ctx: &mut CTX) {
            let pc = interp.bytecode.pc();
            let op = interp.bytecode.opcode();
            let name = OpCode::new(op)
                .map(|op| op.as_str())
                .unwrap_or("INVALID")
                .to_owned();
            let data = if (0x60..=0x7f).contains(&op) {
                let n = (op - 0x60 + 1) as usize;
                let raw = interp.bytecode.read_slice(n + 1);
                Some(Buf(raw[1..].to_vec()))
            } else {
                None
            };

            let gas = interp.gas.remaining();
            let stack = interp.stack.len();
            let memory = interp.memory.len();
            self.step = Some(Step {
                pc,
                op,
                name,
                data,
                gas,
                stack,
                memory,
                debug: vec![],
            });
            self.gas = gas;

            if op == 0x55
                && let (Ok(key), Ok(val)) = (interp.stack.peek(0), interp.stack.peek(1))
                && let Some(step) = self.step.as_mut()
            {
                step.debug.push(format!("SSTORE: key={key:0x}"));
                step.debug.push(format!("SSTORE: val={val:0x}"));
            }
        }

        fn step_end(&mut self, interp: &mut Interpreter<EthInterpreter>, _ctx: &mut CTX) {
            let gas = interp.gas.remaining();
            let cost = self.gas - gas;

            let refund = interp.gas.refunded() - self.refund;
            self.refund = interp.gas.refunded();

            if let Some(mut step) = self.step.take() {
                step.gas = gas;
                step.stack = interp.stack.len();
                step.memory = interp.memory.len();
                step.debug.push(format!("cost={cost}"));
                step.debug.push(format!("gas_refund={}", self.refund));
                if refund > 0 {
                    step.debug.push(format!("refund={refund}"));
                }
                step.debug.push(format!("depth={}", self.depth));

                if step.name == "SSTORE"
                    && let (Ok(key), Ok(val)) = (interp.stack.peek(0), interp.stack.peek(1))
                {
                    step.debug.push(format!("SSTORE: key={key:?}"));
                    step.debug.push(format!("SSTORE: val={val:?}"));
                } else if step.name == "CALLER" {
                    let caller = interp.stack.peek(0).unwrap_or_default();
                    step.debug.push(format!("CALLER: {caller:0x}"));
                } else if step.name == "BALANCE" {
                    let balance = interp.stack.peek(0).unwrap_or_default();
                    step.debug.push(format!("BALANCE: {balance:0x}"));
                }

                if let Some(tx) = self.tx.as_ref()
                    && tx.blocking_send(step).is_err()
                {
                    self.tx = None;
                }
            }
        }

        fn call(&mut self, _: &mut CTX, _: &mut CallInputs) -> Option<CallOutcome> {
            self.depth += 1;
            None
        }

        fn call_end(&mut self, _: &mut CTX, _: &CallInputs, _: &mut CallOutcome) {
            self.depth -= 1;
        }

        fn create(&mut self, _: &mut CTX, _: &mut CreateInputs) -> Option<CreateOutcome> {
            self.depth += 1;
            None
        }

        fn create_end(&mut self, _: &mut CTX, _: &CreateInputs, _: &mut CreateOutcome) {
            self.depth -= 1;
        }

        fn selfdestruct(&mut self, _: Address, _: Address, _: U256) {
            self.depth -= 1;
        }
    }

    pub fn run_all(
        chain_id: u64,
        txs: &[TxFull],
        head: Head,
        sender: mpsc::Sender<Step>,
        result_sender: mpsc::Sender<RevmResult>,
        provider: impl Provider + Clone,
    ) -> eyre::Result<()> {
        let to_addr = |a: &Acc| Address::from(<[u8; 20]>::try_from(a.as_ref()).unwrap());
        let to_u256 = |i: &Int| U256::from_be_bytes(<[u8; 32]>::try_from(i.as_ref()).unwrap());
        let to_b256 = |i: &Int| B256::from(<[u8; 32]>::try_from(i.as_ref()).unwrap());

        let db = AlloyDB::new(provider, BlockId::from(to_b256(&head.parent_hash)));
        let db = WrapDatabaseAsync::new(db).unwrap();
        let mut db = CacheDB::new(db);

        if let Some(root) = head.parent_beacon_block_root {
            let beacon_roots =
                alloy_primitives::address!("000f3df6d732807ef1319fb7b8bb8522d0beac02");
            let timestamp = to_u256(&head.timestamp).to::<u64>();
            let slot = U256::from(timestamp % 8191);
            db.insert_account_storage(beacon_roots, slot, U256::from(timestamp))
                .map_err(|e| eyre::eyre!("{e:?}"))?;
            db.insert_account_storage(
                beacon_roots,
                slot + U256::from(8191u64),
                U256::from_be_bytes(to_b256(&root).0),
            )
            .map_err(|e| eyre::eyre!("{e:?}"))?;
        }

        let mut ctx = Context::mainnet().with_db(db);
        ctx.block.number = U256::from(head.number.as_u64());
        ctx.block.timestamp = to_u256(&head.timestamp);
        ctx.block.gas_limit = head.gas_limit.as_u64();
        ctx.block.beneficiary = to_addr(&head.coinbase);
        ctx.block.basefee = head.base_fee.as_u64();
        ctx.block.prevrandao = Some(to_b256(&head.prevrandao));
        ctx.cfg.chain_id = chain_id;
        // Update fraction is fork-scheduled (EIP-7892 BPO forks bump it),
        // not a fixed protocol constant -- hardcoded here to BPO2's value
        // (11_684_671), matching yevm_core::call::blob_base_fee's own
        // hardcode. Correct for blocks mined under BPO2, wrong again once
        // the next BPO fork lands.
        if let Some(excess) = head.excess_blob_gas {
            let fraction = 11_684_671u64;
            ctx.block
                .set_blob_excess_gas_and_price(excess.as_u64(), fraction);
        }

        // let fork = revm::primitives::hardfork::SpecId::OSAKA;
        // ctx.cfg.set_spec_and_mainnet_gas_params(fork);

        let inspector = Tracer {
            tx: Some(sender),
            ..Tracer::default()
        };
        let mut evm = ctx.build_mainnet_with_inspector(inspector);

        for tx in txs {
            let (tx, call): (Tx, Call) = (tx.tx.clone(), tx.call.clone().into());
            // For legacy tx (max_fee_per_gas=0), use gas_price for effective fee
            let max_fee = if tx.max_fee_per_gas.is_zero() {
                tx.gas_price.as_u128()
            } else {
                tx.max_fee_per_gas.as_u128()
            };
            let priority_fee = if tx.max_fee_per_gas.is_zero() {
                tx.gas_price.as_u128()
            } else {
                tx.max_priority_fee_per_gas.as_u128()
            };

            let kind = if let Some(to) = call.to {
                TxKind::Call(to_addr(&to))
            } else {
                TxKind::Create
            };
            let tx = TxEnv::builder()
                .caller(to_addr(&call.by))
                .kind(kind)
                .gas_limit(call.gas)
                .gas_price(tx.gas_price.as_u128())
                .value(to_u256(&call.eth))
                .data(Bytes::from(call.data.0.clone()))
                .nonce(tx.nonce.as_u64())
                .access_list(AccessList::from(
                    tx.access_list
                        .iter()
                        .map(|item| AccessListItem {
                            address: to_addr(&item.address),
                            storage_keys: item
                                .storage_keys
                                .iter()
                                .map(to_b256)
                                .collect::<Vec<B256>>(),
                        })
                        .collect::<Vec<AccessListItem>>(),
                ))
                .max_fee_per_gas(max_fee)
                .gas_priority_fee(Some(priority_fee))
                .authorization_list_signed(signed_authorizations(&tx))
                .blob_hashes(
                    tx.blob_versioned_hashes
                        .iter()
                        .map(to_b256)
                        .collect::<Vec<B256>>(),
                )
                .max_fee_per_blob_gas(tx.max_fee_per_blob_gas.unwrap_or_default().as_u128())
                .build()
                .map_err(|e| eyre::eyre!("{e:?}"))?;

            let ms = std::time::Instant::now();
            let ExecResultAndState { result, state } = evm.inspect_tx(tx)?;
            evm.commit(state.clone());
            let ms = ms.elapsed().as_micros() as f64 / 1_000.0;

            let revm_result = to_revm_result(result, state, ms);
            result_sender.blocking_send(revm_result)?;
        }
        let _ = evm.inspector.tx.take();
        Ok(())
    }

    pub fn run_one(
        call: Call,
        tx: Tx,
        head: Head,
        network_chain_id: u64,
        sender: mpsc::Sender<Step>,
        result_sender: mpsc::Sender<RevmResult>,
        provider: impl Provider + Clone,
    ) -> eyre::Result<()> {
        let to_addr = |a: &Acc| Address::from(<[u8; 20]>::try_from(a.as_ref()).unwrap());
        let to_u256 = |i: &Int| U256::from_be_bytes(<[u8; 32]>::try_from(i.as_ref()).unwrap());
        let to_b256 = |i: &Int| B256::from(<[u8; 32]>::try_from(i.as_ref()).unwrap());

        let db = AlloyDB::new(provider, BlockId::from(to_b256(&head.parent_hash)));
        let db = WrapDatabaseAsync::new(db).unwrap();
        let mut db = CacheDB::new(db);

        if let Some(root) = head.parent_beacon_block_root {
            let beacon_roots =
                alloy_primitives::address!("000f3df6d732807ef1319fb7b8bb8522d0beac02");
            let timestamp = to_u256(&head.timestamp).to::<u64>();
            let slot = U256::from(timestamp % 8191);
            db.insert_account_storage(beacon_roots, slot, U256::from(timestamp))
                .map_err(|e| eyre::eyre!("{e:?}"))?;
            db.insert_account_storage(
                beacon_roots,
                slot + U256::from(8191u64),
                U256::from_be_bytes(to_b256(&root).0),
            )
            .map_err(|e| eyre::eyre!("{e:?}"))?;
        }

        let mut ctx = Context::mainnet().with_db(db);
        ctx.block.number = U256::from(head.number.as_u64());
        ctx.block.timestamp = to_u256(&head.timestamp);
        ctx.block.gas_limit = head.gas_limit.as_u64();
        ctx.block.beneficiary = to_addr(&head.coinbase);
        ctx.block.basefee = head.base_fee.as_u64();
        ctx.block.prevrandao = Some(to_b256(&head.prevrandao));
        ctx.cfg.chain_id = if tx.chain_id.is_zero() {
            network_chain_id
        } else {
            tx.chain_id.as_u64()
        };

        // For legacy tx (max_fee_per_gas=0), use gas_price for effective fee
        let max_fee = if tx.max_fee_per_gas.is_zero() {
            tx.gas_price.as_u128()
        } else {
            tx.max_fee_per_gas.as_u128()
        };
        let priority_fee = if tx.max_fee_per_gas.is_zero() {
            tx.gas_price.as_u128()
        } else {
            tx.max_priority_fee_per_gas.as_u128()
        };

        let kind = if let Some(to) = call.to {
            TxKind::Call(to_addr(&to))
        } else {
            TxKind::Create
        };
        let tx_env = TxEnv::builder()
            .caller(to_addr(&call.by))
            .kind(kind)
            .gas_limit(call.gas)
            .gas_price(tx.gas_price.as_u128())
            .value(to_u256(&call.eth))
            .data(Bytes::from(call.data.0.clone()))
            .nonce(tx.nonce.as_u64())
            .access_list(AccessList::from(
                tx.access_list
                    .iter()
                    .map(|item| AccessListItem {
                        address: to_addr(&item.address),
                        storage_keys: item.storage_keys.iter().map(to_b256).collect::<Vec<B256>>(),
                    })
                    .collect::<Vec<AccessListItem>>(),
            ))
            .max_fee_per_gas(max_fee)
            .gas_priority_fee(Some(priority_fee))
            .authorization_list_signed(signed_authorizations(&tx))
            .blob_hashes(
                tx.blob_versioned_hashes
                    .iter()
                    .map(to_b256)
                    .collect::<Vec<B256>>(),
            )
            .max_fee_per_blob_gas(tx.max_fee_per_blob_gas.unwrap_or_default().as_u128())
            .build()
            .map_err(|e| eyre::eyre!("{e:?}"))?;

        let inspector = Tracer {
            tx: Some(sender),
            ..Tracer::default()
        };
        let mut evm = ctx.build_mainnet_with_inspector(inspector);

        let ms = std::time::Instant::now();
        let ExecResultAndState { result, state } = evm.inspect_tx(tx_env)?;
        let ms = ms.elapsed().as_micros() as f64 / 1_000.0;

        let revm_result = to_revm_result(result, state, ms);
        result_sender.blocking_send(revm_result)?;
        let _ = evm.inspector.tx.take();
        Ok(())
    }

    fn to_revm_result(
        result: ExecutionResult<HaltReason>,
        state: revm::primitives::HashMap<Address, revm::state::Account, FbBuildHasher<20>>,
        millis: f64,
    ) -> RevmResult {
        RevmResult {
            call: match result {
                ExecutionResult::Success {
                    reason: _,
                    gas,
                    logs: _,
                    output: Output::Call(ret),
                } => yevm_core::exe::CallResult::Done {
                    status: Int::ONE,
                    ret: ret.to_vec().into(),
                    gas: Gas {
                        limit: 0,
                        spent: 0,
                        refund: 0,
                        finalized: gas.used() as i64,
                    },
                },
                ExecutionResult::Success {
                    reason: _,
                    gas,
                    logs: _,
                    output: Output::Create(code, Some(address)),
                } => yevm_core::exe::CallResult::Created {
                    acc: Acc::from(address.as_slice()),
                    code: code.to_vec().into(),
                    gas: Gas {
                        limit: 0,
                        spent: 0,
                        refund: 0,
                        finalized: gas.used() as i64,
                    },
                },
                ExecutionResult::Success {
                    reason: _,
                    gas,
                    logs: _,
                    output: Output::Create(code, None),
                } => yevm_core::exe::CallResult::Created {
                    acc: Acc::ZERO,
                    code: code.to_vec().into(),
                    gas: Gas {
                        limit: 0,
                        spent: 0,
                        refund: 0,
                        finalized: gas.used() as i64,
                    },
                },
                ExecutionResult::Revert {
                    gas,
                    logs: _,
                    output: ret,
                } => yevm_core::exe::CallResult::Done {
                    status: Int::ZERO,
                    ret: ret.to_vec().into(),
                    gas: Gas {
                        limit: 0,
                        spent: 0,
                        refund: 0,
                        finalized: gas.used() as i64,
                    },
                },
                ExecutionResult::Halt {
                    reason: _,
                    gas,
                    logs: _,
                } => yevm_core::exe::CallResult::Done {
                    status: Int::ZERO,
                    ret: vec![].into(),
                    gas: Gas {
                        limit: 0,
                        spent: 0,
                        refund: 0,
                        finalized: gas.used() as i64,
                    },
                },
            },
            state: state
                .into_iter()
                .filter(|(_, account)| !account.is_selfdestructed())
                .map(|(address, account)| {
                    let storage = account
                        .storage
                        .into_iter()
                        .map(|(slot, value)| {
                            (
                                Int::from(slot.to_be_bytes::<32>().as_slice()),
                                Int::from(value.present_value.to_be_bytes::<32>().as_slice()),
                            )
                        })
                        .collect();
                    let bytecode = account.info.code.unwrap_or_default();
                    let code = if bytecode.is_empty() {
                        Buf::default()
                    } else {
                        bytecode.original_byte_slice().to_vec().into()
                    };
                    let account = Account {
                        value: Int::from(account.info.balance.to_be_bytes::<32>().as_slice()),
                        nonce: account.info.nonce.into(),
                        code: (code, Int::ZERO),
                    };
                    let acc = Acc::from(address.as_slice());
                    (acc, account, storage)
                })
                .collect(),
            millis,
        }
    }
}
