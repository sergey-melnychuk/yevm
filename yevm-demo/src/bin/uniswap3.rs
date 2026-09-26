//! Hermetic Uniswap V3 swap against YEVM: buy WETH with USDC.
//!
//! Same record/replay design as `uniswap2.rs`, against Uniswap V3 instead:
//!
//!   1. RECORD (online, once): execute the swap against real mainnet state via
//!      `YEVM_RPC_URL`, saving every state fetch to `fetch/uniswap3-usdc-weth.json`.
//!   2. REPLAY (offline, always): rerun from the snapshot with the network
//!      backend disabled (`NoChain` errors if touched). This is the reported run.
//!
//! V3 has no simple reserves; the executed price comes out of the pool's tick
//! liquidity. So instead of pricing the trade ourselves, we route it through the
//! real V3 `SwapRouter.exactInputSingle` with `amountOutMinimum = 0` and read
//! what the swap returns. The sender approves the router on-chain (real ERC-20
//! `approve`, no allowance-slot poking); only its ETH-for-gas and USDC balance
//! are seeded, since there is no private key for a real holder.
//!
//! Usage:
//!   YEVM_RPC_URL=https://ethereum-rpc.publicnode.com cargo run -p yevm-demo --bin uniswap3

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

// --- Mainnet addresses -------------------------------------------------------

const USDC: Acc = acc("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const WETH: Acc = acc("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
/// Uniswap V3 SwapRouter (the original 0xE592... router, with `deadline`).
const ROUTER: Acc = acc("0xE592427A0AEce92De3Edee1F18E0157C05861564");
/// USDC/WETH 0.05% pool -- read only, to show a spot price alongside the fill.
const POOL: Acc = acc("0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");

/// Synthetic sender. No on-chain presence; we fund it locally.
const SENDER: Acc = acc("0x000000000000000000000000000000000000BEEF");

/// USDC `balances` mapping storage slot.
const USDC_BALANCES_SLOT: u64 = 9;
/// Fee tier of the pool we route through (0.05% = 500).
const FEE: u128 = 500;

const SNAPSHOT: &str = "fetch/uniswap3-usdc-weth.json";

/// How much USDC to spend (6 decimals): 3,000 USDC.
const AMOUNT_IN_USDC: u128 = 3_000 * 1_000_000;

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

/// Read the pool's slot0.sqrtPriceX96 and derive a human USDC/WETH spot price.
async fn spot_price(
    cache: &mut Cache,
    chain: &impl Chain,
    head: &Head,
    chain_id: u64,
) -> Result<f64> {
    let call = Call::builder()
        .by(SENDER)
        .to(POOL)
        .gas(GAS_LIMIT)
        .call("slot0()", &[])
        .build();
    let (ok, ret, _) = exec(call, cache, chain, head, chain_id).await?;
    if !ok || ret.len() < 32 {
        bail!("slot0() failed");
    }
    // First return word is uint160 sqrtPriceX96 (fits in u128 for this pool).
    let sqrt_price_x96 = Int::from(&ret.as_slice()[0..32]).as_u128() as f64;
    let sqrt_p = sqrt_price_x96 / 2f64.powi(96);
    let price_raw = sqrt_p * sqrt_p; // token1(wei) per token0(1e-6 USDC)
    // USDC/WETH (human) = 1e12 / price_raw  (decimals: WETH 18, USDC 6)
    Ok(1e12 / price_raw)
}

struct Report {
    spot: f64,
    amount_in: u128,
    amount_out: u128,
    weth_gained: u128,
    gas_approve: i64,
    gas_swap: i64,
}

/// Identical in record and replay: seed the sender, approve the router on-chain,
/// then `exactInputSingle` USDC -> WETH and read the result.
async fn simulate(
    cache: &mut Cache,
    chain: &impl Chain,
    head: &Head,
    chain_id: u64,
) -> Result<Report> {
    // Bring USDC's account/code into state before seeding its balance slot.
    fetch(Fetch::Account(USDC), cache, chain).await?;
    cache.set_value(&SENDER, int("0x56bc75e2d63100000")); // 100 ETH
    cache.init(
        &USDC,
        &map_slot(SENDER, USDC_BALANCES_SLOT),
        word(AMOUNT_IN_USDC),
    );

    let spot = spot_price(cache, chain, head, chain_id).await?;

    // 1) approve the router to pull USDC (real ERC-20 approve; sets allowance).
    let approve = Call::builder()
        .by(SENDER)
        .to(USDC)
        .gas(GAS_LIMIT)
        .call(
            "approve(address,uint256)",
            &[word_addr(ROUTER).as_ref(), word(AMOUNT_IN_USDC).as_ref()],
        )
        .build();
    let (ok, _, gas_approve) = exec(approve, cache, chain, head, chain_id).await?;
    if !ok {
        bail!("USDC.approve reverted");
    }

    let weth_before = balance_of(WETH, SENDER, cache, chain, head, chain_id).await?;

    // 2) exactInputSingle((tokenIn, tokenOut, fee, recipient, deadline,
    //                       amountIn, amountOutMinimum, sqrtPriceLimitX96))
    //    The tuple is all-static, so its 8 words encode inline after the selector.
    let swap = Call::builder()
        .by(SENDER)
        .to(ROUTER)
        .gas(GAS_LIMIT)
        .call(
            "exactInputSingle((address,address,uint24,address,uint256,uint256,uint256,uint160))",
            &[
                word_addr(USDC).as_ref(),
                word_addr(WETH).as_ref(),
                word(FEE).as_ref(),
                word_addr(SENDER).as_ref(),
                word(u128::MAX).as_ref(), // deadline: far future
                word(AMOUNT_IN_USDC).as_ref(),
                word(0).as_ref(), // amountOutMinimum
                word(0).as_ref(), // sqrtPriceLimitX96: no limit
            ],
        )
        .build();
    let (ok, ret, gas_swap) = exec(swap, cache, chain, head, chain_id).await?;
    if !ok {
        bail!("router.exactInputSingle reverted");
    }
    let amount_out = Int::from(&ret.as_slice()[0..32]).as_u128();

    let weth_after = balance_of(WETH, SENDER, cache, chain, head, chain_id).await?;

    Ok(Report {
        spot,
        amount_in: AMOUNT_IN_USDC,
        amount_out,
        weth_gained: weth_after.saturating_sub(weth_before),
        gas_approve,
        gas_swap,
    })
}

// --- Snapshot record / replay ------------------------------------------------

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
    cache.prefetched(fetched);
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

    let r = replay().await?;

    println!("=== Hermetic Uniswap V3 swap: buy WETH with USDC ===");
    println!("(offline replay; NoChain backend errors on any network access)\n");
    println!(
        "pool          : USDC/WETH V3 {:.2}% fee",
        FEE as f64 / 10_000.0
    );
    println!("spot price    : {:.2} USDC/WETH\n", r.spot);
    println!("spent         : {} USDC", units(r.amount_in, 6));
    println!(
        "received      : {} WETH  (router returned {})",
        units(r.weth_gained, 18),
        units(r.amount_out, 18)
    );
    let paid = r.amount_in as f64 / 1e6 / (r.weth_gained as f64 / 1e18);
    println!("effective     : {paid:.2} USDC/WETH");
    println!("slippage      : {:.3}%", (paid / r.spot - 1.0) * 100.0);
    println!(
        "gas           : {} approve + {} swap",
        r.gas_approve, r.gas_swap
    );

    assert_eq!(
        r.weth_gained, r.amount_out,
        "received WETH should match the router's reported amountOut"
    );
    Ok(())
}
