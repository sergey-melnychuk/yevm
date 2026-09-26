//! Hermetic Uniswap V2 swap against YEVM: buy WETH with USDC.
//!
//! What this does, and why it is "hermetic":
//!
//!   1. RECORD (online, once): connect to a public JSON-RPC node, execute the
//!      swap against real mainnet state, and record *every* piece of state the
//!      EVM touched (contract code, storage slots, accounts) into an ordered
//!      list. That list is saved to `fetch/uniswap2-usdc-weth.json`.
//!
//!   2. REPLAY (offline, always): reconstruct the exact same swap from the saved
//!      snapshot with the network backend disabled (`NoChain` errors if touched).
//!      Because YEVM's execution is deterministic, the offline run demands the
//!      same state in the same order and serves it from the snapshot -- so it
//!      never touches any chain, testnet or otherwise. This is the run whose
//!      result we report.
//!
//! The swap talks to the Uniswap V2 USDC/WETH pair directly (no router, no token
//! approval needed): send USDC into the pair, then call `swap(...)` to pull WETH
//! out. The sender is a synthetic address; only its ETH-for-gas and its USDC
//! balance are seeded locally (there is no private key for a real whale). Every
//! contract -- USDC, WETH, the pair -- and the live pool reserves come from chain.
//!
//! Usage:
//!   YEVM_RPC_URL=https://ethereum-rpc.publicnode.com cargo run -p yevm-demo --bin uniswap2
//!   # first run records the snapshot, then replays it offline;
//!   # later runs replay the snapshot offline only (no network).

use std::path::Path;

use eyre::{Result, bail};
use yevm::base::{Acc, Int, acc, int};
use yevm::core::cache::Cache;
use yevm::core::call::{Block, Head, Tx};
use yevm::core::chain::{Chain, Fetched, fetch};
use yevm::core::evm::Fetch;
use yevm::core::exe::{CallResult, Executor};
use yevm::core::rpc::Rpc;
use yevm::core::state::{Account, State};
use yevm::misc::keccak256;
use yevm::{Buf, Call};

// --- Mainnet addresses (checksummed, lower-cased by the parser) --------------

const USDC: Acc = acc("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const WETH: Acc = acc("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
/// Uniswap V2 USDC/WETH pair. token0 = USDC (0xA0.. < 0xC0.. = WETH), token1 = WETH.
const PAIR: Acc = acc("0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc");

/// Synthetic sender. No on-chain presence; we fund it locally (ETH + USDC).
const SENDER: Acc = acc("0x000000000000000000000000000000000000BEEF");

/// USDC (FiatTokenV2) `balances` mapping is at storage slot 9. Seeding this slot
/// is what gives SENDER a spendable USDC balance. If this were ever wrong, the
/// USDC transfer below would revert with "insufficient balance" -- a loud failure.
const USDC_BALANCES_SLOT: u64 = 9;

const SNAPSHOT: &str = "fetch/uniswap2-usdc-weth.json";

/// How much USDC to spend (6 decimals): 3,000 USDC.
const AMOUNT_IN_USDC: u128 = 3_000 * 1_000_000;

const GAS_LIMIT: u64 = 2_000_000;

// --- A chain backend that must never be called (proves the replay is offline) -

struct NoChain;

#[async_trait::async_trait]
impl Chain for NoChain {
    async fn get(&self, _: &Acc, _: &Int) -> Result<Int> {
        bail!("hermetic replay tried to read a storage slot from the network")
    }
    async fn acc(&self, _: &Acc) -> Result<Account> {
        bail!("hermetic replay tried to read an account from the network")
    }
    async fn code(&self, _: &Acc) -> Result<(Buf, Int)> {
        bail!("hermetic replay tried to read code from the network")
    }
    async fn nonce(&self, _: &Acc) -> Result<u64> {
        bail!("hermetic replay tried to read a nonce from the network")
    }
    async fn balance(&self, _: &Acc) -> Result<Int> {
        bail!("hermetic replay tried to read a balance from the network")
    }
    async fn head(&self, _: u64) -> Result<Head> {
        bail!("hermetic replay tried to read a block header from the network")
    }
    async fn block(&self, _: u64) -> Result<Block> {
        bail!("hermetic replay tried to read a block from the network")
    }
    async fn chain_id(&self) -> Result<u64> {
        bail!("hermetic replay tried to read the chain id from the network")
    }
}

// --- ABI helpers -------------------------------------------------------------

/// A `uint256` ABI word.
fn word(x: u128) -> Int {
    Int::from(x)
}

/// An `address` ABI word (20-byte address, left-padded to 32 bytes).
fn word_addr(a: Acc) -> Int {
    a.to::<32>()
}

/// Storage slot of `mapping(address => uint256)[key]` at mapping index `slot`.
/// Solidity layout: keccak256(pad32(key) ++ pad32(slot)).
fn map_slot(key: Acc, slot: u64) -> Int {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(word_addr(key).as_ref());
    buf[32..].copy_from_slice(word(slot as u128).as_ref());
    keccak256(&buf)
}

/// Uniswap V2 constant-product output with the 0.3% fee.
fn get_amount_out(amount_in: u128, reserve_in: u128, reserve_out: u128) -> Result<u128> {
    let amount_in_with_fee = amount_in
        .checked_mul(997)
        .ok_or_else(|| eyre::eyre!("overflow in amount_in * 997"))?;
    let numerator = amount_in_with_fee
        .checked_mul(reserve_out)
        .ok_or_else(|| eyre::eyre!("overflow in numerator; try a smaller AMOUNT_IN"))?;
    let denominator = reserve_in
        .checked_mul(1000)
        .and_then(|v| v.checked_add(amount_in_with_fee))
        .ok_or_else(|| eyre::eyre!("overflow in denominator"))?;
    Ok(numerator / denominator)
}

/// Render a token amount with `decimals` fractional digits (trailing zeros trimmed).
fn units(amount: u128, decimals: u32) -> String {
    let scale = 10u128.pow(decimals);
    let whole = amount / scale;
    let frac = amount % scale;
    if frac == 0 {
        return whole.to_string();
    }
    let frac = format!("{frac:0width$}", width = decimals as usize);
    format!("{whole}.{}", frac.trim_end_matches('0'))
}

// --- Execution ---------------------------------------------------------------

fn tx(chain_id: u64, base_fee: Int) -> Tx {
    Tx {
        chain_id: chain_id.into(),
        nonce: Int::ZERO,
        // Legacy tx (max_fee_per_gas == 0): gas_price must cover base_fee.
        gas_price: base_fee,
        max_fee_per_gas: Int::ZERO,
        max_priority_fee_per_gas: Int::ZERO,
        access_list: vec![],
        authorization_list: vec![],
        blob_versioned_hashes: vec![],
        max_fee_per_blob_gas: None,
        hash: Int::ZERO,
        index: Int::ZERO,
    }
}

/// Run a single call to completion and return (status, returndata, gas_used).
async fn exec(
    call: Call,
    cache: &mut Cache,
    chain: &impl Chain,
    head: &Head,
    chain_id: u64,
) -> Result<(bool, Buf, i64)> {
    cache.reset();
    let mut executor = Executor::new(call);
    let result = executor
        .run(&tx(chain_id, head.base_fee), head, cache, chain)
        .await?;
    match result {
        CallResult::Done { status, ret, gas } => Ok((!status.is_zero(), ret, gas.finalized)),
        CallResult::Created { .. } => bail!("unexpected contract creation"),
    }
}

struct Report {
    reserve_usdc: u128,
    reserve_weth: u128,
    amount_in: u128,
    expected_out: u128,
    weth_gained: u128,
    gas_transfer: i64,
    gas_swap: i64,
}

/// The swap itself. Identical code runs in the record phase (Rpc backend,
/// offline == false) and the replay phase (NoChain backend, offline == true).
/// Only the seeding (local, not fetched) and the deterministic call sequence
/// matter; both are identical across phases, so the offline fetch stream lines
/// up with what was recorded.
async fn simulate(
    cache: &mut Cache,
    chain: &impl Chain,
    head: &Head,
    chain_id: u64,
) -> Result<Report> {
    // Bring USDC's account/code into state BEFORE seeding its storage, so the
    // seeded slot lands on the real contract (not an empty, code-less account).
    fetch(Fetch::Account(USDC), cache, chain).await?;

    // Seed the synthetic sender: 100 ETH for gas, and AMOUNT_IN USDC to spend.
    cache.set_value(&SENDER, int("0x56bc75e2d63100000")); // 100 * 1e18 wei
    cache.init(
        &USDC,
        &map_slot(SENDER, USDC_BALANCES_SLOT),
        word(AMOUNT_IN_USDC),
    );

    // 1) Read live pool reserves: getReserves() -> (uint112 r0, uint112 r1, uint32).
    let get_reserves = Call::builder()
        .by(SENDER)
        .to(PAIR)
        .gas(GAS_LIMIT)
        .call("getReserves()", &[])
        .build();
    let (ok, ret, _) = exec(get_reserves, cache, chain, head, chain_id).await?;
    if !ok || ret.len() < 64 {
        bail!("getReserves() failed");
    }
    let reserve_usdc = Int::from(&ret.as_slice()[0..32]).as_u128();
    let reserve_weth = Int::from(&ret.as_slice()[32..64]).as_u128();

    let amount_out = get_amount_out(AMOUNT_IN_USDC, reserve_usdc, reserve_weth)?;

    // 2) WETH balance of the sender before the swap (expected 0).
    let weth_before = balance_of(WETH, SENDER, cache, chain, head, chain_id).await?;

    // 3) Move USDC into the pair: USDC.transfer(pair, amountIn).
    let transfer = Call::builder()
        .by(SENDER)
        .to(USDC)
        .gas(GAS_LIMIT)
        .call(
            "transfer(address,uint256)",
            &[word_addr(PAIR).as_ref(), word(AMOUNT_IN_USDC).as_ref()],
        )
        .build();
    let (ok, _, gas_transfer) = exec(transfer, cache, chain, head, chain_id).await?;
    if !ok {
        bail!(
            "USDC.transfer into the pair reverted (is slot {USDC_BALANCES_SLOT} still the balances map?)"
        );
    }

    // 4) Pull WETH out: pair.swap(0, amountOut, sender, "").
    //    ABI tail: amount0Out, amount1Out, to, bytes-offset(=0x80), bytes-len(=0).
    let swap = Call::builder()
        .by(SENDER)
        .to(PAIR)
        .gas(GAS_LIMIT)
        .call(
            "swap(uint256,uint256,address,bytes)",
            &[
                word(0).as_ref(),
                word(amount_out).as_ref(),
                word_addr(SENDER).as_ref(),
                word(0x80).as_ref(),
                word(0).as_ref(),
            ],
        )
        .build();
    let (ok, _, gas_swap) = exec(swap, cache, chain, head, chain_id).await?;
    if !ok {
        bail!("pair.swap reverted");
    }

    // 5) WETH balance after; the difference is what we actually received.
    let weth_after = balance_of(WETH, SENDER, cache, chain, head, chain_id).await?;

    Ok(Report {
        reserve_usdc,
        reserve_weth,
        amount_in: AMOUNT_IN_USDC,
        expected_out: amount_out,
        weth_gained: weth_after.saturating_sub(weth_before),
        gas_transfer,
        gas_swap,
    })
}

async fn balance_of(
    token: Acc,
    who: Acc,
    cache: &mut Cache,
    chain: &impl Chain,
    head: &Head,
    chain_id: u64,
) -> Result<u128> {
    let call = Call::builder()
        .by(SENDER)
        .to(token)
        .gas(GAS_LIMIT)
        .call("balanceOf(address)", &[word_addr(who).as_ref()])
        .build();
    let (ok, ret, _) = exec(call, cache, chain, head, chain_id).await?;
    if !ok || ret.len() < 32 {
        bail!("balanceOf failed");
    }
    Ok(Int::from(&ret.as_slice()[0..32]).as_u128())
}

// --- Record / replay wiring --------------------------------------------------

/// RECORD: run once against RPC, saving the ordered fetch stream to `SNAPSHOT`.
async fn record(url: &str) -> Result<()> {
    let rpc = Rpc::latest(url.to_string()).await?;
    let chain_id = rpc.chain_id().await?;
    let head = rpc.head(rpc.block_number).await?;
    println!(
        "recording snapshot at block {} (chain id {chain_id})...",
        head.number.as_u64()
    );

    let mut cache = Cache::new();
    cache.set_chain_id(chain_id);
    // The snapshot's first two entries are chain id and block, by convention
    // (Cache::prefetched skips them); real fetches follow, in execution order.
    cache.save_fetched(Fetched::ChainId(chain_id), 0.0);
    cache.save_fetched(
        Fetched::Block(Block {
            head: head.clone(),
            txs: vec![],
            withdrawals: vec![],
        }),
        0.0,
    );

    simulate(&mut cache, &rpc, &head, chain_id).await?;

    std::fs::create_dir_all("fetch")?;
    std::fs::write(SNAPSHOT, serde_json::to_vec(&cache.fetched)?)?;
    println!(
        "saved {} state fetches to {SNAPSHOT}\n",
        cache.fetched.len().saturating_sub(2)
    );
    Ok(())
}

/// REPLAY: reconstruct the swap purely from `SNAPSHOT`, network disabled.
async fn replay() -> Result<Report> {
    let bytes = std::fs::read(SNAPSHOT)?;
    let fetched: Vec<Fetched> = serde_json::from_str(std::str::from_utf8(&bytes)?)?;

    let Some(Fetched::ChainId(chain_id)) = fetched.first().cloned() else {
        bail!("snapshot missing chain id");
    };
    let Some(Fetched::Block(block)) = fetched.get(1).cloned() else {
        bail!("snapshot missing block");
    };
    let head = block.head;

    let mut cache = Cache::new();
    cache.set_chain_id(chain_id);
    cache.prefetched(fetched); // marks the cache offline; serves state from the list

    simulate(&mut cache, &NoChain, &head, chain_id).await
}

#[tokio::main]
async fn main() -> Result<()> {
    let url = std::env::var("YEVM_RPC_URL")
        .unwrap_or_else(|_| "https://ethereum-rpc.publicnode.com".to_string());

    if !Path::new(SNAPSHOT).exists() {
        record(&url).await?;
    } else {
        println!("using existing snapshot {SNAPSHOT} (delete it to re-record)\n");
    }

    // The reported result comes from the offline replay: no chain is touched.
    let r = replay().await?;

    println!("=== Hermetic Uniswap V2 swap: buy WETH with USDC ===");
    println!("(offline replay; NoChain backend errors on any network access)\n");
    println!(
        "pool reserves : {} USDC / {} WETH",
        units(r.reserve_usdc, 6),
        units(r.reserve_weth, 18)
    );
    let spot = r.reserve_usdc as f64 / 1e6 / (r.reserve_weth as f64 / 1e18);
    println!("spot price    : {spot:.2} USDC/WETH\n");

    println!("spent         : {} USDC", units(r.amount_in, 6));
    println!(
        "received      : {} WETH  (quoted {})",
        units(r.weth_gained, 18),
        units(r.expected_out, 18)
    );
    let paid = r.amount_in as f64 / 1e6 / (r.weth_gained as f64 / 1e18);
    println!("effective     : {paid:.2} USDC/WETH");
    println!("slippage      : {:.3}%", (paid / spot - 1.0) * 100.0);
    println!(
        "gas           : {} transfer + {} swap",
        r.gas_transfer, r.gas_swap
    );

    assert_eq!(
        r.weth_gained, r.expected_out,
        "received WETH should match the constant-product quote"
    );
    Ok(())
}
