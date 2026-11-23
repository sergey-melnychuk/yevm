//! Execution-spec state tests (`fixtures/state_tests/for_<fork>`, execution-specs `tests@v21`)
//! run through yevm alone.
//!
//! Each post entry is one transaction on a pre-state. yevm leaves deciding whether a
//! transaction may run to its host, so the runner decides it first, as the specification does
//! ([`crate::validate`]): the entry passes when the runner rejects the transaction for the
//! reason the fixture expects, or when it accepts it and yevm produces the fixture's receipt
//! status and gas used, its logs (as the hash of their RLP list), and its post-state: every
//! listed account's balance, nonce, code and storage, and no other non-empty account.
//!
//! ```text
//! EEST_STATE=tmp/eest/v21.0.3/fixtures/state_tests EEST_FORK=Amsterdam \
//!     cargo test -p yevm-test --release eest -- --ignored --nocapture
//! ```
//!
//! `EEST_FILTER` keeps files whose path contains it, `EEST_SHOW` sets how many failures print
//! (default 20); every failure goes to `tmp/eest/state-<fork>.txt`.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
};

use serde::Deserialize;
use yevm_base::{Acc, Int};
use yevm_core::{
    Call, Head, Spec, Tx,
    cache::Cache,
    call::{AccessListItem, AuthorizationListItem, Block},
    chain::Chain,
    exe::{CallResult, Executor},
    state::Account,
};
use yevm_misc::{buf::Buf, keccak256};

/// The pre-state served the way a host serves chain state, so reverted accounts are read
/// back from it as they would be in production.
struct PreChain(HashMap<Acc, Account>, HashMap<Acc, BTreeMap<Int, Int>>);

impl PreChain {
    fn new(pre: &HashMap<Acc, State>) -> Self {
        let mut accounts = HashMap::new();
        let mut storage = HashMap::new();
        for (acc, s) in pre {
            let code = s.code.as_slice().to_vec();
            let hash = if code.is_empty() {
                Int::ZERO
            } else {
                Int::from(keccak256(&code).as_ref())
            };
            accounts.insert(
                *acc,
                Account {
                    value: s.balance,
                    nonce: s.nonce,
                    code: (code.into(), hash),
                },
            );
            storage.insert(*acc, s.storage.clone());
        }
        Self(accounts, storage)
    }
}

#[async_trait::async_trait]
impl Chain for PreChain {
    async fn get(&self, acc: &Acc, key: &Int) -> eyre::Result<Int> {
        Ok(self
            .1
            .get(acc)
            .and_then(|s| s.get(key))
            .copied()
            .unwrap_or_default())
    }
    async fn acc(&self, acc: &Acc) -> eyre::Result<Account> {
        Ok(self.0.get(acc).cloned().unwrap_or_default())
    }
    async fn code(&self, acc: &Acc) -> eyre::Result<(Buf, Int)> {
        Ok(self.0.get(acc).map(|a| a.code.clone()).unwrap_or_default())
    }
    async fn nonce(&self, acc: &Acc) -> eyre::Result<u64> {
        Ok(self
            .0
            .get(acc)
            .map(|a| a.nonce.as_u64())
            .unwrap_or_default())
    }
    async fn balance(&self, acc: &Acc) -> eyre::Result<Int> {
        Ok(self.0.get(acc).map(|a| a.value).unwrap_or_default())
    }
    async fn head(&self, number: u64) -> eyre::Result<Head> {
        eyre::bail!("no header {number} in a state test")
    }
    async fn block(&self, number: u64) -> eyre::Result<Block> {
        eyre::bail!("no block {number} in a state test")
    }
    async fn chain_id(&self) -> eyre::Result<u64> {
        Ok(1)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Case {
    pub env: Env,
    pub pre: HashMap<Acc, State>,
    pub transaction: Transaction,
    pub post: HashMap<String, Vec<Post>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Env {
    pub current_coinbase: Acc,
    pub current_gas_limit: Int,
    pub current_number: Int,
    pub current_timestamp: Int,
    pub current_base_fee: Option<Int>,
    pub current_random: Option<Int>,
    pub current_difficulty: Option<Int>,
    pub current_excess_blob_gas: Option<Int>,
    pub slot_number: Option<Int>,
}

#[derive(Deserialize)]
pub struct State {
    pub balance: Int,
    pub code: Buf,
    pub nonce: Int,
    pub storage: BTreeMap<Int, Int>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transaction {
    pub chain_id: Option<Int>,
    pub data: Vec<Buf>,
    pub gas_limit: Vec<Int>,
    pub gas_price: Option<Int>,
    pub max_fee_per_gas: Option<Int>,
    pub max_priority_fee_per_gas: Option<Int>,
    pub max_fee_per_blob_gas: Option<Int>,
    pub nonce: Int,
    pub sender: Acc,
    #[serde(deserialize_with = "to")]
    pub to: Option<Acc>,
    pub value: Vec<Int>,
    #[serde(default)]
    pub access_lists: Option<Vec<Option<Vec<AccessList>>>>,
    #[serde(default)]
    pub authorization_list: Option<Vec<Authorization>>,
    #[serde(default)]
    pub blob_versioned_hashes: Option<Vec<Int>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessList {
    pub address: Acc,
    #[serde(default)]
    pub storage_keys: Vec<Int>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Authorization {
    pub chain_id: Int,
    pub address: Acc,
    pub nonce: Int,
    pub r: Int,
    pub s: Int,
    pub y_parity: Int,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Post {
    pub indexes: Indexes,
    pub logs: Int,
    #[serde(default)]
    pub state: HashMap<Acc, State>,
    pub expect_exception: Option<String>,
    pub receipt: Option<Receipt>,
    /// The signed transaction, for its signature and chain ID.
    pub txbytes: Buf,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Receipt {
    #[serde(deserialize_with = "status")]
    pub status: Int,
    pub cumulative_gas_used: Int,
}

#[derive(Deserialize)]
pub struct Indexes {
    pub data: usize,
    pub gas: usize,
    pub value: usize,
}

/// A transaction's recipient; empty for contract creation.
fn to<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Acc>, D::Error> {
    match Option::<String>::deserialize(d)? {
        None => Ok(None),
        Some(s) if s.is_empty() => Ok(None),
        Some(s) => serde_json::from_value(serde_json::Value::String(s))
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

/// A receipt status, written as `true`/`false` or as a hex number.
fn status<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Int, D::Error> {
    match serde_json::Value::deserialize(d)? {
        serde_json::Value::Bool(ok) => Ok(if ok { Int::ONE } else { Int::ZERO }),
        v => serde_json::from_value(v).map_err(serde::de::Error::custom),
    }
}

pub fn spec(fork: &str) -> Option<Spec> {
    match fork {
        "Osaka" => Some(Spec::Osaka),
        "Amsterdam" => Some(Spec::Amsterdam),
        _ => None,
    }
}

/// EIP-4844 `fake_exponential(1, excess, fraction)`.
fn blob_base_fee(excess: u64, fraction: u128) -> u128 {
    let (mut i, mut out, mut acc) = (1u128, 0u128, fraction);
    while acc > 0 {
        out += acc;
        acc = acc * excess as u128 / (fraction * i);
        i += 1;
    }
    out / fraction
}

fn head(env: &Env, spec: Spec) -> Head {
    let random = env
        .current_random
        .or(env.current_difficulty)
        .unwrap_or_default();
    // The fixtures' blob schedule: Prague's fraction through Osaka, BPO2's at Amsterdam.
    let fraction = match spec {
        Spec::Osaka => 5_007_716,
        Spec::Amsterdam => 11_684_671,
    };
    let excess = env.current_excess_blob_gas;
    Head {
        number: env.current_number.as_u64().into(),
        hash: random,
        gas_limit: env.current_gas_limit,
        coinbase: env.current_coinbase,
        timestamp: env.current_timestamp,
        base_fee: env.current_base_fee.unwrap_or_default(),
        excess_blob_gas: excess,
        blob_base_fee: excess.map(|e| Int::from(blob_base_fee(e.as_u64(), fraction))),
        prevrandao: random,
        slot_number: env.slot_number,
        spec,
        ..Head::default()
    }
}

fn call_and_tx(t: &Transaction, i: &Indexes) -> (Call, Tx) {
    let call = Call {
        by: t.sender,
        to: t.to,
        gas: t.gas_limit[i.gas].as_u64(),
        eth: t.value[i.value],
        data: t.data[i.data].as_slice().to_vec().into(),
    };
    let access_list = t
        .access_lists
        .as_ref()
        .and_then(|lists| lists.get(i.data))
        .and_then(Option::as_ref)
        .map(|list| {
            list.iter()
                .map(|a| AccessListItem {
                    address: a.address,
                    storage_keys: a.storage_keys.clone(),
                })
                .collect()
        })
        .unwrap_or_default();
    let authorization_list = t
        .authorization_list
        .iter()
        .flatten()
        .map(|a| AuthorizationListItem {
            address: a.address,
            chain_id: a.chain_id,
            nonce: a.nonce,
            r: a.r,
            s: a.s,
            y_parity: a.y_parity,
        })
        .collect();
    let tx = Tx {
        chain_id: t.chain_id.unwrap_or(Int::ONE).as_u64().into(),
        nonce: t.nonce,
        gas_price: t.gas_price.unwrap_or_default(),
        max_fee_per_gas: t.max_fee_per_gas.unwrap_or_default(),
        max_priority_fee_per_gas: t.max_priority_fee_per_gas.unwrap_or_default(),
        access_list,
        authorization_list,
        blob_versioned_hashes: t.blob_versioned_hashes.clone().unwrap_or_default(),
        max_fee_per_blob_gas: t.max_fee_per_blob_gas,
        hash: Int::ZERO,
        index: Int::ZERO,
    };
    (call, tx)
}

fn rlp_bytes(out: &mut Vec<u8>, b: &[u8]) {
    if b.len() == 1 && b[0] < 0x80 {
        out.push(b[0]);
    } else {
        rlp_len(out, b.len(), 0x80);
        out.extend_from_slice(b);
    }
}

fn rlp_len(out: &mut Vec<u8>, len: usize, base: u8) {
    if len < 56 {
        out.push(base + len as u8);
    } else {
        let be = len.to_be_bytes();
        let be = &be[be.iter().position(|b| *b != 0).unwrap_or(be.len() - 1)..];
        out.push(base + 55 + be.len() as u8);
        out.extend_from_slice(be);
    }
}

fn rlp_list(items: &[Vec<u8>]) -> Vec<u8> {
    let body: Vec<u8> = items.concat();
    let mut out = Vec::with_capacity(body.len() + 9);
    rlp_len(&mut out, body.len(), 0xc0);
    out.extend_from_slice(&body);
    out
}

/// `keccak(rlp([[address, [topics...], data], ...]))`, the fixtures' `logs` hash.
fn logs_hash(logs: &[(Acc, Buf, Vec<Int>)]) -> Int {
    let items: Vec<Vec<u8>> = logs
        .iter()
        .map(|(acc, data, topics)| {
            let mut a = Vec::new();
            rlp_bytes(&mut a, acc.as_ref());
            let topics: Vec<Vec<u8>> = topics
                .iter()
                .map(|t| {
                    let mut v = Vec::new();
                    rlp_bytes(&mut v, t.as_ref());
                    v
                })
                .collect();
            let mut d = Vec::new();
            rlp_bytes(&mut d, data.as_slice());
            rlp_list(&[a, rlp_list(&topics), d])
        })
        .collect();
    Int::from(keccak256(&rlp_list(&items)).as_ref())
}

/// Runs one post entry; `Ok` when yevm matches it.
pub async fn run(case: &Case, spec: Spec, post: &Post) -> eyre::Result<()> {
    let head = head(&case.env, spec);
    let (call, tx) = call_and_tx(&case.transaction, &post.indexes);

    match (
        crate::validate::validate(case, post, &head),
        &post.expect_exception,
    ) {
        (Ok(()), None) => {}
        (Err(have), Some(want)) => {
            let expected = want
                .split('|')
                .any(|w| w.strip_prefix("TransactionException.") == Some(have));
            eyre::ensure!(expected, "rejected as {have}, the fixture expects {want}");
            return Ok(());
        }
        (Err(have), None) => eyre::bail!("rejected as {have}, the fixture accepts it"),
        (Ok(()), Some(want)) => eyre::bail!("accepted, the fixture expects {want}"),
    }

    let chain = PreChain::new(&case.pre);
    let mut state = Cache::new();
    let mut exe = Executor::new(call).with_max_steps(None);
    let result = match exe.run(&tx, &head, &mut state, &chain).await {
        Ok(result) => result,
        Err(e) => eyre::bail!("yevm rejected a valid transaction: {e}"),
    };

    if let Some(receipt) = &post.receipt {
        let status = match &result {
            CallResult::Done { status, .. } => *status,
            CallResult::Created { .. } => Int::ONE,
        };
        eyre::ensure!(
            status == receipt.status,
            "status: have {status:?}, want {:?}",
            receipt.status
        );
        let have = result.gas().finalized as u64;
        let want = receipt.cumulative_gas_used.as_u64();
        eyre::ensure!(
            have == want,
            "gas used: have {have}, want {want} ({:+})",
            have as i64 - want as i64
        );
    }

    let logs = logs_hash(&state.logs);
    eyre::ensure!(
        logs == post.logs,
        "logs hash differs ({} logs)",
        state.logs.len()
    );

    // The post-state: the pre-state with the cache's accounts and slots over it.
    let mut have: HashMap<Acc, (Account, BTreeMap<Int, Int>)> = chain
        .0
        .iter()
        .map(|(acc, account)| (*acc, (account.clone(), chain.1[acc].clone())))
        .collect();
    for (acc, account, storage) in state.snapshot() {
        let entry = have.entry(acc).or_default();
        entry.0 = account;
        entry.1.extend(storage);
    }
    for (_, storage) in have.values_mut() {
        storage.retain(|_, v| !v.is_zero());
    }
    let mut diffs = Vec::new();
    for (acc, want) in &post.state {
        let Some((account, storage)) = have.remove(acc) else {
            let empty = want.balance.is_zero()
                && want.nonce.is_zero()
                && want.code.as_slice().is_empty()
                && want.storage.values().all(Int::is_zero);
            if !empty {
                diffs.push(format!("{acc:?}: missing"));
            }
            continue;
        };
        if account.value != want.balance {
            diffs.push(format!(
                "{acc:?} balance: have {:?}, want {:?}",
                account.value, want.balance
            ));
        }
        if account.nonce != want.nonce {
            diffs.push(format!(
                "{acc:?} nonce: have {:?}, want {:?}",
                account.nonce, want.nonce
            ));
        }
        if account.code.0.as_slice() != want.code.as_slice() {
            diffs.push(format!(
                "{acc:?} code: have {}, want {}",
                hex::encode(account.code.0.as_slice()),
                hex::encode(want.code.as_slice())
            ));
        }
        let want_storage: BTreeMap<Int, Int> = want
            .storage
            .iter()
            .filter(|(_, v)| !v.is_zero())
            .map(|(k, v)| (*k, *v))
            .collect();
        if storage != want_storage {
            for (k, v) in want_storage.iter() {
                if storage.get(k) != Some(v) {
                    diffs.push(format!(
                        "{acc:?}[{k:?}]: have {:?}, want {v:?}",
                        storage.get(k)
                    ));
                }
            }
            for (k, v) in storage.iter() {
                if !want_storage.contains_key(k) {
                    diffs.push(format!("{acc:?}[{k:?}]: have {v:?}, want none"));
                }
            }
        }
    }
    for (acc, (account, storage)) in have {
        let empty = account.value.is_zero()
            && account.nonce.is_zero()
            && account.code.0.is_empty()
            && storage.is_empty();
        if !empty {
            diffs.push(format!("{acc:?}: present, want none"));
        }
    }
    eyre::ensure!(diffs.is_empty(), "post-state:\n  {}", diffs.join("\n  "));
    Ok(())
}

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files(&path, out);
        } else if path.extension().is_some_and(|e| e == "json") {
            out.push(path);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the execution-spec fixtures (see etc/AMSTERDAM.md)"]
async fn eest_state() -> eyre::Result<()> {
    let root = std::env::var("EEST_STATE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../tmp/eest/v21.0.3/fixtures/state_tests")
        });
    let fork = std::env::var("EEST_FORK").unwrap_or_else(|_| "Amsterdam".into());
    let spec = spec(&fork).ok_or_else(|| eyre::eyre!("unknown fork {fork}"))?;
    let filter = std::env::var("EEST_FILTER").ok();
    let show: usize = std::env::var("EEST_SHOW")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);

    let dir = root.join(format!("for_{}", fork.to_lowercase()));
    let mut paths = Vec::new();
    files(&dir, &mut paths);
    paths.sort();
    paths.retain(|p| {
        filter
            .as_ref()
            .is_none_or(|f| p.to_string_lossy().contains(f.as_str()))
    });
    eyre::ensure!(!paths.is_empty(), "no fixtures under {}", dir.display());

    let started = std::time::Instant::now();
    let mut handles = Vec::new();
    for path in paths {
        let fork = fork.clone();
        let dir = dir.clone();
        handles.push(tokio::spawn(async move {
            let name = path
                .strip_prefix(&dir)
                .unwrap_or(&path)
                .display()
                .to_string();
            let mut out = (0usize, Vec::new());
            let cases: BTreeMap<String, Case> =
                match std::fs::read_to_string(&path).map(|s| serde_json::from_str(&s)) {
                    Ok(Ok(cases)) => cases,
                    Ok(Err(e)) => {
                        out.0 += 1;
                        out.1.push(format!("{name}: parse: {e}"));
                        return out;
                    }
                    Err(e) => {
                        out.0 += 1;
                        out.1.push(format!("{name}: read: {e}"));
                        return out;
                    }
                };
            for (id, case) in &cases {
                let id = id.rsplit("::").next().unwrap_or(id);
                for (i, post) in case.post.get(&fork).into_iter().flatten().enumerate() {
                    out.0 += 1;
                    let result = std::panic::AssertUnwindSafe(run(case, spec, post));
                    let result = futures::FutureExt::catch_unwind(result).await;
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => out.1.push(format!("{name} {id} [{i}]: {e}")),
                        Err(_) => out.1.push(format!("{name} {id} [{i}]: panic")),
                    }
                }
            }
            out
        }));
    }

    let (mut total, mut failed) = (0, Vec::new());
    for handle in handles {
        let (n, f) = handle.await?;
        total += n;
        failed.extend(f);
    }
    failed.sort();
    let report = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(format!("../tmp/eest/state-{}.txt", fork.to_lowercase()));
    let _ = std::fs::write(&report, failed.join("\n---\n"));
    for f in failed.iter().take(show) {
        println!("---\n{f}");
    }
    println!("\n=== EEST state tests, yevm on {fork} ===");
    println!("passed: {}", total - failed.len());
    println!("failed: {}", failed.len());
    println!(" TOTAL: {total}");
    println!("  time: {:.0?}", started.elapsed());
    println!("report: {}", report.display());
    Ok(())
}
