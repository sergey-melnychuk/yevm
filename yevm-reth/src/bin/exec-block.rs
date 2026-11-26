//! Executes one block from a reth datadir with evm2 and with yevm on the same pre-block state, and
//! reports time and state reads per transaction for both.
//!
//! The datadir is opened read-only, so this runs next to the live node. The state before the
//! block comes from reth's history (changeset reverts back from the persisted tip), so the block
//! must be within the node's retained history. Each engine runs twice: the first pass reads
//! mostly cold pages, the second pass reads pages the first one loaded.
//!
//! Given a range `<block>..<last>` (inclusive), it executes every block in it the same way,
//! prints one line per block and sums the warm runs: total time, time in state reads, the rest,
//! and gas per second for each engine, and whether the receipts agree. A range also runs evm2
//! without reth's shared precompile result cache ("evm2-nc"), with which evm2's warm run would
//! otherwise reuse the precompile outputs of its first run. `EXEC_BLOCK_PASSES=<n>` runs each
//! engine n times per block instead of twice and keeps the last run (for profiling warm runs).
//!
//! Usage:
//!   cargo run --release -p yevm-reth --bin exec-block -- <datadir> <block>[..<last>] [top]

// The node's allocator (jemalloc with the default features), so both engines allocate as they
// do in the node.
#[global_allocator]
static ALLOC: reth_ethereum::cli::allocator::Allocator =
    reth_ethereum::cli::allocator::new_allocator();

use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};

use alloy_primitives::{Address, B256};
use evm2::{
    bytecode::Bytecode,
    evm::{AccountInfo, Database},
    interpreter::Word,
};
use reth_chainspec::MAINNET;
use reth_ethereum::provider::{
    BlockReader, StateProvider, StateProviderBox, StateProviderFactory, TransactionVariant,
    providers::BlockchainProvider,
};
use reth_ethereum_primitives::{Block, EthPrimitives, Receipt};
use reth_evm::{
    ConfigureEvm, Evm, EvmEnv, database::StateProviderDatabase, execute::BlockExecutor,
};
use reth_evm_ethereum::{EthEvmConfig, EvmFactory, RethEvmFactory};
use reth_primitives_traits::RecoveredBlock;
use yevm_reth::{Factory, RethDb, exec::YevmEvmConfig};

/// State reads and the time spent in them.
#[derive(Clone, Copy, Default)]
struct Reads {
    accounts: u64,
    storage: u64,
    code: u64,
    hashes: u64,
    time: Duration,
}

impl Reads {
    fn since(self, start: Self) -> Self {
        Self {
            accounts: self.accounts - start.accounts,
            storage: self.storage - start.storage,
            code: self.code - start.code,
            hashes: self.hashes - start.hashes,
            time: self.time - start.time,
        }
    }

    fn total(self) -> u64 {
        self.accounts + self.storage + self.code + self.hashes
    }
}

/// A database that counts and times the reads that reach it.
struct Counted<D> {
    db: D,
    reads: Rc<Cell<Reads>>,
}

impl<D: Database> Counted<D> {
    fn count<T>(&mut self, kind: fn(&mut Reads), read: impl FnOnce(&mut D) -> T) -> T {
        let start = Instant::now();
        let value = read(&mut self.db);
        let mut reads = self.reads.get();
        kind(&mut reads);
        reads.time += start.elapsed();
        self.reads.set(reads);
        value
    }
}

impl<D: Database> Database for Counted<D> {
    type Error = D::Error;

    fn is_fatal(error: &Self::Error) -> bool {
        D::is_fatal(error)
    }

    fn get_account(&mut self, address: &Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.count(|reads| reads.accounts += 1, |db| db.get_account(address))
    }

    fn get_code_by_hash(&mut self, code_hash: &B256) -> Result<Bytecode, Self::Error> {
        self.count(|reads| reads.code += 1, |db| db.get_code_by_hash(code_hash))
    }

    fn get_storage(&mut self, address: &Address, key: &Word) -> Result<Word, Self::Error> {
        self.count(
            |reads| reads.storage += 1,
            |db| db.get_storage(address, key),
        )
    }

    fn get_block_hash(&mut self, number: &Word) -> Result<B256, Self::Error> {
        self.count(|reads| reads.hashes += 1, |db| db.get_block_hash(number))
    }
}

/// One transaction's execution time and state reads.
#[derive(Clone, Copy)]
struct TxRun {
    time: Duration,
    reads: Reads,
}

/// One engine's execution of the block.
struct Run {
    time: Duration,
    reads: Reads,
    txs: Vec<TxRun>,
    receipts: Vec<Receipt>,
}

fn state_before(factory: &Factory, block: u64) -> eyre::Result<StateProviderBox> {
    Ok(BlockchainProvider::new(factory.clone())?.history_by_block_number(block - 1)?)
}

fn run(
    evm_config: &impl ConfigureEvm<Primitives = EthPrimitives>,
    state: StateProviderBox,
    block: &RecoveredBlock<Block>,
) -> eyre::Result<Run> {
    let reads = Rc::new(Cell::new(Reads::default()));
    let mut db = Counted {
        db: StateProviderDatabase::new(state.into_evm_state_provider()),
        reads: reads.clone(),
    };
    let start = Instant::now();
    let mut executor = evm_config.executor_for_block(&mut db, block.sealed_block())?;
    // The node always streams state updates to its state root task.
    executor.set_state_hook(|_| {});
    executor.apply_pre_execution_changes()?;
    let mut txs = Vec::with_capacity(block.body().transactions.len());
    for transaction in block.transactions_recovered() {
        let (before, tx_start) = (reads.get(), Instant::now());
        executor.execute_transaction(transaction)?;
        txs.push(TxRun {
            time: tx_start.elapsed(),
            reads: reads.get().since(before),
        });
    }
    let output = executor.finish()?;
    Ok(Run {
        time: start.elapsed(),
        reads: reads.get(),
        txs,
        receipts: output.result.receipts,
    })
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// One engine's time over a range of blocks, summed over each block's warm run.
#[derive(Default)]
struct Totals {
    time: Duration,
    db: Duration,
}

impl Totals {
    fn add(&mut self, run: &Run) {
        self.time += run.time;
        self.db += run.reads.time;
    }

    /// The time outside state reads.
    fn exec(&self) -> Duration {
        self.time.saturating_sub(self.db)
    }
}

/// The value at `q` (0 to 1) of sorted `values`.
fn quantile(values: &[f64], q: f64) -> f64 {
    values[((values.len() - 1) as f64 * q).round() as usize]
}

/// reth's EVM factory with its shared precompile result cache off (custom factories default to
/// uncached), to time evm2's own precompiles: reth's default factory caches every pure
/// precompile's output by input, so evm2's second run of a block reuses the first one's.
#[derive(Clone, Debug, Default)]
struct Uncached(RethEvmFactory);

impl EvmFactory for Uncached {
    type Types = evm2::BaseEvmTypes;
    type SpecId = evm2::SpecId;

    fn spec_id(&self, spec: evm2::SpecId) -> evm2::SpecId {
        self.0.spec_id(spec)
    }

    fn tx_registry(
        &self,
        spec: evm2::SpecId,
    ) -> evm2::registry::TxRegistry<Self::Types, evm2::TxResult<Self::Types>> {
        self.0.tx_registry(spec)
    }

    fn configure_evm(&self, evm: &mut evm2::Evm<'_, Self::Types>) {
        self.0.configure_evm(evm)
    }
}

/// The engines a range runs: evm2 as reth configures it (with the shared precompile cache),
/// evm2 without that cache, and yevm (which has none).
const ENGINES: [&str; 3] = ["evm2", "evm2-nc", "yevm"];

/// Executes blocks `from..=to`, each with every engine twice (or `EXEC_BLOCK_PASSES` times) on
/// the same pre-block state, and sums the last (warm) runs. Which engine goes first rotates from block to block, so none
/// gets the pages another loaded more often.
fn range(factory: &Factory, from: u64, to: u64) -> eyre::Result<()> {
    eyre::ensure!(from <= to, "empty range {from}..{to}");
    let evm2 = EthEvmConfig::new(MAINNET.clone());
    let evm2_uncached = EthEvmConfig::new_with_evm_factory(MAINNET.clone(), Uncached::default());
    let yevm = YevmEvmConfig::new(MAINNET.clone());
    let passes = std::env::var("EXEC_BLOCK_PASSES")
        .ok()
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or(2)
        .max(1);
    let mut totals: [Totals; 3] = Default::default();
    let (mut txs, mut gas) = (0usize, 0u64);
    // yevm's time outside reads over evm2's and over uncached evm2's, per block.
    let (mut ratios, mut ratios_nc, mut differ) = (Vec::new(), Vec::new(), Vec::new());

    println!(
        "{:>9} {:>5} {:>7} | {:>9} {:>9} {:>9} {:>9} | {:>6} {:>6}",
        "block", "txs", "Mgas", "evm2 ms", "nc ms", "yevm ms", "db ms", "y/e", "y/nc"
    );
    for number in from..=to {
        let block = factory
            .provider()?
            .recovered_block(number.into(), TransactionVariant::WithHash)?
            .ok_or_else(|| eyre::eyre!("block {number} is not in the datadir"))?;
        let once = |engine: usize| -> eyre::Result<Run> {
            let state = state_before(factory, number)?;
            match engine {
                0 => run(&evm2, state, &block),
                1 => run(&evm2_uncached, state, &block),
                _ => run(&yevm, state, &block),
            }
        };
        let mut warm: [Option<Run>; 3] = Default::default();
        for _pass in 0..passes {
            for i in 0..3 {
                let engine = (i + number as usize) % 3;
                warm[engine] = Some(once(engine)?);
            }
        }
        let [Some(evm2_run), Some(nc_run), Some(yevm_run)] = warm else {
            unreachable!("every engine ran")
        };

        let exec = |run: &Run| ms(run.time.saturating_sub(run.reads.time)).max(1e-3);
        let (ratio, ratio_nc) = (
            exec(&yevm_run) / exec(&evm2_run),
            exec(&yevm_run) / exec(&nc_run),
        );
        println!(
            "{number:>9} {:>5} {:>7.2} | {:>9.2} {:>9.2} {:>9.2} {:>9.2} | {ratio:>6.2} {ratio_nc:>6.2}",
            block.body().transactions.len(),
            block.header().gas_used as f64 / 1e6,
            ms(evm2_run.time),
            ms(nc_run.time),
            ms(yevm_run.time),
            ms(yevm_run.reads.time),
        );
        if nc_run.receipts != evm2_run.receipts || yevm_run.receipts != evm2_run.receipts {
            differ.push(number);
        }
        for (totals, run) in totals.iter_mut().zip([&evm2_run, &nc_run, &yevm_run]) {
            totals.add(run);
        }
        ratios.push(ratio);
        ratios_nc.push(ratio_nc);
        txs += block.body().transactions.len();
        gas += block.header().gas_used;
    }

    let mgas = gas as f64 / 1e6;
    println!(
        "\n{} blocks {from}..={to}: {txs} txs, {mgas:.1} Mgas (warm runs)",
        to - from + 1
    );
    println!(
        "{:<8} {:>10} {:>10} {:>10} {:>9} {:>12}",
        "", "total ms", "db ms", "exec ms", "Mgas/s", "exec Mgas/s"
    );
    for (name, totals) in ENGINES.iter().zip(&totals) {
        println!(
            "{name:<8} {:>10.1} {:>10.1} {:>10.1} {:>9.1} {:>12.1}",
            ms(totals.time),
            ms(totals.db),
            ms(totals.exec()),
            mgas / totals.time.as_secs_f64(),
            mgas / totals.exec().as_secs_f64(),
        );
    }
    for (name, ratios) in [("evm2", &mut ratios), ("evm2-nc", &mut ratios_nc)] {
        ratios.sort_by(f64::total_cmp);
        println!(
            "yevm/{name} time outside reads, per block: median {:.2}, p10 {:.2}, p90 {:.2}",
            quantile(ratios, 0.5),
            quantile(ratios, 0.1),
            quantile(ratios, 0.9),
        );
    }
    if differ.is_empty() {
        println!("receipts match in every block");
    } else {
        println!("receipts differ in {} blocks: {differ:?}", differ.len());
    }
    Ok(())
}

fn main() -> eyre::Result<()> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: exec-block <datadir> <block>[..<last>] [top]";
    let datadir = args.next().ok_or_else(|| eyre::eyre!(usage))?;
    let blocks = args.next().ok_or_else(|| eyre::eyre!(usage))?;
    if let Some((from, to)) = blocks.split_once("..") {
        let db = RethDb::latest(datadir)?;
        return range(db.factory(), from.parse()?, to.parse()?);
    }
    let number: u64 = blocks.parse()?;
    let top: usize = args
        .next()
        .map(|top| top.parse())
        .transpose()?
        .unwrap_or(10);

    let db = RethDb::latest(datadir)?;
    let factory = db.factory();
    let block = factory
        .provider()?
        .recovered_block(number.into(), TransactionVariant::WithHash)?
        .ok_or_else(|| eyre::eyre!("block {number} is not in the datadir"))?;
    let evm2 = EthEvmConfig::new(MAINNET.clone());
    let yevm = YevmEvmConfig::new(MAINNET.clone());

    println!(
        "block {number}: {} txs, {:.2} Mgas",
        block.body().transactions.len(),
        block.header().gas_used as f64 / 1e6
    );
    println!(
        "{:<8} {:>10} {:>10} {:>9} {:>9} {:>7} {:>7}",
        "run", "total ms", "db ms", "accounts", "storage", "code", "hashes"
    );
    let mut runs = Vec::new();
    for pass in 1..=2 {
        for (name, result) in [
            ("evm2", run(&evm2, state_before(factory, number)?, &block)),
            ("yevm", run(&yevm, state_before(factory, number)?, &block)),
        ] {
            let result = result?;
            let reads = result.reads;
            println!(
                "{:<8} {:>10.1} {:>10.1} {:>9} {:>9} {:>7} {:>7}",
                format!("{name} #{pass}"),
                ms(result.time),
                ms(reads.time),
                reads.accounts,
                reads.storage,
                reads.code,
                reads.hashes
            );
            runs.push(result);
        }
    }

    let (evm2_run, yevm_run) = (&runs[2], &runs[3]);
    println!(
        "receipts match: {}",
        runs.iter().all(|run| run.receipts == evm2_run.receipts)
    );

    let gas = |index: usize| {
        let cumulative = |index: usize| evm2_run.receipts[index].cumulative_gas_used;
        cumulative(index) - index.checked_sub(1).map_or(0, cumulative)
    };
    let mut order: Vec<usize> = (0..yevm_run.txs.len()).collect();
    order.sort_by_key(|&index| core::cmp::Reverse(yevm_run.txs[index].time));
    println!("\nslowest {top} transactions on yevm (pass 2):");
    println!(
        "{:>4} {:<12} {:>9} | {:>9} {:>9} {:>6} | {:>9} {:>9} {:>6} {:>6} {:>5}",
        "idx",
        "hash",
        "gas",
        "evm2 ms",
        "db ms",
        "reads",
        "yevm ms",
        "db ms",
        "acc",
        "slots",
        "code"
    );
    for &index in order.iter().take(top) {
        let (evm2_tx, yevm_tx) = (evm2_run.txs[index], yevm_run.txs[index]);
        let hash = block.body().transactions[index].tx_hash();
        println!(
            "{index:>4} {:<12} {:>9} | {:>9.2} {:>9.2} {:>6} | {:>9.2} {:>9.2} {:>6} {:>6} {:>5}",
            &hash.to_string()[..12],
            gas(index),
            ms(evm2_tx.time),
            ms(evm2_tx.reads.time),
            evm2_tx.reads.total(),
            ms(yevm_tx.time),
            ms(yevm_tx.reads.time),
            yevm_tx.reads.accounts,
            yevm_tx.reads.storage,
            yevm_tx.reads.code
        );
    }

    // Engine prewarming executes each transaction alone on the pre-block state, with nonce and
    // balance checks disabled, to load its state into the shared cache.
    let evm2_prewarm = prewarm!(evm2, factory, &block, number);
    let yevm_prewarm = prewarm!(yevm, factory, &block, number);
    let ran = |error: &Option<String>| match error {
        None => true,
        Some(error) => error.contains("executed-transaction views"),
    };
    let (evm2_ran, yevm_ran) = (
        evm2_prewarm.iter().filter(|(error, _)| ran(error)).count(),
        yevm_prewarm.iter().filter(|(error, _)| ran(error)).count(),
    );
    println!(
        "\nprewarm: evm2 executed {evm2_ran}/{n}, yevm executed {yevm_ran}/{n}",
        n = block.body().transactions.len()
    );
    println!("transactions whose prewarm differs:");
    for (index, ((evm2_error, evm2_reads), (yevm_error, yevm_reads))) in
        evm2_prewarm.iter().zip(&yevm_prewarm).enumerate()
    {
        if ran(evm2_error) != ran(yevm_error) || evm2_reads.total() != yevm_reads.total() {
            println!(
                "{index:>4} {:<12} evm2: {} reads, {} | yevm: {} reads, {}",
                &block.body().transactions[index].tx_hash().to_string()[..12],
                evm2_reads.total(),
                evm2_error.as_deref().unwrap_or("ok"),
                yevm_reads.total(),
                yevm_error.as_deref().unwrap_or("ok"),
            );
        }
    }
    Ok(())
}

/// Executes each transaction of `$block` alone on the state before it, like engine prewarming,
/// and returns each one's error, if any, and state reads.
macro_rules! prewarm {
    ($evm_config:expr, $factory:expr, $block:expr, $number:expr) => {{
        let env = $evm_config
            .evm_env($block.header())?
            .with_nonce_check_disabled()
            .with_balance_check_disabled();
        let mut results = Vec::new();
        for transaction in $block.transactions_recovered() {
            let reads = Rc::new(Cell::new(Reads::default()));
            let mut db = Counted {
                db: StateProviderDatabase::new(
                    state_before($factory, $number)?.into_evm_state_provider(),
                ),
                reads: reads.clone(),
            };
            let mut evm = $evm_config.evm_with_env(&mut db, env.clone());
            let error = evm
                .transact_and_discard(&transaction.cloned().convert(), |_| ())
                .err()
                .map(|error| error.to_string());
            drop(evm);
            results.push((error, reads.get()));
        }
        results
    }};
}
use prewarm;
