use std::{
    env::args,
    io::Write,
    sync::Arc,
    time::{Duration, Instant},
};

use futures::channel::mpsc;

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
        trace::filter,
    },
    misc::hex::parse,
};

const YEVM_RPC_URL: &str = "YEVM_RPC_URL";

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

const TARGET: Acc = acc("0xE715Dc29d2c273D0FC5A03e5Cca9CcB0Abb1dCDB");
// more:
// 0x9d40cfec47b60b8ebdb9eaf5a2bb1b41eef9002f
// 0x5979458912f80b96d30d4220af8e2e4925a33320

// swap(address,address,uint256,uint256,address,uint256)
const SELECT: [u8; 4] = parse("0x9908fc8b");

// 0xb6466384: quote(address,address,uint256)

// 0xc3251075: ??? (to: 0xe715dc29d2c273d0fc5a03e5cca9ccb0abb1dcdb)
// 0000000000000000000000000000000000000000000000e6cd51e4dcf00949ca
// fffffffffffffffffffffffffffffffffffffffffffffedf9bedd168ac5ae86e
// 0000000000000000000000000000000000000000000000000000000000000060
// 0000000000000000000000000000000000000000000000000000000000000000

const DELAY: Duration = Duration::from_secs(60);
const PROBE: Duration = Duration::from_secs(6);

async fn run() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    let url = std::env::var(YEVM_RPC_URL)?;
    let mut rpc = Rpc::latest(url.clone()).await?;
    let chain_id = rpc.chain_id().await?;

    let mut block = if let Some(arg) = args().nth(1) {
        let number = arg.parse::<u64>()?;
        rpc.block(number).await?
    } else {
        rpc.block(rpc.block_number).await?
    };

    let (yevm_tx, mut yevm_rx) = mpsc::channel(1 << 20);
    let mut cache = Cache::with_sender(yevm_tx, filter::CALL | filter::TAG);
    cache.set_chain_id(chain_id);

    let done = Arc::new(Notify::new());

    let done_copy = Arc::clone(&done);
    tokio::spawn(async move {
        // TODO: detect reverted swaps (add REVERT to the filter to receive revert events)
        let (mut block, mut index, mut hash) = Default::default();
        while let Ok(trace) = yevm_rx.recv().await {
            match &trace.event {
                Event::Call(
                    Call { to, data, .. },
                    CallMode::Call(_, _) | CallMode::Static(_, _),
                ) => {
                    let is_router = to.unwrap_or_default() == TARGET;
                    let is_swap = data.0.starts_with(&SELECT);
                    if is_router || is_swap {
                        println!("\r\x1b[2K");
                        println!("block={block} index={index} hash={hash:?}");
                        println!("{:#?}", trace);
                    }
                }
                Event::Tag(b, i, h) => {
                    (block, index, hash) = (*b, *i, *h);
                    print!("\r\x1b[2K{block}:{index}");
                    std::io::stdout().flush().unwrap();
                }
                _ => (),
            }
        }
        done_copy.notify_one();
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

        // static EUREKA: Acc = acc("0xfb74767c1ce1aada0a0e114441173b57f8c1571b");
        // static TITAN: Acc = acc("0x4838B106FCe9647Bdf1E7877BF73cE8B0BAD5f97");
        // if block.head.coinbase != TITAN {
        //     continue;
        // }
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
