//! Cross-check `RethDb::get` (the actual `Chain` read path) against a plain
//! `eth_getStorageAt` over the node's own RPC, for one address/slot.
//!
//! Usage:
//!   RETH_DATADIR=... YEVM_RPC_URL=http://127.0.0.1:8545/ \
//!     cargo run -p yevm-reth --bin verify -- 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2 2

use yevm_base::{Acc, Int, int};
use yevm_core::{chain::Chain, rpc::Rpc};
use yevm_reth::RethDb;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let mut args = std::env::args().skip(1);
    let addr = args
        .next()
        .ok_or_else(|| eyre::eyre!("usage: verify <address> <slot>"))?;
    let slot = args
        .next()
        .ok_or_else(|| eyre::eyre!("usage: verify <address> <slot>"))?;

    let acc: Acc = addr
        .as_str()
        .try_into()
        .map_err(|_| eyre::eyre!("bad address"))?;
    let key: Int = int(&slot);

    let datadir = std::env::var("RETH_DATADIR")?;
    let rpc_url = std::env::var("YEVM_RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:8545/".into());

    // `RethDb` pins state at its state frontier (not the persisted tip, see
    // `RethDb::state_frontier`), so compare RPC at that same block.
    let db = RethDb::latest(datadir)?;
    let frontier = db.state_frontier()?;
    let rpc = Rpc::number(rpc_url.clone(), frontier).await?;
    let rpc_tip = Rpc::latest(rpc_url).await?.block_number;

    let via_db = db.get(&acc, &key).await?;
    let via_rpc = rpc.get(&acc, &key).await?;

    println!("addr:   {addr}");
    println!("slot:   {slot}");
    println!("db:     {via_db}");
    println!("rpc:    {via_rpc}");
    println!("match:  {}", via_db == via_rpc);

    let db_acc = db.acc(&acc).await?;
    let rpc_acc = rpc.acc(&acc).await?;
    println!("---");
    println!(
        "db  balance/nonce/code_len: {} / {} / {}",
        db_acc.value,
        db_acc.nonce,
        db_acc.code.0.len()
    );
    println!(
        "rpc balance/nonce/code_len: {} / {} / {}",
        rpc_acc.value,
        rpc_acc.nonce,
        rpc_acc.code.0.len()
    );
    println!("acc match: {}", db_acc == rpc_acc);

    let db_tip = db.best_block_number()?;
    println!("---");
    println!("db state frontier: {frontier} (state compared at this block)");
    println!("db persisted tip:  {db_tip}");
    println!("rpc reported tip:  {rpc_tip}");
    println!("lag: {}", rpc_tip.saturating_sub(db_tip));

    // A few blocks behind the DB's own persisted tip (not RPC's), so both
    // backends definitely see the exact same, already-finalized block.
    let number = db_tip.saturating_sub(5);
    let db_head = db.head(number).await?;
    let rpc_head = rpc.head(number).await?;
    println!("---");
    println!("block: {number}");
    println!(
        "db  head: num={} hash={} parent={} gas_limit={} coinbase={} base_fee={} timestamp={}",
        db_head.number.as_u64(),
        db_head.hash,
        db_head.parent_hash,
        db_head.gas_limit,
        db_head.coinbase,
        db_head.base_fee,
        db_head.timestamp,
    );
    println!(
        "rpc head: num={} hash={} parent={} gas_limit={} coinbase={} base_fee={} timestamp={}",
        rpc_head.number.as_u64(),
        rpc_head.hash,
        rpc_head.parent_hash,
        rpc_head.gas_limit,
        rpc_head.coinbase,
        rpc_head.base_fee,
        rpc_head.timestamp,
    );
    println!(
        "head fields match: {}",
        db_head.number == rpc_head.number
            && db_head.hash == rpc_head.hash
            && db_head.parent_hash == rpc_head.parent_hash
            && db_head.gas_limit == rpc_head.gas_limit
            && db_head.coinbase == rpc_head.coinbase
            && db_head.base_fee == rpc_head.base_fee
            && db_head.timestamp == rpc_head.timestamp
            && db_head.prevrandao == rpc_head.prevrandao
            && db_head.parent_beacon_block_root == rpc_head.parent_beacon_block_root
    );

    let db_block = db.block(number).await?;
    let rpc_block = rpc.block(number).await?;
    println!("---");
    println!("db  block: {} txs", db_block.txs.len());
    println!("rpc block: {} txs", rpc_block.txs.len());
    let mut all_match = db_block.txs.len() == rpc_block.txs.len();
    for (i, (a, b)) in db_block.txs.iter().zip(rpc_block.txs.iter()).enumerate() {
        // `gas_price` only means what it says for legacy/2930 txs; for
        // dynamic-fee types (1559/4844/7702) RPC reports an *effective*
        // display value there that yevm's own execution never reads once
        // `max_fee_per_gas` is nonzero (see `exe::intrinsic`, and
        // `yevm-gate/src/tx.rs`'s identical convention) -- so it's not
        // part of the correctness check for those.
        let gas_price_relevant = a.tx.max_fee_per_gas.is_zero();
        let same = a.tx.hash == b.tx.hash
            && a.call.from == b.call.from
            && a.call.to == b.call.to
            && a.call.value == b.call.value
            && a.tx.nonce == b.tx.nonce
            && (!gas_price_relevant || a.tx.gas_price == b.tx.gas_price)
            && a.tx.max_fee_per_gas == b.tx.max_fee_per_gas
            && a.tx.max_priority_fee_per_gas == b.tx.max_priority_fee_per_gas
            && a.tx.access_list.len() == b.tx.access_list.len()
            // `AuthorizationListItem` has no `PartialEq`; compare via Debug.
            && format!("{:?}", a.tx.authorization_list) == format!("{:?}", b.tx.authorization_list)
            && a.tx.blob_versioned_hashes == b.tx.blob_versioned_hashes
            && a.tx.max_fee_per_blob_gas == b.tx.max_fee_per_blob_gas;
        if !same {
            all_match = false;
            println!("tx[{i}] MISMATCH\n  db: {a:#?}\n  rpc: {b:#?}");
        }
    }
    println!("all {} txs match: {all_match}", db_block.txs.len());

    Ok(())
}
