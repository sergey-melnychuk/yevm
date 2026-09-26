//! Hermetic sandwich-attack demo against YEVM (educational; never touches a chain).
//!
//! A sandwich attack is a form of MEV: an attacker who sees a victim's pending
//! buy brackets it with two of their own trades on the same pool.
//!
//!   1. FRONT-RUN: attacker buys WETH first, pushing the price up.
//!   2. VICTIM:    the victim's buy now executes at that worse price.
//!   3. BACK-RUN:  attacker sells the WETH back into the (now higher) pool.
//!
//! The attacker pockets the difference; the victim's extra slippage is the loss.
//!
//! This builds on `uniswap*.rs`. Same record/replay design: real Uniswap state is
//! pulled from RPC once and saved, then every result below is produced by an
//! OFFLINE replay whose chain backend (`NoChain`) errors if the network is
//! touched. Two scenarios are recorded against the SAME pinned block:
//!
//!   * baseline  -- the victim swaps alone.
//!   * sandwich  -- front-run, victim, back-run, sharing one state so each trade
//!                  sees the previous trade's price impact.
//!
//! Comparing the victim's WETH in the two scenarios is the harm the attack does.
//!
//! Usage:
//!   YEVM_RPC_URL=https://ethereum-rpc.publicnode.com cargo run -p yevm-demo --bin sandwich

use std::path::Path;

use eyre::{Result, bail};
use serde::{Deserialize, Serialize};
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

// --- Mainnet addresses -------------------------------------------------------

const USDC: Acc = acc("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const WETH: Acc = acc("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
/// Uniswap V2 USDC/WETH pair. token0 = USDC, token1 = WETH (USDC addr < WETH addr).
const PAIR: Acc = acc("0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc");

const ATTACKER: Acc = acc("0x000000000000000000000000000000000000a77a");
const VICTIM: Acc = acc("0x000000000000000000000000000000000000f00d");

/// USDC `balances` mapping storage slot.
const USDC_BALANCES_SLOT: u64 = 9;

const SNAPSHOT: &str = "fetch/sandwich-usdc-weth.json";

/// The victim's buy. Large relative to this pool, so it slips noticeably -- which
/// is exactly the condition that makes a sandwich profitable.
const VICTIM_IN_USDC: u128 = 50_000 * 1_000_000;
/// The attacker's front-run size (USDC). Tuned to be near-optimal for the above.
const ATTACKER_IN_USDC: u128 = 50_000 * 1_000_000;

const GAS_LIMIT: u64 = 3_000_000;

// --- A chain backend that must never be called (proves the replay is offline) -

struct NoChain;

#[async_trait::async_trait]
impl Chain for NoChain {
    async fn get(&self, _: &Acc, _: &Int) -> Result<Int> {
        bail!("hermetic replay tried to read storage from the network")
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
        bail!("hermetic replay tried to read a header from the network")
    }
    async fn block(&self, _: u64) -> Result<Block> {
        bail!("hermetic replay tried to read a block from the network")
    }
    async fn chain_id(&self) -> Result<u64> {
        bail!("hermetic replay tried to read the chain id from the network")
    }
}

// --- ABI + math helpers ------------------------------------------------------

fn word(x: u128) -> Int {
    Int::from(x)
}
fn word_addr(a: Acc) -> Int {
    a.to::<32>()
}
fn map_slot(key: Acc, slot: u64) -> Int {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(word_addr(key).as_ref());
    buf[32..].copy_from_slice(word(slot as u128).as_ref());
    keccak256(&buf)
}

/// Uniswap V2 constant-product output with the 0.3% fee.
fn get_amount_out(amount_in: u128, reserve_in: u128, reserve_out: u128) -> Result<u128> {
    let fee_in = amount_in
        .checked_mul(997)
        .ok_or_else(|| eyre::eyre!("overflow (amount too large)"))?;
    let numerator = fee_in
        .checked_mul(reserve_out)
        .ok_or_else(|| eyre::eyre!("overflow (amount too large)"))?;
    let denominator = reserve_in
        .checked_mul(1000)
        .and_then(|v| v.checked_add(fee_in))
        .ok_or_else(|| eyre::eyre!("overflow"))?;
    Ok(numerator / denominator)
}

fn units(amount: u128, decimals: u32) -> String {
    let scale = 10u128.pow(decimals);
    let (whole, frac) = (amount / scale, amount % scale);
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
        gas_price: base_fee, // legacy tx: must cover base fee
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

/// Fund a synthetic actor: 100 ETH for gas and `usdc` units of USDC to spend.
/// `fetch(USDC account)` must have run first so the balance slot lands on the
/// real contract, not a code-less placeholder account.
fn fund(cache: &mut Cache, who: Acc, usdc: u128) {
    cache.set_value(&who, int("0x56bc75e2d63100000")); // 100 * 1e18 wei
    cache.init(&USDC, &map_slot(who, USDC_BALANCES_SLOT), word(usdc));
}

async fn read_reserves(
    cache: &mut Cache,
    chain: &impl Chain,
    head: &Head,
    chain_id: u64,
) -> Result<(u128, u128)> {
    let call = Call::builder()
        .by(ATTACKER)
        .to(PAIR)
        .gas(GAS_LIMIT)
        .call("getReserves()", &[])
        .build();
    let (ok, ret, _) = exec(call, cache, chain, head, chain_id).await?;
    if !ok || ret.len() < 64 {
        bail!("getReserves() failed");
    }
    let r0 = Int::from(&ret.as_slice()[0..32]).as_u128(); // USDC
    let r1 = Int::from(&ret.as_slice()[32..64]).as_u128(); // WETH
    Ok((r0, r1))
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
        .by(who)
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

/// Buy WETH with `usdc_in` USDC on the pair. Returns the reserves used and the
/// WETH bought. Transfer the input into the pair, then `swap` it out.
async fn buy_weth(
    buyer: Acc,
    usdc_in: u128,
    cache: &mut Cache,
    chain: &impl Chain,
    head: &Head,
    chain_id: u64,
) -> Result<(u128, u128, u128, i64)> {
    let (r0, r1) = read_reserves(cache, chain, head, chain_id).await?;
    let weth_out = get_amount_out(usdc_in, r0, r1)?;

    let transfer = Call::builder()
        .by(buyer)
        .to(USDC)
        .gas(GAS_LIMIT)
        .call(
            "transfer(address,uint256)",
            &[word_addr(PAIR).as_ref(), word(usdc_in).as_ref()],
        )
        .build();
    let (ok, _, g1) = exec(transfer, cache, chain, head, chain_id).await?;
    if !ok {
        bail!("USDC.transfer into pair reverted");
    }

    // swap(amount0Out=0, amount1Out=weth_out, to=buyer, data="")
    let swap = Call::builder()
        .by(buyer)
        .to(PAIR)
        .gas(GAS_LIMIT)
        .call(
            "swap(uint256,uint256,address,bytes)",
            &[
                word(0).as_ref(),
                word(weth_out).as_ref(),
                word_addr(buyer).as_ref(),
                word(0x80).as_ref(),
                word(0).as_ref(),
            ],
        )
        .build();
    let (ok, _, g2) = exec(swap, cache, chain, head, chain_id).await?;
    if !ok {
        bail!("pair.swap (buy) reverted");
    }
    Ok((r0, r1, weth_out, g1 + g2))
}

/// Sell `weth_in` WETH back into the pair for USDC. Returns USDC received.
async fn sell_weth(
    seller: Acc,
    weth_in: u128,
    cache: &mut Cache,
    chain: &impl Chain,
    head: &Head,
    chain_id: u64,
) -> Result<(u128, i64)> {
    let (r0, r1) = read_reserves(cache, chain, head, chain_id).await?;
    let usdc_out = get_amount_out(weth_in, r1, r0)?; // in = WETH (r1), out = USDC (r0)

    let transfer = Call::builder()
        .by(seller)
        .to(WETH)
        .gas(GAS_LIMIT)
        .call(
            "transfer(address,uint256)",
            &[word_addr(PAIR).as_ref(), word(weth_in).as_ref()],
        )
        .build();
    let (ok, _, g1) = exec(transfer, cache, chain, head, chain_id).await?;
    if !ok {
        bail!("WETH.transfer into pair reverted");
    }

    // swap(amount0Out=usdc_out, amount1Out=0, to=seller, data="")
    let swap = Call::builder()
        .by(seller)
        .to(PAIR)
        .gas(GAS_LIMIT)
        .call(
            "swap(uint256,uint256,address,bytes)",
            &[
                word(usdc_out).as_ref(),
                word(0).as_ref(),
                word_addr(seller).as_ref(),
                word(0x80).as_ref(),
                word(0).as_ref(),
            ],
        )
        .build();
    let (ok, _, g2) = exec(swap, cache, chain, head, chain_id).await?;
    if !ok {
        bail!("pair.swap (sell) reverted");
    }
    Ok((usdc_out, g1 + g2))
}

// --- Scenarios ---------------------------------------------------------------

struct Baseline {
    reserve_usdc: u128,
    reserve_weth: u128,
    victim_out: u128,
}

struct SandwichRun {
    attacker_weth: u128,
    victim_out: u128,
    attacker_usdc_back: u128,
    gas_front: i64,
    gas_victim: i64,
    gas_back: i64,
}

/// Victim swaps alone -- the price they *should* have gotten.
async fn run_baseline(
    cache: &mut Cache,
    chain: &impl Chain,
    head: &Head,
    chain_id: u64,
) -> Result<Baseline> {
    fetch(Fetch::Account(USDC), cache, chain).await?;
    fund(cache, VICTIM, VICTIM_IN_USDC);

    let (r0, r1, _quote, _) =
        buy_weth(VICTIM, VICTIM_IN_USDC, cache, chain, head, chain_id).await?;
    let victim_out = balance_of(WETH, VICTIM, cache, chain, head, chain_id).await?;
    Ok(Baseline {
        reserve_usdc: r0,
        reserve_weth: r1,
        victim_out,
    })
}

/// Front-run, victim, back-run -- sharing one state, so each trade moves the
/// price for the next.
async fn run_sandwich(
    cache: &mut Cache,
    chain: &impl Chain,
    head: &Head,
    chain_id: u64,
) -> Result<SandwichRun> {
    fetch(Fetch::Account(USDC), cache, chain).await?;
    fund(cache, ATTACKER, ATTACKER_IN_USDC);
    fund(cache, VICTIM, VICTIM_IN_USDC);

    // 1) attacker front-runs
    let (_, _, attacker_weth, gas_front) =
        buy_weth(ATTACKER, ATTACKER_IN_USDC, cache, chain, head, chain_id).await?;
    // 2) victim buys at the worsened price
    let (_, _, _victim_quote, gas_victim) =
        buy_weth(VICTIM, VICTIM_IN_USDC, cache, chain, head, chain_id).await?;
    // 3) attacker back-runs, selling exactly the WETH it acquired
    let (_usdc, gas_back) =
        sell_weth(ATTACKER, attacker_weth, cache, chain, head, chain_id).await?;

    let victim_out = balance_of(WETH, VICTIM, cache, chain, head, chain_id).await?;
    let attacker_usdc_back = balance_of(USDC, ATTACKER, cache, chain, head, chain_id).await?;
    Ok(SandwichRun {
        attacker_weth,
        victim_out,
        attacker_usdc_back,
        gas_front,
        gas_victim,
        gas_back,
    })
}

// --- Snapshot record / replay ------------------------------------------------

#[derive(Serialize, Deserialize)]
struct Snapshot {
    baseline: Vec<Fetched>,
    sandwich: Vec<Fetched>,
}

fn seed_cache(chain_id: u64, head: &Head) -> Cache {
    let mut cache = Cache::new();
    cache.set_chain_id(chain_id);
    cache.save_fetched(Fetched::ChainId(chain_id), 0.0);
    cache.save_fetched(
        Fetched::Block(Block {
            head: head.clone(),
            txs: vec![],
            withdrawals: vec![],
        }),
        0.0,
    );
    cache
}

/// RECORD: run both scenarios against RPC at one pinned block, save both fetch
/// streams. State reads for both scenarios come from the same block, so the
/// baseline and the sandwich start from identical pool reserves.
async fn record(url: &str) -> Result<()> {
    let rpc = Rpc::latest(url.to_string()).await?;
    let chain_id = rpc.chain_id().await?;
    let head = rpc.head(rpc.block_number).await?;
    println!(
        "recording snapshot at block {} (chain id {chain_id})...",
        head.number.as_u64()
    );

    let mut base_cache = seed_cache(chain_id, &head);
    run_baseline(&mut base_cache, &rpc, &head, chain_id).await?;

    let mut sand_cache = seed_cache(chain_id, &head);
    run_sandwich(&mut sand_cache, &rpc, &head, chain_id).await?;

    let snap = Snapshot {
        baseline: base_cache.fetched,
        sandwich: sand_cache.fetched,
    };
    std::fs::create_dir_all("fetch")?;
    std::fs::write(SNAPSHOT, serde_json::to_vec(&snap)?)?;
    println!(
        "saved {} + {} state fetches to {SNAPSHOT}\n",
        snap.baseline.len().saturating_sub(2),
        snap.sandwich.len().saturating_sub(2),
    );
    Ok(())
}

/// REPLAY: reconstruct both scenarios purely from the snapshot, network off.
async fn replay() -> Result<(Baseline, SandwichRun)> {
    let bytes = std::fs::read(SNAPSHOT)?;
    let snap: Snapshot = serde_json::from_str(std::str::from_utf8(&bytes)?)?;

    let (chain_id, head) = match (snap.baseline.first(), snap.baseline.get(1)) {
        (Some(Fetched::ChainId(id)), Some(Fetched::Block(b))) => (*id, b.head.clone()),
        _ => bail!("snapshot missing chain id / block prefix"),
    };

    let mut base_cache = Cache::new();
    base_cache.set_chain_id(chain_id);
    base_cache.prefetched(snap.baseline);
    let baseline = run_baseline(&mut base_cache, &NoChain, &head, chain_id).await?;

    let mut sand_cache = Cache::new();
    sand_cache.set_chain_id(chain_id);
    sand_cache.prefetched(snap.sandwich);
    let sandwich = run_sandwich(&mut sand_cache, &NoChain, &head, chain_id).await?;

    Ok((baseline, sandwich))
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

    let (base, sand) = replay().await?;

    let spot = base.reserve_usdc as f64 / 1e6 / (base.reserve_weth as f64 / 1e18);
    let victim_loss_weth = base.victim_out.saturating_sub(sand.victim_out);
    let victim_loss_usd = victim_loss_weth as f64 / 1e18 * spot;
    let profit = sand.attacker_usdc_back as i128 - ATTACKER_IN_USDC as i128;

    println!("=== Hermetic sandwich attack: Uniswap V2 USDC/WETH ===");
    println!("(offline replay; NoChain backend errors on any network access)\n");

    println!(
        "pool          : {} USDC / {} WETH  ({spot:.2} USDC/WETH)\n",
        units(base.reserve_usdc, 6),
        units(base.reserve_weth, 18)
    );

    println!("VICTIM buys with {} USDC", units(VICTIM_IN_USDC, 6));
    println!(
        "  alone       : receives {} WETH",
        units(base.victim_out, 18)
    );
    println!(
        "  sandwiched  : receives {} WETH",
        units(sand.victim_out, 18)
    );
    println!(
        "  --> loss    : {} WETH (~{:.0} USDC) stolen via slippage\n",
        units(victim_loss_weth, 18),
        victim_loss_usd
    );

    println!("ATTACKER (front-run {} USDC)", units(ATTACKER_IN_USDC, 6));
    println!(
        "  front-run   : buys  {} WETH",
        units(sand.attacker_weth, 18)
    );
    println!(
        "  back-run    : sells {} WETH -> {} USDC",
        units(sand.attacker_weth, 18),
        units(sand.attacker_usdc_back, 6)
    );
    if profit >= 0 {
        println!(
            "  --> profit  : +{} USDC (before gas)",
            units(profit as u128, 6)
        );
    } else {
        println!(
            "  --> loss    : -{} USDC (attack unprofitable at this size)",
            units((-profit) as u128, 6)
        );
    }
    println!(
        "  gas         : {} front + {} back\n",
        sand.gas_front, sand.gas_back
    );

    // Defense: a victim min-out anywhere in this window would have reverted the
    // sandwiched swap, denying the attack.
    println!("DEFENSE: a slippage limit (minOut) between the sandwiched and alone");
    println!(
        "  amounts ({} .. {} WETH) reverts the victim's swap and blocks the attack.",
        units(sand.victim_out, 18),
        units(base.victim_out, 18)
    );
    println!(
        "  victim gas ~{} spent regardless on the reverting tx.",
        sand.gas_victim
    );

    assert!(
        sand.victim_out < base.victim_out,
        "sandwiched victim must receive less than baseline"
    );
    Ok(())
}
