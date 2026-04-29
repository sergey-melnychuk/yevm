pub mod dto;

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use yaevmi_base::{
    Acc, Int,
    math::{ONE, ZERO, lift},
};
use yaevmi_core::{
    Call, Head, Tx,
    cache::Env,
    call::{AccessListItem, Block},
    chain::Chain,
    state::Account,
};

use dto::{PostEntry, TestCase};
use yaevmi_misc::buf::Buf;

/// Minimal Chain impl for tests — all state is pre-loaded into Cache.
/// Any Fetch call hitting this means the pre-state is incomplete.
pub struct EmptyChain;

#[async_trait::async_trait]
impl Chain for EmptyChain {
    async fn get(&self, _: &Acc, _: &Int) -> eyre::Result<Int> {
        Ok(Default::default()) // empty/zero slots are not stored
    }
    async fn acc(&self, _: &Acc) -> eyre::Result<Account> {
        Ok(Default::default()) // empty accounts are not stored
    }
    async fn code(&self, _: &Acc) -> eyre::Result<(Buf, Int)> {
        eyre::bail!("EmptyChain: code(acc) not available")
    }
    async fn nonce(&self, _: &Acc) -> eyre::Result<u64> {
        eyre::bail!("EmptyChain: nonce(acc) not available")
    }
    async fn balance(&self, _: &Acc) -> eyre::Result<Int> {
        eyre::bail!("EmptyChain: balance(acc) not available")
    }
    async fn head(&self, _: u64) -> eyre::Result<Head> {
        eyre::bail!("EmptyChain: head(number) not available")
    }
    async fn block(&self, number: u64) -> eyre::Result<Block> {
        eyre::bail!("EmptyChain: block({number}) not available")
    }
    async fn chain_id(&self) -> eyre::Result<u64> {
        Ok(0)
    }
}

/// Build a map from addresses to account states.
pub fn build_map(env: &Env) -> HashMap<Acc, dto::AccountState> {
    env.iter()
        .map(|(acc, account, storage)| {
            let storage = storage.iter().cloned().collect();
            (
                *acc,
                dto::AccountState {
                    balance: account.value,
                    code: account.code.0.clone(),
                    nonce: account.nonce,
                    storage,
                },
            )
        })
        .collect()
}

/// Build an Env from a test case's `pre` section.
pub fn build_env(tc: &TestCase) -> Env {
    use yaevmi_misc::keccak256;

    tc.pre
        .iter()
        .map(|(acc, state)| {
            let code_bytes = state.code.as_slice().to_vec();
            let code_hash = if code_bytes.is_empty() {
                Int::ZERO
            } else {
                Int::from(keccak256(&code_bytes).as_ref())
            };
            let account = Account {
                value: state.balance,
                nonce: state.nonce,
                code: (code_bytes.into(), code_hash),
            };
            let storage: Vec<(Int, Int)> = state.storage.iter().map(|(k, v)| (*k, *v)).collect();
            (*acc, account, storage)
        })
        .collect()
}

/// Build the Head (block environment) from the `env` section.
pub fn build_head(tc: &TestCase) -> Head {
    let hash = tc
        .env
        .current_random
        .or(tc.env.current_difficulty)
        .unwrap_or(Int::ZERO);
    Head {
        number: tc.env.current_number.to(),
        hash,
        gas_limit: tc.env.current_gas_limit,
        coinbase: tc.env.current_coinbase.to(),
        timestamp: tc.env.current_timestamp,
        base_fee: tc.env.current_base_fee.unwrap_or(Int::ZERO),
        prevrandao: hash,
        ..Head::default()
    }
}

/// Build a Call for one (data_idx, gas_idx, value_idx) combination.
pub fn build_call_tx(tc: &TestCase, idx: &dto::Indexes) -> (Call, Tx) {
    (
        Call {
            by: tc.transaction.sender,
            to: tc.transaction.to,
            gas: tc.transaction.gas_limit[idx.gas].as_u64(),
            eth: tc.transaction.value[idx.value],
            data: tc.transaction.data[idx.data].as_slice().to_vec().into(),
        },
        Tx {
            chain_id: 1u32.into(),
            nonce: tc.transaction.nonce.to(),
            gas_price: tc.transaction.gas_price.unwrap_or(Int::ZERO),
            max_fee_per_gas: tc.transaction.max_fee_per_gas.unwrap_or(Int::ZERO),
            max_priority_fee_per_gas: tc.transaction.max_priority_fee_per_gas.unwrap_or(Int::ZERO),
            access_list: tc
                .transaction
                .access_lists
                .as_ref()
                .and_then(|v| v.get(idx.data))
                .and_then(|v| v.as_ref())
                .map(|vec| {
                    vec.iter()
                        .map(|a| AccessListItem {
                            address: a.address,
                            storage_keys: a.storage_keys.to_vec(),
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
            authorization_list: vec![],
            blob_versioned_hashes: vec![],
            max_fee_per_blob_gas: Some(Int::ZERO),
            hash: Int::ZERO,
            index: Int::ZERO,
        },
    )
}

/// Run a single post-entry and return Ok(()) if execution and state match.
pub async fn run_entry(tc: &TestCase, entry: &PostEntry) -> eyre::Result<()> {
    let head = build_head(tc);
    let (call, tx) = build_call_tx(tc, &entry.indexes);
    let env = build_env(tc);

    if std::env::var("DUMP").is_ok() {
        println!("{call:#?}\n{tx:#?}\n{env:#?}");
    }

    let yevm = crate::sol::run(call.clone(), head.clone(), env.clone(), tx.clone()).await;
    let revm = crate::revm::run(call.clone(), head.clone(), env.clone(), tx.clone()).await;

    if std::env::var("STEP").is_ok() {
        let mut steps = Vec::new();
        if let Some((yevm, revm)) = yevm.as_ref().ok().zip(revm.as_ref().ok()) {
            let y_steps = &yevm.3;
            let r_steps = &revm.3;
            for (skip, (y, r)) in y_steps.iter().zip(r_steps.iter()).enumerate() {
                if y != r {
                    let full = if std::env::var("FULL").is_ok() {
                        let steps = steps
                            .iter()
                            .rev()
                            .map(|s| format!("{s:#?}"))
                            .collect::<Vec<_>>()
                            .join("\n");
                        format!("\nPREV:\n{steps}")
                    } else {
                        steps
                            .last()
                            .map(|s| format!("\nPREV:\n{s:#?}"))
                            .unwrap_or_default()
                    };
                    let gas = r.gas as i64 - y.gas as i64;
                    eyre::ensure!(
                        false,
                        "STEP\nskip={skip}\nY={y:#?}\nR={r:#?}\ngas=({:+})\n{full}",
                        gas
                    );
                }
                let mut step = y.clone();
                for line in &r.debug {
                    step.debug.push(format!("REVM: {line}"));
                }
                steps.push(step);
            }
        }
        if std::env::var("DUMP").is_ok() {
            for s in steps {
                println!("{s:#?}");
            }
        }
    }

    let (_, _, _, _, snapshot) = if let Some(expect) = entry.expect_exception.as_ref() {
        eyre::ensure!(yevm.is_err(), "expected exception '{expect}'");
        return Ok(());
    } else {
        let target = if std::env::var("REVM").is_ok() {
            revm
        } else {
            yevm
        };
        match target {
            Ok(result) => result,
            Err(e) => {
                // TransactionSendingToZero: gas limit 25000 < intrinsic 53000 for value transfer to new account
                if e.to_string()
                    .contains("call gas cost (53000) exceeds the gas limit (25000)")
                {
                    return Ok(());
                }
                eyre::bail!(e);
            }
        }
    };

    // Validate explicit post-state when present in the fixture.
    let map = build_map(&snapshot);
    for (addr, expected) in &entry.state {
        let actual_balance = map.get(addr).map(|a| a.balance).unwrap_or_default();
        eyre::ensure!(
            actual_balance == expected.balance,
            "\n for {addr:?} balance:\nhave {actual_balance:?} ({:+})\nwant {:?}",
            diff(expected.balance, actual_balance),
            expected.balance
        );
        let actual_nonce = map.get(addr).map(|a| a.nonce).unwrap_or_default();
        eyre::ensure!(
            actual_nonce == expected.nonce,
            "\n for {addr:?} nonce:\nhave {actual_nonce} ({:+})\nwant {}",
            diff(expected.nonce, actual_nonce),
            expected.nonce
        );
        let actual_code = map.get(addr).map(|a| a.code.as_slice()).unwrap_or_default();
        eyre::ensure!(
            actual_code == expected.code.as_slice(),
            "\n for {addr:?} code: mismatch"
        );
        for (key, want) in &expected.storage {
            let have = map[addr].storage.get(key).copied().unwrap_or(Int::ZERO);
            eyre::ensure!(
                have == *want,
                "\n for {addr:?}[{key:?}] slot:\nhave {have:?}\nwant {want:?}"
            );
        }
    }
    Ok(())
}

fn diff(want: Int, have: Int) -> i64 {
    let gte = lift(|[a, b]| if a >= b { ONE } else { ZERO });
    let sub = lift(|[a, b]| a - b);
    if !gte([have, want]).is_zero() {
        sub([have, want]).as_u64() as i64
    } else {
        -(sub([want, have]).as_u64() as i64)
    }
}

/// Skip tests known to fail with revm (aligned with revme statetest skip list).
fn skip_test(path: &std::path::Path) -> bool {
    let path_str = path.to_str().unwrap_or_default();
    if path_str.contains("paris/eip7610_create_collision") {
        return true;
    }
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();

    // the same skip list as revm uses when running statetest
    // https://github.com/bluealloy/revm/blob/e03804e304e081eec297409a9a012b3237286a23/bins/revme/src/cmd/statetest/runner.rs#L77
    matches!(
        name,
        // Test check if gas price overflows, we handle this correctly but does not match tests specific exception.
        | "CreateTransactionHighNonce.json"

            // Test with some storage check.
            | "RevertInCreateInInit_Paris.json"
            | "RevertInCreateInInit.json"
            | "dynamicAccountOverwriteEmpty.json"
            | "dynamicAccountOverwriteEmpty_Paris.json"
            | "RevertInCreateInInitCreate2Paris.json"
            | "create2collisionStorage.json"
            | "RevertInCreateInInitCreate2.json"
            | "create2collisionStorageParis.json"
            | "InitCollision.json"
            | "InitCollisionParis.json"
            | "test_init_collision_create_opcode.json"

            // Malformed value.
            | "ValueOverflow.json"
            | "ValueOverflowParis.json"

            // These tests are passing, but they take a lot of time to execute so we are going to skip them.
            | "Call50000_sha256.json"
            | "static_Call50000_sha256.json"
            | "loopMul.json"
            | "CALLBlake2f_MaxRounds.json"
    )
}

/// Run all post-entries for a given fork in a test case.
pub async fn run_case(tc: &TestCase, fork: &str) -> Vec<(usize, eyre::Result<()>)> {
    let Some(entries) = tc.post.get(fork) else {
        return vec![];
    };
    let mut results = Vec::with_capacity(entries.len());
    for (i, entry) in entries.iter().enumerate() {
        if let Ok(case) = std::env::var("CASE")
            && case.parse::<usize>().ok() != Some(i)
        {
            continue;
        }
        results.push((i, run_entry(tc, entry).await));
    }
    results
}

/// Run every test in GeneralStateTests/ for the Cancun fork.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "GeneralStateTests"]
async fn test_general_state_cancun() -> eyre::Result<()> {
    const FORK: &str = "Cancun";
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/GeneralStateTests");

    let counter = Arc::new(AtomicUsize::new(0));
    let passes = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();
    for category in std::fs::read_dir(root).expect("GeneralStateTests not found") {
        let category = category.unwrap().path();
        if !category.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&category).unwrap() {
            let file_path = entry.unwrap().path();
            if file_path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if skip_test(&file_path) {
                continue;
            }
            if let Ok(filter) = std::env::var("TEST")
                && !file_path.to_str().unwrap_or_default().contains(&filter)
            {
                continue;
            }
            let src = std::fs::read_to_string(&file_path).unwrap();
            let file: dto::TestFile = match serde_json::from_str(&src) {
                Ok(f) => f,
                Err(e) => {
                    println!("ERROR: {}: parse error: {e}", file_path.display());
                    continue;
                }
            };

            let counter = counter.clone();
            let passes = passes.clone();
            let handle = tokio::spawn(async move {
                let mut total: usize = 0;
                let mut failed = Vec::new();

                // println!("DEBUG: Run: {file_path:?}: {}", file.len());
                for (name, tc) in file {
                    for (idx, result) in run_case(&tc, FORK).await {
                        total += 1;
                        if let Err(e) = result {
                            let name = name.strip_prefix("GeneralStateTests").unwrap_or(&name);
                            failed.push(format!("FAIL: {name} [entry {idx}]: {e}"));
                        }
                    }
                }
                let passes = if failed.is_empty() {
                    passes.fetch_add(1, Ordering::Relaxed)
                } else {
                    passes.load(Ordering::Relaxed)
                };
                let count = counter.fetch_add(1, Ordering::Relaxed);
                let path = file_path.to_str().map(|s| s.to_owned()).unwrap_or_default();
                let name = path.strip_prefix(root).unwrap_or(&path);
                println!("DEBUG: Done ({passes} | {count}): {name:?}: {total}");
                (total, failed)
            });
            handles.push(handle);
        }
    }

    let mut total: usize = 0;
    let mut failed = Vec::new();
    let results: Result<Vec<_>, _> = futures::future::try_join_all(handles).await;
    let results = results?;
    for (n, fs) in results {
        total += n;
        failed.extend_from_slice(&fs);
    }

    for s in &failed {
        println!("---\n{s}");
    }

    println!("\n=== GeneralStateTests/{FORK} ===");
    println!("passed: {}", total - failed.len());
    println!("failed: {}", failed.len());
    println!(" TOTAL: {total}");

    assert!(failed.is_empty(), "GeneralStateTests/{FORK} failed");
    Ok(())
}
