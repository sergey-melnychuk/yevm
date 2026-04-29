use std::{
    fs::File,
    io::{BufReader, Read},
    path::Path,
    time::Instant,
};

use yaevmi_core::{cache::Cache, chain::Fetched, exe::{Executor, pre_block}, rpc::Rpc, state::State};

const BLOCK: u64 = 24929490;
const ITERS: usize = 1000;

fn args(block: u64, iters: usize) -> eyre::Result<(u64, usize)> {
    let mut args = std::env::args();
    let _ = args.next();
    let block = match args.next() {
        Some(s) => s.parse().map_err(|_| eyre::eyre!("invalid block: {s}"))?,
        None => block,
    };
    let iters = match args.next() {
        Some(s) => s.parse().map_err(|_| eyre::eyre!("invalid iters: {s}"))?,
        None => iters,
    };
    Ok((block, iters))
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let rpc = Rpc::offline();

    let (block, iters) = args(BLOCK, ITERS)?;
    println!("bench: block={block} iters={iters}");

    let path = format!("fetch/{}.json", block);
    let fetches = Path::new(&path);
    if !fetches.exists() {
        eyre::bail!("No saved fetches found: {path}");
    }
    let (block, chain_id, fetched) = {
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
        (block, chain_id, fetched)
    };

    let head = block.head.clone();
    for i in 0..iters {
        let mut cache = Cache::new();
        cache.set_chain_id(chain_id);
        cache.prefetched(fetched.clone());

        let now = Instant::now();
        pre_block(&head, &mut cache, &rpc).await?;
        for tx in &block.txs {
            let (tx, call) = (tx.tx.clone(), tx.call.clone().into());
            let mut exe = Executor::new(call);
            cache.reset();
            let _ = exe.run(tx, head.clone(), &mut cache, &rpc).await?;
        }
        let ms = now.elapsed().as_millis() as u64;
        println!("I={i:03} T={:6.4}", ms as f64 / 1000.0);
    }
    Ok(())
}
