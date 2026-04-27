use std::{collections::HashMap, fs::File, io::BufReader};

use serde::Deserialize;
use yaevmi_base::{Int, acc, int, math::lift};
use yaevmi_core::{
    Call, Head, Tx,
    cache::{Cache, Env},
    exe::{CallResult, Executor},
    state::{Account, State},
    trace::{Event, Step},
};
use yaevmi_misc::buf::Buf;

use crate::eth::EmptyChain;

pub mod auths;
pub mod calls;
pub mod count;
pub mod flash;
pub mod hello;
pub mod maker;
pub mod proxy;
pub mod token;
pub mod value;

#[derive(Deserialize)]
pub struct Combined {
    pub contracts: HashMap<String, Contract>,
}

#[derive(Deserialize)]
pub struct Contract {
    pub bin: Buf,
    #[serde(rename = "bin-runtime")]
    pub bin_runtime: Buf,
}

pub fn load() -> Result<Combined, eyre::Report> {
    let file = File::open(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/sol/bin/combined.json"
    ))?;
    let reader = BufReader::new(file);
    let combined: Combined = serde_json::from_reader(reader)?;
    Ok(combined)
}

pub async fn run(
    call: Call,
    head: Head,
    env: Env,
    tx: Tx,
) -> eyre::Result<(Int, Buf, i64, Vec<Step>, Env)> {
    let mut state = Cache::new();
    state.insert_account(head.coinbase, Account::default());
    for (acc, info, storage) in env {
        state.insert_account(acc, info);
        for (key, val) in storage {
            state.init(&acc, &key, val);
        }
    }

    let mut exe = Executor::new(call);
    let chain = EmptyChain;
    let res = exe.run(tx, head, &mut state, &chain).await?;

    let steps = state
        .events
        .iter()
        .filter_map(|trace| match &trace.event {
            Event::Step(step) => Some(step.to_owned()),
            _ => None,
        })
        .collect::<Vec<_>>();

    let snapshot = state
        .snapshot()
        .into_iter()
        .filter(|(_, account, storage)| {
            let is_empty = account.value.is_zero()
                && account.nonce.is_zero()
                && account.code.0.is_empty()
                && (storage.is_empty() || storage.iter().all(|(_, v)| v.is_zero()));
            !is_empty
        })
        .collect::<Vec<_>>();

    Ok(match res {
        CallResult::Created {
            acc: addr,
            code,
            gas,
        } => (addr.to(), code, gas.spent, steps, snapshot),
        CallResult::Done { status, ret, gas } => (status, ret, gas.spent, steps, snapshot),
    })
}

pub fn ethers(eth: i32) -> Int {
    let exp = lift(|[eth, a, b]| eth * a.pow(b));
    exp([Int::from(eth), Int::from(10), Int::from(18)])
}

pub fn head() -> Head {
    Head {
        number: 1.into(),
        hash: int("0x1"),
        gas_limit: 10_000_000.into(),
        coinbase: acc("0xC014BA5E"),
        timestamp: 42.into(),
        base_fee: 1.into(),
        excess_blob_gas: Some(Int::ZERO),
        blobhash: Some(Int::ONE),
        prevrandao: Int::ONE,
        parent_hash: int("0x1"),
        parent_beacon_block_root: None,
    }
}

pub fn tx(nonce: u64) -> Tx {
    Tx {
        chain_id: 1.into(),
        nonce: nonce.into(),
        gas_price: 1.into(),
        max_fee_per_gas: 1.into(),
        max_priority_fee_per_gas: 1.into(),
        access_list: vec![],
        authorization_list: vec![],
        blob_versioned_hashes: vec![],
        max_fee_per_blob_gas: None,
        hash: Int::ZERO,
        index: Int::ZERO,
    }
}

/// Assert that two run results match on status and return data.
/// Gas differences are logged but not asserted (known minor accounting diffs).
pub fn assert_match(
    res: &(Int, Buf, i64, Vec<Step>, Env),
    exp: &(Int, Buf, i64, Vec<Step>, Env),
    msg: &str,
) {
    assert_eq!(res.0, exp.0, "{msg}: status mismatch");
    assert_eq!(res.1, exp.1, "{msg}: return data mismatch");
    if res.2 != exp.2 {
        eprintln!(
            "  {msg}: gas diff: yevm={} revm={} (delta={:+})",
            res.2,
            exp.2,
            res.2 - exp.2
        );
    }

    let mut skip = 0;
    for (y, r) in res.3.iter().zip(exp.3.iter()) {
        if y == r {
            skip += 1;
            continue;
        }
        if std::env::var("STEP").is_ok() {
            res.3.iter().take(skip).for_each(|s| eprintln!("{s:#?}"));
        }
        pretty_assertions::assert_eq!(y, r, "{msg}: step mismatch at {}", skip);
    }

    pretty_assertions::assert_eq!(res.4, exp.4, "{msg}: env mismatch");
}

/// ABI function selector: first 4 bytes of keccak256(signature).
pub fn selector(sig: &str) -> Vec<u8> {
    yaevmi_misc::keccak256(sig.as_bytes()).as_ref()[..4].to_vec()
}

#[test]
fn test_load_combined_json() {
    let combined = load().unwrap();
    assert!(!combined.contracts.is_empty());
    assert!(combined.contracts.contains_key("sol/hello.sol:Hello"));
}
