use std::{
    collections::BTreeMap,
    env::args,
    io::Write,
    sync::Arc,
    time::{Duration, Instant},
};

use futures::{SinkExt as _, channel::mpsc};

use tokio::sync::Notify;
use yevm::{
    Acc, Call, Event, State,
    base::acc,
    core::{
        cache::Cache,
        call::{Block, TxFull},
        chain::Chain,
        evm::CallMode,
        exe::{Executor, post_block, pre_block},
        rpc::Rpc,
    },
    lens,
    misc::hex::parse,
    trace::filter,
};

const YEVM_RPC_URL: &str = "YEVM_RPC_URL";

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

// https://github.com/lambdaclass/propamm-router-contracts#deployed-contracts
const ROUTER: Acc = acc("0x4ddf368080cd7946db5b459ad591c350158175e1");
const METRIC: Acc = acc("0xE715Dc29d2c273D0FC5A03e5Cca9CcB0Abb1dCDB");
const BEBOP: Acc = acc("0xB09AaA5614916d7AEb59C295C52c92ca82aDdD76");
const FERMI: Acc = acc("0x5979458912f80b96d30d4220af8e2e4925a33320");
const KIPSELI: Acc = acc("0x71e790dd841c8a9061487cb3e78c288e75ce0b3d");
const TEMPEST: Acc = acc("0x00000003f1ec2379e79F58E12EC6C4F51Ee92149");
const TAURUS: Acc = acc("0x217d58931A8549ca539426AA8152E33dAfc3d95A");
const ZORRO: Acc = acc("0xCF211B4dD0D2be5C173Ea57Bcf938FC61d1d3bd3");
const UNKNOWN: Acc = acc("0x9d40cfec47b60b8ebdb9eaf5a2bb1b41eef9002f");

// Titan's central pAMM quote store ("da7afeed" = data feed). Makers' signed
// quote-update txs call it directly: it verifies the maker signature and
// stores the price levels in ITS OWN storage (no token movement, no logs).
// During a swap the amm STATICCALLs it for the live quote, so quote tracking
// keys off this contract's storage, not the amms'. The builder places the
// latest quote update immediately before the taker tx in the same block.
// https://docs.titanbuilder.xyz/propamms
const QUOTES: Acc = acc("0xda7afeed021eafc1c1af9c362de477dad0396b81");

// A second quote store, maker-run (serves e.g. the 0x585d44.. inventory wallet
// behind Fermi fills); same pattern: 2-slot writes right before the taker tx.
// The tracker does not need stores listed -- it indexes all storage writes --
// but naming it here captures its update txs so they print as they land.
const QUOTES2: Acc = acc("0x0109aa912b58508886a2a707204b0f8c8b164ccc");

// pAMM pools observed filling via the quote store but not (yet) listed in the
// router repo's deployed-contracts table.
const POOL_B099: Acc = acc("0xb09999a44e3240193193be9dd99cc73c8cc945a6");
const POOL_C9A9: Acc = acc("0xc9a956b5196dfe110debe8781857bc8b97d32091");

// swap(address,address,uint256,uint256,address,uint256)
const SWAP: [u8; 4] = parse("0x9908fc8b");
// quote(address,address,uint256)
const QUOTE: [u8; 4] = parse("0xb6466384");

// const UPDATE: [u8; 4] = parse("0xe50de8ea");

const WETH: Acc = acc("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
const USDC: Acc = acc("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
const USDT: Acc = acc("0xdac17f958d2ee523a2206206994597c13d831ec7");

const EUREKA: Acc = acc("0xfb74767c1ce1aada0a0e114441173b57f8c1571b");
const TITAN: Acc = acc("0x4838B106FCe9647Bdf1E7877BF73cE8B0BAD5f97");

const DELAY: Duration = Duration::from_secs(60);
const PROBE: Duration = Duration::from_secs(6);

async fn run() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    let url = std::env::var(YEVM_RPC_URL)?;

    let lookup: BTreeMap<Acc, &'static str> = [
        (ROUTER, "amm:ROUTER"),
        (METRIC, "amm:Metric"),
        (BEBOP, "amm:Bebop"),
        (FERMI, "amm:Fermi"),
        (KIPSELI, "amm:Kipseli"),
        (TEMPEST, "amm:Tempest"),
        (TAURUS, "amm:Taurus"),
        (ZORRO, "amm:Zorro"),
        (UNKNOWN, "amm:unknown"),
        (POOL_B099, "amm:pool-b099"),
        (POOL_C9A9, "amm:pool-c9a9"),
        (QUOTES, "feed:Quotes"),
        (QUOTES2, "feed:Quotes2"),
        (WETH, "erc20:WETH"),
        (USDC, "erc20:USDC"),
        (USDT, "erc20:USDT"),
        (TITAN, "builder:Titan"),
        (EUREKA, "builder:Eureka"),
    ]
    .into();

    let mut rpc = Rpc::latest(url.clone()).await?;
    let chain_id = rpc.chain_id().await?;

    let mut block = if let Some(arg) = args().nth(1) {
        let number = arg.parse::<u64>()?;
        rpc.block(number).await?
    } else {
        rpc.block(rpc.block_number).await?
    };

    let (yevm_tx, mut yevm_rx) = mpsc::channel(1 << 20);
    let filter = yevm::lens::FILTER | filter::TAG;
    let mut cache = Cache::with_sender(yevm_tx, filter);
    cache.set_chain_id(chain_id);

    let (mut lens_tx, mut lens_rx) = mpsc::channel(1 << 20);

    let done = Arc::new(Notify::new());
    let names = lookup.clone();

    let done_copy = Arc::clone(&done);
    tokio::spawn(async move {
        // TODO: detect reverted swaps (add REVERT to the filter to receive revert events)
        let (mut block, mut index, mut hash) = Default::default();
        let mut callstack = Vec::with_capacity(16);

        let mut current = Vec::new();
        let mut capture = false;

        while let Ok(trace) = yevm_rx.recv().await {
            match &trace.event {
                Event::Call(
                    Call { to, data, .. },
                    CallMode::Call(_, _) | CallMode::Static(_, _),
                ) => {
                    let is_amm = to
                        .and_then(|to| lookup.get(&to))
                        .map(|tag| tag.starts_with("amm:") || tag.starts_with("feed:"))
                        .unwrap_or_default();
                    let is_swap = data.0.starts_with(&SWAP);
                    let is_quote = data.0.starts_with(&QUOTE);
                    if is_amm || is_swap || is_quote {
                        capture = true;
                        // println!("\r\x1b[2K");
                        // println!("block={block} index={index} hash={hash:?}");
                        // println!("{:#?}", trace);
                        callstack.push(trace.clone());
                    }
                }
                Event::Return(_, _) | Event::Revert(_, _) | Event::Halt(_, _)
                    if callstack
                        .last()
                        .map(|t| t.depth == trace.depth)
                        .unwrap_or_default() =>
                {
                    // println!("\r\x1b[2K");
                    // println!("{:#?}", trace);
                    // TODO: format call & results (ABI decode, lookup table)
                    callstack.pop();
                }
                Event::Tag(b, i, h) => {
                    // Every tx is forwarded so the quote tracker can index its
                    // storage writes (a maker's quote store is not known in
                    // advance); `capture` marks the amm-related ones to print.
                    if !current.is_empty() {
                        let tag = (block, index, hash);
                        let traces = std::mem::take(&mut current);
                        if let Err(e) = lens_tx.send((tag, traces, capture)).await {
                            eprintln!("failed to send to lens: {tag:?}: {e:?}");
                        }
                    }
                    capture = false;

                    (block, index, hash) = (*b, *i, *h);
                    print!("\r\x1b[2K{block}:{index}");
                    std::io::stdout().flush().unwrap();
                    callstack.clear();
                }
                _ => (),
            }
            current.push(trace);
        }
        done_copy.notify_one();
    });

    // PropAMM quote tracking: a maker pushes a quote by writing a quote-store
    // contract's storage (its own feed, or Titan's shared one) in a tx that
    // moves no tokens; the swap right after it reads those exact slots. The
    // tracker indexes storage writes of EVERY tx -- stores are discovered by
    // the slot match, never listed in advance -- and old writes are evicted
    // (updates land in the same block as the swap, right before it). Reorged
    // writes are not evicted here -- demo only.
    tokio::spawn(async move {
        const RETAIN_BLOCKS: u64 = 64;
        let mut tracker = lens::quotes::QuoteTracker::all();
        let mut seen = (0u64, 0u64);
        let name = |acc: &Acc| names.get(acc).copied().unwrap_or("?");

        while let Ok(((block, index, hash), traces, captured)) = lens_rx.recv().await {
            // Going backwards in (block, index) means a block is being
            // re-executed: a reorg. Drop everything indexed from that block on
            // before observing it again, so a reorged quote update cannot be
            // linked. Detected here rather than signalled from the reorg path
            // so it cannot race the traces still in flight.
            if (block, index) <= seen {
                tracker.purge_from(block);
            }
            if block != seen.0 {
                tracker.evict_before(block.saturating_sub(RETAIN_BLOCKS));
            }
            seen = (block, index);

            let alerts = lens::analyse(&traces);
            let obs = tracker.observe(block, index, hash, &traces, &alerts);
            if !captured {
                continue;
            }
            // Nothing survived -- e.g. a reverted take attempt (captured
            // because it entered an amm, but all its state was undone).
            if alerts.swaps.is_empty() && obs.updates.is_empty() && obs.links.is_empty() {
                continue;
            }

            // propAMM venues actually ENTERED during the tx -- from Call
            // events (execution), never from logs, which any contract can
            // fake. Names the venue even when the amm moves no tokens itself
            // (the maker's inventory wallet does).
            let mut venues: Vec<&str> = Vec::new();
            for t in &traces {
                if t.reverted {
                    continue;
                }
                if let Event::Call(Call { to: Some(to), .. }, _) = &t.event
                    && let Some(n) = names.get(to)
                    && n.starts_with("amm:")
                    && !venues.contains(n)
                {
                    venues.push(n);
                }
            }

            println!("\r\x1b[2K");
            println!("block={block} index={index} hash={hash:?}");
            if !alerts.swaps.is_empty() {
                println!("{:#?}", alerts.swaps);
                if !venues.is_empty() {
                    println!("via: {}", venues.join(", "));
                }
            }
            for u in &obs.updates {
                println!(
                    "quote update: {} ({:?}) by maker {:?}, {} slots",
                    name(&u.amm),
                    u.amm,
                    u.maker,
                    u.slots,
                );
            }
            for l in &obs.links {
                println!(
                    "quote consumed: {} ({:?}) -- pushed by maker {:?} in block {} index {} ({} slots, {} blocks ago)",
                    name(&l.amm),
                    l.amm,
                    l.update.sender,
                    l.update.block,
                    l.update.index,
                    l.slots,
                    block.saturating_sub(l.update.block),
                );
            }
        }
    });

    let ret = loop {
        let tip = block.head.number.as_u64();
        rpc.reset(tip - 1, block.head.parent_hash);
        let Block {
            txs,
            head,
            withdrawals,
        } = block;

        let mut purge_required = false;
        if let Err(e) = pre_block(&head, &mut cache, &rpc).await {
            eprintln!("pre block failed for {tip}: {e:?}");
            purge_required = true;
        }
        for tx in txs {
            let TxFull { tx, call } = tx;
            let mut exe = Executor::new(call.into());
            if let Err(e) = exe.run(&tx, &head, &mut cache, &rpc).await {
                eprintln!("tx {:?} failed with {e:?}", tx.hash);
                purge_required = true;
            }
            cache.reset();
        }
        if let Err(e) = post_block(&withdrawals, &mut cache, &rpc).await {
            eprintln!("post block failed for {tip}: {e:?}");
            purge_required = true;
        }

        if purge_required {
            cache.purge();
        }

        let Ok(next) = next_tip(&mut rpc, tip, DELAY, PROBE).await else {
            break Err(eyre::eyre!("failed to pull next block: {tip}"));
        };
        if next.head.parent_hash != head.hash {
            // reorg
            cache.purge();
            match rpc.block(tip).await {
                Ok(b) => {
                    block = b;
                    continue;
                }
                Err(e) => break Err(e.wrap_err(format!("failed to pull reorged block: {tip}"))),
            }
        }
        block = next;
    };

    drop(cache.sender.take());
    done.notified().await;

    ret
}

async fn next_tip(
    rpc: &mut Rpc,
    tip: u64,
    delay: Duration,
    probe: Duration,
) -> eyre::Result<Block> {
    let now = Instant::now();
    loop {
        if let Ok(block) = rpc.block(tip + 1).await {
            return Ok(block);
        }
        if now.elapsed() > delay {
            eyre::bail!("block fetch timeout");
        }
        tokio::time::sleep(probe).await;
    }
}
