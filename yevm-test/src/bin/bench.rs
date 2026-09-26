use std::{
    fs::File,
    io::{BufReader, Read},
    path::Path,
    time::Instant,
};

use yevm_core::{
    cache::Cache,
    call::{Block, TxFull},
    chain::Fetched,
    exe::{Executor, post_block, pre_block},
    rpc::Rpc,
    state::State,
};

const BLOCK: u64 = 24929490;
const ITERS: usize = 10;

fn args(block: u64, iters: usize) -> eyre::Result<(Option<u64>, usize)> {
    let mut args = std::env::args();
    let _ = args.next();
    let first = args.next();
    if first.as_deref() == Some("all") {
        return Ok((None, iters));
    }
    let block = match first {
        Some(s) => s.parse().map_err(|_| eyre::eyre!("invalid block: {s}"))?,
        None => block,
    };
    let iters = match args.next() {
        Some(s) => s.parse().map_err(|_| eyre::eyre!("invalid iters: {s}"))?,
        None => iters,
    };
    Ok((Some(block), iters))
}

fn fetch_blocks() -> eyre::Result<Vec<u64>> {
    let mut blocks = Vec::new();
    for entry in std::fs::read_dir("fetch")? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if let Ok(n) = stem.parse::<u64>() {
            blocks.push(n);
        }
    }
    blocks.sort_unstable();
    Ok(blocks)
}

fn load(block: u64) -> eyre::Result<(Block, u64, Vec<Fetched>)> {
    let path = format!("fetch/{}.json", block);
    let fetches = Path::new(&path);
    if !fetches.exists() {
        eyre::bail!("No saved fetches found: {path}");
    }
    let file = File::open(fetches)?;
    let mut reader = BufReader::new(file);
    let mut content = String::new();
    reader.read_to_string(&mut content)?;
    let fetched: Vec<Fetched> = serde_json::from_str(&content)?;
    let Some(Fetched::ChainId(chain_id)) = fetched.first().cloned() else {
        eyre::bail!("Cannot find fetched chain id");
    };
    let Some(Fetched::Block(block)) = fetched.get(1).cloned() else {
        eyre::bail!("Cannot find fetched block");
    };
    Ok((block, chain_id, fetched))
}

async fn bench_block(
    rpc: &Rpc,
    block: &Block,
    chain_id: u64,
    fetched: &[Fetched],
    iters: usize,
) -> eyre::Result<()> {
    let (num, txs) = (block.head.number.as_u64(), block.txs.len());
    println!("bench: block={num} txs={txs} iters={iters}",);

    let head = block.head.clone();
    for i in 0..iters {
        let mut cache = Cache::new();
        cache.set_chain_id(chain_id);
        cache.prefetched(fetched.to_vec());

        let now = Instant::now();
        pre_block(&head, &mut cache, rpc).await?;
        let mut gas = 0;
        for tx in &block.txs {
            let TxFull { tx, call } = tx;
            let mut exe = Executor::new(call.clone().into());
            cache.reset();
            let res = exe.run(&tx, &head, &mut cache, rpc).await?;
            gas += res.gas().spent.max(0) as u64;
        }
        post_block(&block.withdrawals, &mut cache, rpc).await?;
        let t = now.elapsed().as_micros() as f64 / 1_000_000.0;
        let n = block.txs.len() as f64 / t;
        let g = gas as f64 / t;
        println!("I={i:03} T={t:8.6} Txn/s={n:6.2} Gas/s={g:8.2}");
    }
    Ok(())
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let rpc = Rpc::offline();
    let (block, iters) = args(BLOCK, ITERS)?;
    let blocks = match block {
        Some(n) => vec![n],
        None => {
            let blocks = fetch_blocks()?;
            if blocks.is_empty() {
                eyre::bail!("No saved fetches found in fetch/");
            }
            println!("bench: all {} blocks, iters={iters}", blocks.len());
            blocks
        }
    };

    for n in blocks {
        let (block, chain_id, fetched) = load(n)?;
        bench_block(&rpc, &block, chain_id, &fetched, iters).await?;
    }
    Ok(())
}
