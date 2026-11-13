//! Ethereum execution-spec-tests (EEST) blockchain tests, executed with yevm.
//!
//! Each case imports its blocks one at a time into an in-memory world state, with reth's block
//! executor running [`YevmEvmConfig`], the configuration the `node` binary executes blocks with.
//! A block is accepted only if it passes, in order:
//!
//! 1. RLP decoding (with no trailing bytes) and sender recovery;
//! 2. reth's Ethereum consensus checks before execution: the header on its own and against its
//!    parent, and the body against the header;
//! 3. execution;
//! 4. reth's consensus checks after execution: gas used, receipts root, logs bloom, requests hash;
//! 5. the state root, computed here with `alloy-trie` from the full world state, independently of
//!    reth's trie code.
//!
//! A block the fixture marks with `expectException` must be rejected; it is dropped and the next
//! block builds on its parent. Every other block must be accepted. Afterwards the head must be the
//! fixture's `lastblockhash`, and the world state must equal its `postState` exactly: the same
//! accounts, and the same nonce, balance, code and storage in each.
//!
//! yevm implements only Osaka, so only the Osaka cases and the transitions between the
//! blob-parameter-only forks after it run (see [`NETWORKS`]). Every EEST state test is also filled
//! as a blockchain test, so this covers the state tests as well. Cases listed in
//! `eest-known-failures.list` fail the same way on evm2 and don't fail the run.
//!
//! ```text
//! etc/eest.sh                                   # download the fixtures into tmp/eest/
//! cargo test -p yevm-reth --profile eest --test eest -- --ignored --nocapture
//! ```
//!
//! Environment variables:
//! - `EEST_FIXTURES`: the fixtures directory (default `tmp/eest/v5.4.0-develop/fixtures`).
//! - `EEST_EVM`: `yevm` (default), or `evm2` to run the same cases on reth's own EVM, which
//!   checks the harness itself.
//! - `EEST_FILTER`: only run cases whose file path or name contains this string.
//! - `EEST_REPORT`: where to write the list of failures (default `tmp/eest/report-<evm>.txt`).
//! - `EEST_TRACE`: print each case to stderr as it starts and ends.
//! - `EEST_THREADS`: how many fixture files run at once (default: one per CPU).

use alloy_eips::eip7840::BlobParams;
use alloy_primitives::{Address, B256, Bytes, U64, U256, keccak256};
use alloy_trie::{
    KECCAK_EMPTY, TrieAccount,
    root::{state_root_unhashed, storage_root_unhashed},
};
use evm2::{
    bytecode::Bytecode,
    evm::{AccountInfo, Database},
};
use reth_chainspec::{ChainSpec, ChainSpecBuilder};
use reth_ethereum::consensus::{Consensus, EthBeaconConsensus, FullConsensus, HeaderValidator};
use reth_ethereum_primitives::{Block, EthPrimitives};
use reth_evm::{BundleState, ConfigureEvm, execute::Executor};
use reth_evm_ethereum::EthEvmConfig;
use reth_primitives_traits::{SealedBlock, SealedHeader};
use serde::Deserialize;
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt::Write as _,
    fs, io,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};
use yevm_reth::exec::YevmEvmConfig;

/// The networks that run. yevm implements only the Osaka rules; the blob-parameter-only forks
/// after Osaka (EIP-7892) change only the blob schedule, so their transitions run too.
const NETWORKS: &[&str] = &[
    "Osaka",
    "OsakaToBPO1AtTime15k",
    "BPO1ToBPO2AtTime15k",
    "BPO2ToBPO3AtTime15k",
    "BPO3ToBPO4AtTime15k",
];

/// The timestamp at which the `<A>To<B>AtTime15k` networks switch to `B`.
const TRANSITION_TIME: u64 = 15_000;

/// Cases that fail on evm2, reth's own EVM, in the same way. See the file for why.
const KNOWN_FAILURES: &str = include_str!("eest-known-failures.list");

/// The parts of an EEST blockchain test fixture this runner reads.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    #[serde(rename = "genesisRLP")]
    genesis_rlp: Bytes,
    pre: BTreeMap<Address, FixtureAccount>,
    post_state: Option<BTreeMap<Address, FixtureAccount>>,
    post_state_hash: Option<B256>,
    #[serde(rename = "lastblockhash")]
    last_block_hash: B256,
    blocks: Vec<FixtureBlock>,
    config: Option<FixtureConfig>,
}

#[derive(Deserialize)]
struct FixtureAccount {
    nonce: U64,
    balance: U256,
    code: Bytes,
    storage: BTreeMap<U256, U256>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureBlock {
    rlp: Bytes,
    /// Set if the block is invalid: the reason, as the fixture's filler named it.
    expect_exception: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureConfig {
    /// Blob parameters by fork name (`Osaka`, `BPO1`, ...).
    #[serde(default)]
    blob_schedule: BTreeMap<String, FixtureBlobParams>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureBlobParams {
    target: U64,
    max: U64,
    base_fee_update_fraction: U64,
}

/// The world state of a running case: the fixture's `pre`, then each accepted block's changes.
#[derive(Clone, Debug)]
struct World {
    accounts: BTreeMap<Address, WorldAccount>,
    /// Bytecode by hash. Never pruned: a hash may be looked up after its account is gone.
    code: HashMap<B256, Bytecode>,
    /// Hashes of the genesis and the accepted blocks, by number, for `BLOCKHASH`.
    block_hashes: HashMap<u64, B256>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct WorldAccount {
    nonce: u64,
    balance: U256,
    code_hash: B256,
    /// Non-zero slots only.
    storage: BTreeMap<U256, U256>,
}

impl FixtureAccount {
    fn to_world(&self) -> WorldAccount {
        WorldAccount {
            nonce: self.nonce.to(),
            balance: self.balance,
            code_hash: keccak256(&self.code),
            storage: self
                .storage
                .iter()
                .filter(|(_, value)| !value.is_zero())
                .map(|(slot, value)| (*slot, *value))
                .collect(),
        }
    }
}

impl World {
    fn new(pre: &BTreeMap<Address, FixtureAccount>) -> Self {
        let mut world = Self {
            accounts: BTreeMap::new(),
            code: HashMap::from([(KECCAK_EMPTY, Bytecode::default())]),
            block_hashes: HashMap::new(),
        };
        for (address, account) in pre {
            let account_state = account.to_world();
            world
                .code
                .entry(account_state.code_hash)
                .or_insert_with(|| Bytecode::new_raw(account.code.clone()));
            world.accounts.insert(*address, account_state);
        }
        world
    }

    /// Applies a block's state changes.
    fn apply(&mut self, changes: &BundleState) {
        for (hash, code) in &changes.contracts {
            self.code.entry(*hash).or_insert_with(|| code.clone());
        }
        for (address, change) in &changes.state {
            let Some(info) = &change.info else {
                self.accounts.remove(address);
                continue;
            };
            if let Some(code) = &info.code {
                self.code
                    .entry(info.code_hash)
                    .or_insert_with(|| code.clone());
            }
            let account = self.accounts.entry(*address).or_default();
            if change.was_destroyed() {
                account.storage.clear();
            }
            account.nonce = info.nonce;
            account.balance = info.balance;
            account.code_hash = info.code_hash;
            for (slot, value) in &change.storage {
                if value.present_value.is_zero() {
                    account.storage.remove(slot);
                } else {
                    account.storage.insert(*slot, value.present_value);
                }
            }
        }
    }

    fn state_root(&self) -> B256 {
        state_root_unhashed(self.accounts.iter().map(|(address, account)| {
            let storage_root = storage_root_unhashed(
                account
                    .storage
                    .iter()
                    .map(|(slot, value)| (B256::from(*slot), *value)),
            );
            let leaf = TrieAccount::new(
                account.nonce,
                account.balance,
                storage_root,
                account.code_hash,
            );
            (*address, leaf)
        }))
    }

    /// Checks that the world state is exactly `expected`.
    fn check(&self, expected: &BTreeMap<Address, FixtureAccount>) -> Result<(), String> {
        if let Some(address) = self.accounts.keys().find(|a| !expected.contains_key(*a)) {
            return Err(format!("post-state: unexpected account {address}"));
        }
        for (address, account) in expected {
            let want = account.to_world();
            match self.accounts.get(address) {
                None => return Err(format!("post-state: missing account {address}")),
                Some(have) if *have != want => {
                    return Err(format!(
                        "post-state of {address}: {}",
                        account_diff(have, &want)
                    ));
                }
                Some(_) => {}
            }
        }
        Ok(())
    }
}

/// The differences between two accounts, as `field: have X, want Y` items.
fn account_diff(have: &WorldAccount, want: &WorldAccount) -> String {
    let mut diff = Vec::new();
    if have.nonce != want.nonce {
        diff.push(format!("nonce: have {}, want {}", have.nonce, want.nonce));
    }
    if have.balance != want.balance {
        diff.push(format!(
            "balance: have {}, want {}",
            have.balance, want.balance
        ));
    }
    if have.code_hash != want.code_hash {
        diff.push(format!(
            "code hash: have {}, want {}",
            have.code_hash, want.code_hash
        ));
    }
    let slots: BTreeSet<_> = have.storage.keys().chain(want.storage.keys()).collect();
    for slot in slots {
        let (h, w) = (have.storage.get(slot), want.storage.get(slot));
        if h != w {
            let (h, w) = (
                h.copied().unwrap_or_default(),
                w.copied().unwrap_or_default(),
            );
            diff.push(format!("slot {slot:#x}: have {h:#x}, want {w:#x}"));
        }
    }
    diff.join(", ")
}

/// The block executor reads the world state through this, as a node reads its database.
impl Database for &World {
    type Error = io::Error;

    fn get_account(&mut self, address: &Address) -> Result<Option<AccountInfo>, Self::Error> {
        Ok(self.accounts.get(address).map(|account| {
            let code = self.code.get(&account.code_hash).cloned();
            AccountInfo::new(account.balance, account.nonce, account.code_hash, code)
        }))
    }

    fn get_code_by_hash(&mut self, code_hash: &B256) -> Result<Bytecode, Self::Error> {
        self.code
            .get(code_hash)
            .cloned()
            .ok_or_else(|| io::Error::other(format!("no code with hash {code_hash}")))
    }

    fn get_storage(&mut self, address: &Address, key: &U256) -> Result<U256, Self::Error> {
        Ok(self
            .accounts
            .get(address)
            .and_then(|account| account.storage.get(key))
            .copied()
            .unwrap_or_default())
    }

    fn get_block_hash(&mut self, number: &U256) -> Result<B256, Self::Error> {
        u64::try_from(*number)
            .ok()
            .and_then(|number| self.block_hashes.get(&number))
            .copied()
            .ok_or_else(|| io::Error::other(format!("no block {number}")))
    }
}

/// The EVM that executes the blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Evm {
    Yevm,
    Evm2,
}

impl Evm {
    fn from_env() -> Self {
        match std::env::var("EEST_EVM").as_deref() {
            Err(_) | Ok("yevm") => Self::Yevm,
            Ok("evm2") => Self::Evm2,
            Ok(other) => panic!("EEST_EVM must be `yevm` or `evm2`, not `{other}`"),
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Yevm => "yevm",
            Self::Evm2 => "evm2",
        }
    }
}

/// How one case ended.
enum Outcome {
    Pass,
    Fail(String),
    Panic(String),
}

/// One case that ran.
struct CaseResult {
    /// The fixture file, relative to `blockchain_tests/`.
    file: String,
    name: String,
    outcome: Outcome,
}

/// The results of one fixture file.
#[derive(Default)]
struct FileResults {
    cases: Vec<CaseResult>,
    /// Cases that did not run, by network.
    not_run: HashMap<String, usize>,
}

#[test]
#[ignore = "needs the EEST fixtures: run etc/eest.sh first"]
fn eest_blockchain_tests() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let fixtures = std::env::var_os("EEST_FIXTURES")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("tmp/eest/v5.4.0-develop/fixtures"));
    let root = fixtures.join("blockchain_tests");
    assert!(
        root.is_dir(),
        "{} not found: run etc/eest.sh",
        root.display()
    );

    let evm = Evm::from_env();
    let filter = std::env::var("EEST_FILTER").ok();
    let report = std::env::var_os("EEST_REPORT")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join(format!("tmp/eest/report-{}.txt", evm.name())));

    let mut files = Vec::new();
    collect_json(&root, &mut files);
    files.sort();

    println!(
        "EEST blockchain tests: {} files under {}, evm={}",
        files.len(),
        root.display(),
        evm.name()
    );
    let started = Instant::now();

    // Report panics as case failures, with the location, instead of printing each one.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|info| {
        LAST_PANIC.with(|p| *p.borrow_mut() = Some(info.to_string()));
    }));
    let results = run_files(&root, &files, evm, filter.as_deref());
    std::panic::set_hook(default_hook);

    summarize(&results, evm, &report, started);
}

/// Runs the files on plain OS threads, `EEST_THREADS` of them (default: one per CPU).
///
/// Not on rayon: reth may use rayon inside block execution, and a rayon worker that waits on that
/// work steals queued tasks onto its own stack. With the files as rayon tasks, each waiting case
/// could start another file's case on top of itself until the stack overflows. Callers outside
/// the pool block instead.
fn run_files(root: &Path, files: &[PathBuf], evm: Evm, filter: Option<&str>) -> Vec<FileResults> {
    let threads = std::env::var("EEST_THREADS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
    let next = AtomicUsize::new(0);
    let results = Mutex::new(Vec::with_capacity(files.len()));
    std::thread::scope(|scope| {
        for i in 0..threads {
            std::thread::Builder::new()
                .name(format!("eest-{i}"))
                .spawn_scoped(scope, || {
                    while let Some(path) = files.get(next.fetch_add(1, Ordering::Relaxed)) {
                        let file = run_file(root, path, evm, filter);
                        results.lock().unwrap().push(file);
                    }
                })
                .expect("failed to spawn a test thread");
        }
    });
    results.into_inner().unwrap()
}

thread_local! {
    static LAST_PANIC: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn collect_json(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_json(&path, out);
        } else if path.extension().is_some_and(|e| e == "json") {
            out.push(path);
        }
    }
}

fn run_file(root: &Path, path: &Path, evm: Evm, filter: Option<&str>) -> FileResults {
    let file = path
        .strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string();
    let trace = std::env::var_os("EEST_TRACE").is_some();
    let mut results = FileResults::default();
    let fail = |name: &str, msg: String| CaseResult {
        file: file.clone(),
        name: name.to_owned(),
        outcome: Outcome::Fail(msg),
    };

    // Look at each case's network before parsing the rest: most cases in the fixtures are for
    // other forks, and those must not fail the file.
    let cases: BTreeMap<String, serde_json::Value> = match fs::read_to_string(path)
        .map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
    {
        Ok(cases) => cases,
        Err(e) => {
            results
                .cases
                .push(fail("<file>", format!("cannot read fixture: {e}")));
            return results;
        }
    };

    for (name, value) in cases {
        let network = value
            .get("network")
            .and_then(|n| n.as_str())
            .unwrap_or("<none>");
        if !NETWORKS.contains(&network) {
            *results.not_run.entry(network.to_owned()).or_default() += 1;
            continue;
        }
        if let Some(filter) = filter
            && !file.contains(filter)
            && !name.contains(filter)
        {
            continue;
        }
        let fixture = match Fixture::deserialize(&value) {
            Ok(fixture) => fixture,
            Err(e) => {
                results
                    .cases
                    .push(fail(&name, format!("cannot parse case: {e}")));
                continue;
            }
        };
        let chain_spec = match chain_spec(network, fixture.config.as_ref()) {
            Ok(spec) => spec,
            Err(e) => {
                results.cases.push(fail(&name, e));
                continue;
            }
        };
        // A stack overflow aborts the process without unwinding; this shows which case it was.
        if trace {
            eprintln!("start {file}::{name}");
        }
        let run = || match evm {
            Evm::Yevm => run_case(&fixture, chain_spec.clone(), YevmEvmConfig::new(chain_spec)),
            Evm::Evm2 => run_case(&fixture, chain_spec.clone(), EthEvmConfig::new(chain_spec)),
        };
        let outcome = match catch_unwind(AssertUnwindSafe(run)) {
            Ok(Ok(())) => Outcome::Pass,
            Ok(Err(e)) => Outcome::Fail(e),
            Err(payload) => Outcome::Panic(
                LAST_PANIC
                    .with(|p| p.borrow_mut().take())
                    .unwrap_or_else(|| panic_message(&payload)),
            ),
        };
        if trace {
            eprintln!("end {file}::{name}");
        }
        results.cases.push(CaseResult {
            file: file.clone(),
            name,
            outcome,
        });
    }
    results
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panic with a non-string payload".to_owned())
}

/// The chain spec for a network: every fork up to Osaka from genesis. For `<A>To<B>AtTime15k`, the
/// blob parameters are `A`'s from genesis and `B`'s from [`TRANSITION_TIME`], as given in the
/// fixture's `config.blobSchedule`.
fn chain_spec(network: &str, config: Option<&FixtureConfig>) -> Result<Arc<ChainSpec>, String> {
    let mut spec = ChainSpecBuilder::mainnet()
        .reset()
        .osaka_activated()
        .build();
    if network != "Osaka" {
        let (from, to) = network
            .strip_suffix("AtTime15k")
            .and_then(|n| n.split_once("To"))
            .ok_or_else(|| format!("unknown network {network}"))?;
        let params = |fork: &str| {
            let fixture = config
                .and_then(|c| c.blob_schedule.get(fork))
                .ok_or_else(|| format!("{network}: no blob parameters for {fork}"))?;
            Ok::<_, String>(BlobParams {
                target_blob_count: fixture.target.to(),
                max_blob_count: fixture.max.to(),
                update_fraction: fixture.base_fee_update_fraction.to(),
                ..BlobParams::osaka()
            })
        };
        let mut scheduled = vec![(TRANSITION_TIME, params(to)?)];
        if from != "Osaka" {
            scheduled.insert(0, (0, params(from)?));
        }
        spec.blob_params = spec.blob_params.with_scheduled(scheduled);
    }
    Ok(Arc::new(spec))
}

/// Imports the case's blocks and checks where it ends up.
fn run_case<E>(fixture: &Fixture, chain_spec: Arc<ChainSpec>, evm_config: E) -> Result<(), String>
where
    E: ConfigureEvm<Primitives = EthPrimitives>,
{
    let consensus = EthBeaconConsensus::new(chain_spec);
    let genesis = decode_block(&fixture.genesis_rlp).map_err(|e| format!("genesis: {e}"))?;
    let mut world = World::new(&fixture.pre);
    let pre_state_root = world.state_root();
    if pre_state_root != genesis.header().state_root {
        return Err(format!(
            "pre-state root {pre_state_root}, but the genesis header has {}",
            genesis.header().state_root
        ));
    }
    world
        .block_hashes
        .insert(genesis.header().number, genesis.hash());
    let mut head = genesis.clone_sealed_header();

    for (index, block) in fixture.blocks.iter().enumerate() {
        // The position in the fixture: an invalid block may carry any number.
        let position = index + 1;
        let imported = import_block(&evm_config, &consensus, &world, &head, &block.rlp);
        match (imported, &block.expect_exception) {
            (Ok((header, next)), None) => {
                head = header;
                world = next;
            }
            (Err(_), Some(_)) => {}
            (Ok(_), Some(exception)) => {
                return Err(format!(
                    "block {position} was accepted, but must be rejected ({exception})"
                ));
            }
            (Err(error), None) => return Err(format!("block {position} rejected: {error}")),
        }
    }

    if head.hash() != fixture.last_block_hash {
        return Err(format!(
            "head is {}, but the fixture's last block is {}",
            head.hash(),
            fixture.last_block_hash
        ));
    }
    match (&fixture.post_state, fixture.post_state_hash) {
        (Some(post_state), _) => world.check(post_state),
        (None, Some(root)) if world.state_root() == root => Ok(()),
        (None, Some(root)) => Err(format!(
            "post-state root {}, want {root}",
            world.state_root()
        )),
        (None, None) => Err("the fixture has neither postState nor postStateHash".to_owned()),
    }
}

/// Imports one block on top of `parent` and `world`: returns its header and the world state after
/// it, or why the block is invalid.
fn import_block<E>(
    evm_config: &E,
    consensus: &EthBeaconConsensus<ChainSpec>,
    world: &World,
    parent: &SealedHeader,
    rlp: &[u8],
) -> Result<(SealedHeader, World), String>
where
    E: ConfigureEvm<Primitives = EthPrimitives>,
{
    let block = decode_block(rlp)?;
    let header = block.sealed_header();
    consensus
        .validate_header(header)
        .map_err(|e| format!("header: {e}"))?;
    consensus
        .validate_header_against_parent(header, parent)
        .map_err(|e| format!("header against parent: {e}"))?;
    Consensus::<Block>::validate_body_against_header(consensus, block.body(), header)
        .map_err(|e| format!("body against header: {e}"))?;
    consensus
        .validate_block_pre_execution(&block)
        .map_err(|e| format!("block before execution: {e}"))?;
    let block = block
        .try_recover()
        .map_err(|e| format!("sender recovery: {e}"))?;

    let mut executor = evm_config.batch_executor(world);
    let result = executor
        .execute_one(&block)
        .map_err(|e| format!("execution: {e}"))?;
    let changes = executor.into_state();
    FullConsensus::<EthPrimitives>::validate_block_post_execution(
        consensus, &block, &result, None, None,
    )
    .map_err(|e| format!("block after execution: {e}"))?;

    let mut next = world.clone();
    next.apply(&changes);
    let state_root = next.state_root();
    if state_root != block.header().state_root {
        return Err(format!(
            "state root {state_root}, but the header has {}",
            block.header().state_root
        ));
    }
    next.block_hashes
        .insert(block.header().number, block.hash());
    Ok((block.clone_sealed_header(), next))
}

/// Decodes a block, rejecting trailing bytes.
fn decode_block(rlp: &[u8]) -> Result<SealedBlock<Block>, String> {
    alloy_rlp::decode_exact::<Block>(rlp)
        .map(SealedBlock::seal_slow)
        .map_err(|e| format!("RLP: {e}"))
}

/// Prints the results, writes every failure to `report`, and fails the test if any case failed.
fn summarize(results: &[FileResults], evm: Evm, report: &Path, started: Instant) {
    let mut passed = 0;
    let mut failures = Vec::new();
    let mut not_run: BTreeMap<&str, usize> = BTreeMap::new();
    // Per fixture directory (e.g. `osaka/eip7939_count_leading_zeros`): (passed, failed).
    let mut by_dir: BTreeMap<String, (usize, usize)> = BTreeMap::new();

    for file in results {
        for (network, n) in &file.not_run {
            *not_run.entry(network.as_str()).or_default() += n;
        }
        for case in &file.cases {
            let dir = Path::new(&case.file)
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            let entry = by_dir.entry(dir).or_default();
            match &case.outcome {
                Outcome::Pass => {
                    passed += 1;
                    entry.0 += 1;
                }
                Outcome::Fail(msg) => {
                    entry.1 += 1;
                    failures.push((case, "FAIL", msg));
                }
                Outcome::Panic(msg) => {
                    entry.1 += 1;
                    failures.push((case, "PANIC", msg));
                }
            }
        }
    }

    failures.sort_by(|a, b| (&a.0.file, &a.0.name).cmp(&(&b.0.file, &b.0.name)));

    // Known failures don't fail the run, but one that passes now must be taken off the list.
    let known: Vec<&str> = KNOWN_FAILURES
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    let id = |case: &CaseResult| format!("{}::{}", case.file, case.name);
    let known_failing = failures
        .iter()
        .filter(|(case, _, _)| known.contains(&id(case).as_str()))
        .count();
    let now_passing: Vec<String> = results
        .iter()
        .flat_map(|f| &f.cases)
        .filter(|c| matches!(c.outcome, Outcome::Pass) && known.contains(&id(c).as_str()))
        .map(id)
        .collect();
    let unexpected = failures.len() - known_failing;
    let mut text = String::new();
    for (case, kind, msg) in &failures {
        let kind = if known.contains(&id(case).as_str()) {
            "KNOWN"
        } else {
            kind
        };
        let msg = msg.replace('\n', " | ");
        writeln!(text, "{kind} {}::{}: {msg}", case.file, case.name).unwrap();
    }
    if let Some(dir) = report.parent() {
        fs::create_dir_all(dir).ok();
    }
    fs::write(report, &text).unwrap_or_else(|e| panic!("{}: {e}", report.display()));

    let failing_dirs: Vec<_> = by_dir.iter().filter(|(_, (_, f))| *f > 0).collect();
    if !failing_dirs.is_empty() {
        println!("\nfailing directories (passed / failed):");
        let mut dirs = failing_dirs;
        dirs.sort_by_key(|(_, (_, f))| std::cmp::Reverse(*f));
        for (dir, (p, f)) in dirs {
            println!("  {f:>6} / {:<6} {dir}", p + f);
        }
    }
    let panics = failures.iter().filter(|(_, k, _)| *k == "PANIC").count();
    if unexpected > 0 {
        println!(
            "\nfirst unexpected failures (all failures in {}):",
            report.display()
        );
        for line in text.lines().filter(|l| !l.starts_with("KNOWN")).take(20) {
            println!("  {}", truncate(line, 400));
        }
    }
    println!("\nnot run (network): {not_run:?}");
    println!(
        "\n=== EEST blockchain tests, {} on {} ===",
        evm.name(),
        NETWORKS.join(",")
    );
    println!("passed: {passed}");
    println!(
        "failed: {} ({panics} panics, {known_failing} known: tests/eest-known-failures.list)",
        failures.len()
    );
    println!(" TOTAL: {}", passed + failures.len());
    println!("  time: {:.0?}", started.elapsed());

    assert!(
        now_passing.is_empty(),
        "known failures now pass, remove them from tests/eest-known-failures.list: {now_passing:#?}"
    );
    assert!(
        unexpected == 0,
        "{unexpected} EEST cases failed unexpectedly, see {}",
        report.display()
    );
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}
