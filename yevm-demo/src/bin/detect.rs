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
    Acc, Call, Event, State, base::acc, core::{
        cache::Cache,
        call::{Block, TxFull},
        chain::Chain,
        evm::CallMode,
        exe::{Executor, post_block, pre_block},
        rpc::Rpc,
    }, lens, misc::hex::parse, trace::filter,
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

// swap(address,address,uint256,uint256,address,uint256)
const SWAP: [u8; 4] = parse("0x9908fc8b");
// quote(address,address,uint256)
const QUOTE: [u8; 4] = parse("0xb6466384");

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
                        .map(|tag| tag.starts_with("amm:"))
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
                    if capture {
                        let tag = (block, index, hash);
                        let traces = std::mem::take(&mut current);
                        if let Err(e) = lens_tx.send((tag, traces)).await {
                            eprintln!("failed to send to lens: {tag:?}: {e:?}");
                        }
                    }
                    current.clear();
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

    tokio::spawn(async move {
        while let Ok(((block, index, hash), traces)) = lens_rx.recv().await {
            println!("\r\x1b[2K");
            println!("block={block} index={index} hash={hash:?}");

            let alerts = lens::analyse(&traces);
            println!("{:#?}", alerts.swaps);
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
