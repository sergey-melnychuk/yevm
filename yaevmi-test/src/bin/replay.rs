use std::{
    fs::File,
    io::{BufReader, BufWriter, Read, Write},
    path::Path,
    time::Instant,
};

use alloy_provider::ProviderBuilder;
use eyre::OptionExt;
use futures::{StreamExt, channel::mpsc};
use yaevmi_base::{Acc, Int, int, math::lift};
use yaevmi_core::{
    cache::Cache,
    call::Receipt,
    chain::{Chain, Fetched},
    exe::{CallResult, Executor, pre_block},
    rpc::Rpc,
    state::{Account, State},
};
use yaevmi_misc::hex::parse_vec;

const YAEVMI_RPC_URL: &str = "YAEVMI_RPC_URL";

// ./target/release/replay - replay the latest block
// ./target/release/replay <block> - replay the block number
// ./target/release/replay <block>:<index> | <hash> - replay specific transaction

// ## replaying number of consecutive blocks (inclusive interval, replays 11 blocks):
// rm -rf tmp/ && mkdir tmp && cp ./target/release/replay ./tmp
// for i in {0..10}; do x=$(($i + 24935457)); ./tmp/replay $x; done > 10.log &
// ## cat 10.log | grep FAIL | cut -d '=' -f 2 | cut -d ' ' -f 1 >> todo.log
// for i in {0..100}; do x=$(($i + 24935681)); ./tmp/replay $x; done > 100.log &
// for i in {0..200}; do x=$(($i + 24938068)); ./tmp/replay $x; done > 200.log &
// for i in {0..300}; do x=$(($i + 24978072)); ./tmp/replay $x; done > 300.log &
// for i in {0..400}; do x=$(($i + 24994424)); ./tmp/replay $x; done > 400.log &
// for i in {0..999}; do x=$(($i + 24984743)); ./tmp/replay $x; done > 999.log &
// for x in $(cat todo.log); do ./target/release/replay $x; done > todo.run.log

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        println!("{e}");
        std::process::exit(1);
    }
}

async fn run() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    let Ok(url) = std::env::var(YAEVMI_RPC_URL) else {
        eyre::bail!("{YAEVMI_RPC_URL} not set");
    };
    let mut rpc = Rpc::latest(url.clone()).await?;
    let chain_id = rpc.chain_id().await?;

    let (block, index) = {
        let arg = std::env::args()
            .nth(1)
            .unwrap_or_else(|| String::from("latest"));

        if arg.starts_with("0x") {
            if parse_vec(&arg).is_err() {
                eyre::bail!("Invalid hex literal: {arg}");
            }
            let hash = int(&arg);
            let receipt = rpc.receipt(hash).await?;
            let block = receipt.block_number.as_u64();
            let index = receipt.transaction_index.as_u64();
            (block, Some(index as usize))
        } else if arg.contains(":") {
            let mut split = arg.split(":");
            let block = split.next().ok_or_eyre("invalid block:index format")?;
            let block: u64 = if block == "latest" {
                rpc.block_number
            } else {
                block.parse()?
            };
            let index: usize = split
                .next()
                .ok_or_eyre("invalid block:index format")?
                .parse()?;
            (block, Some(index))
        } else {
            let block: u64 = if arg == "latest" {
                rpc.block_number
            } else {
                arg.parse()?
            };
            (block, None)
        }
    };

    let (ytx, mut yrx) = mpsc::channel(4 * 1024 * 1024);
    let mut cache = Cache::with_sender(ytx);

    // TODO: make single-tx also replayable? just save all fetches to block:index.js
    std::fs::create_dir_all("fetch")?;
    let path = format!("fetch/{}.json", block);
    let fetches = Path::new(&path);
    let block = if fetches.exists() && index.is_none() {
        let file = File::open(fetches)?;
        let mut reader = BufReader::new(file);
        let mut content = String::new();
        reader.read_to_string(&mut content)?;
        let fetched: Vec<Fetched> = serde_json::from_str(&content)?;
        let Some(Fetched::ChainId(chain_id)) = fetched.first().cloned() else {
            eyre::bail!("Cannot find fetched chain id");
        };
        cache.set_chain_id(chain_id);
        let Some(Fetched::Block(block)) = fetched.get(1).cloned() else {
            eyre::bail!("Cannot find stored block");
        };
        cache.prefetched(fetched);
        block
    } else {
        let chain_id = rpc.chain_id().await?;
        cache.set_chain_id(chain_id);
        cache.save_fetched(Fetched::ChainId(chain_id));

        let block = rpc.block(block).await?;
        cache.save_fetched(Fetched::Block(block.clone()));
        block
    };

    let head = block.head.clone();
    println!("Begin: {} / {}", head.number.as_u64(), head.hash);
    rpc.reset(head.number.as_u64() - 1, head.parent_hash);

    let (rtx, mut rrx) = tokio::sync::mpsc::channel(4096);
    let handle = tokio::spawn(async move {
        let is_trace = std::env::var("TRACE").is_ok();
        if is_trace {
            println!("---\nSTREAMING OPENED");
        }
        let mut skip = 0;
        loop {
            let y = yrx.next().await;
            let r = rrx.recv().await;
            if let (Some(mut y), Some(mut r)) = (y, r) {
                if y != r {
                    println!("===\nSTEP MISMATCH:\nYEVM: {y:#?}\nREVM: {r:#?}\n(skip: {skip})");
                    break;
                }
                if is_trace {
                    for line in r.debug.drain(..) {
                        y.debug.push(format!("REVM: {line}"));
                    }
                    println!("{y:#?}");
                }
                skip += 1;
            } else {
                break;
            }
        }
        if is_trace {
            println!("STREAMING CLOSED [{skip} items]\n---");
        }
    });

    let txs = block.txs.clone();
    let pack = (txs.clone(), head.clone(), index, chain_id);

    let (revm_result_tx, mut revm_result_rx) = tokio::sync::mpsc::channel::<RevmResult>(1);

    let provider = ProviderBuilder::new().connect(&url).await?;
    tokio::task::spawn_blocking(move || {
        let (txs, head, index, network_chain_id) = pack;
        if let Some(i) = index {
            let tx = &txs[i];
            let (call, tx) = (tx.call.clone().into(), tx.tx.clone());
            if let Err(e) = live::run_one(
                call,
                tx,
                head,
                network_chain_id,
                rtx,
                revm_result_tx,
                provider,
            ) {
                eprintln!("REVM replay error (no trace steps): {e:#}");
            }
        } else if let Err(e) =
            live::run_all(network_chain_id, &txs, head, rtx, revm_result_tx, provider)
        {
            eprintln!("REVM replay error (no trace steps): {e:#}");
        }
    });

    let txs = if let Some(i) = index {
        vec![txs[i].clone()]
    } else {
        txs
    };

    pre_block(&head, &mut cache, &rpc).await?;

    let n = txs.len();
    let mut ok = 0;
    let mut gas_total = 0;
    let mut ms_total = 0;
    let mut revm_drift: Vec<(Acc, Int)> = Vec::new();
    for (i, tx) in txs.into_iter().enumerate() {
        if std::env::var("TRACE").is_ok() {
            println!("{}", serde_json::to_string_pretty(&tx).unwrap());
        }

        let hash = tx.tx.hash;
        let sender = tx.call.from;
        let (tx, call) = (tx.tx.clone(), tx.call.into());
        let mut exe = Executor::new(call);
        cache.reset();

        let now = Instant::now();
        let result = exe.run(tx, head.clone(), &mut cache, &rpc).await?;
        let ms = now.elapsed().as_millis();

        let gas = result.gas().finalized;
        let fetches = exe.fetches;
        let fetching = exe.fetching.as_millis();
        let receipt = rpc.receipt(hash).await?;
        let ty = receipt.r#type.as_u8();

        // TODO: make revm checks optional? (e.g. --revm flag)
        let Some(RevmResult {
            call: revm_call,
            state: revm_state,
        }) = revm_result_rx.recv().await
        else {
            eyre::bail!("revm result unavailable");
        };
        let (mut violations, revm_gas_ok) = check_result(result, receipt, Some(revm_call));
        let skip_value = if revm_gas_ok { vec![] } else { vec![sender, head.coinbase] };
        let new_drift = check_state(revm_state, &mut cache, &mut violations, &skip_value, &revm_drift);
        revm_drift.extend(new_drift);

        let stats = if fetching > 0 {
            format!("{ms}ms/{}ms, fetches:{fetches}/{fetching}ms", ms - fetching)
        } else {
            format!("{ms}ms")
        };
        if violations.is_empty() {
            gas_total += gas;
            ms_total += ms - fetching;
            println!("{hash} [type:{ty}]: OK [{}/{n}, {gas} gas, {stats}]", i + 1);
            ok += 1;
        } else {
            println!(
                "{hash} [type:{ty}]: FAIL={}:{} [{}/{n}, {stats}]\n{}",
                head.number.as_u64(),
                index.unwrap_or(i),
                i + 1,
                violations.join("\n")
            );
        }
    }

    if !fetches.exists() && index.is_none() {
        let fetched = std::mem::take(&mut cache.fetched);
        let file = File::create(fetches)?;
        let mut writer = BufWriter::new(file);
        let content = serde_json::to_vec(&fetched)?;
        writer.write_all(&content)?;
    }

    let ok = if n > 1 {
        format!("Block: {}, {ok}/{n} OK", head.number.as_u64())
    } else {
        String::new()
    };
    let stat = if gas_total > 0 && ms_total > 0 {
        format!(
            "{gas_total} gas, {ms_total}ms: ~{:.2} gas/sec",
            gas_total as f64 * 1000.0 / ms_total as f64
        )
    } else {
        String::new()
    };
    if !ok.is_empty() {
        println!("{ok}, {stat}");
    }

    let _ = cache.sender.take();
    handle.await?;
    Ok(())
}

fn fmt_int(v: Int) -> String {
    let bytes = v.as_ref();
    let start = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len() - 1);
    bytes[start..].iter().fold("0x".to_string(), |mut s, b| {
        s.push_str(&format!("{b:02x}"));
        s
    })
}

fn check_result(result: CallResult, receipt: Receipt, revm: Option<CallResult>) -> (Vec<String>, bool) {
    let mut violations = Vec::new();
    let mut revm_gas_ok = true;
    let used_gas = receipt.gas_used.as_u64() as i64;
    match result {
        CallResult::Done { status, ret, gas } => {
            if status != receipt.status {
                violations.push(format!(
                    " ok: have {} want {}",
                    status.as_u8(),
                    receipt.status.as_u8()
                ));
            }
            if gas.finalized != used_gas {
                let diff = gas.finalized - used_gas;
                violations.push(format!("gas: have {} want {used_gas} [{diff:+}]", gas.finalized));
            }

            if let Some(revm) = revm {
                let CallResult::Done {
                    status: revm_status,
                    ret: revm_ret,
                    gas: revm_gas,
                } = revm
                else {
                    violations.push(format!(
                        "revm: call result mismatch\n  have {ret:#?}\n revm {revm:#?}"
                    ));
                    return (violations, revm_gas_ok);
                };
                if revm_status == receipt.status && status != revm_status {
                    violations.push(format!(
                        "revm: status mismatch: have {status} want {revm_status}"
                    ));
                }
                if revm_gas.finalized != used_gas {
                    // let diff = revm_gas.finalized - used_gas;
                    // violations.push(format!(
                    //     "revm: gas != receipt: revm={} receipt={used_gas} [{diff:+}]",
                    //     revm_gas.finalized
                    // ));
                    revm_gas_ok = false;
                } else if gas.finalized != revm_gas.finalized {
                    let diff = gas.finalized - revm_gas.finalized;
                    violations.push(format!(
                        "revm: gas mismatch: have {} want {} [{diff:+}]",
                        gas.finalized, revm_gas.finalized
                    ));
                }
                if ret != revm_ret {
                    violations.push(format!(
                        "revm: ret mismatch: have {} bytes want {} bytes",
                        ret.len(),
                        revm_ret.len()
                    ));
                }
            }
        }
        CallResult::Created { acc, ref code, gas } => {
            if Some(acc) != receipt.contract_address {
                violations.push(format!(
                    "new: have {} want {}",
                    acc,
                    receipt.contract_address.unwrap_or_default()
                ));
            }
            if gas.finalized != used_gas {
                let diff = gas.finalized - used_gas;
                violations.push(format!("gas: have {} want {used_gas} [{diff:+}]", gas.finalized));
            }

            if let Some(revm) = revm {
                let CallResult::Created {
                    acc: revm_acc,
                    code: revm_code,
                    gas: revm_gas,
                } = revm
                else {
                    violations.push(format!(
                        "revm: call result mismatch\n  have {result:#?}\n revm {revm:#?}"
                    ));
                    return (violations, revm_gas_ok);
                };
                if acc != revm_acc {
                    violations.push(format!(
                        "revm: created mismatch: have {acc} want {revm_acc}"
                    ));
                }
                if code != &revm_code {
                    violations.push(format!(
                        "revm: code mismatch: have {} bytes want {} bytes",
                        code.len(),
                        revm_code.len()
                    ));
                }
                if gas.finalized != revm_gas.finalized {
                    violations.push(format!(
                        "revm: gas mismatch: have {} want {}",
                        gas.finalized, revm_gas.finalized
                    ));
                }
            }
        }
    }
    (violations, revm_gas_ok)
}

pub type Env = Vec<(Acc, Account, Vec<(Int, Int)>)>;

// Returns new drift entries for accounts that were in skip_value but had a value mismatch,
// so callers can accumulate the revm drift across transactions.
fn check_state(
    state: Env,
    cache: &mut Cache,
    violations: &mut Vec<String>,
    skip_value: &[Acc],
    drift: &[(Acc, Int)],
) -> Vec<(Acc, Int)> {
    let wadd = lift(|[a, b]| a.wrapping_add(b));
    let wsub = lift(|[a, b]| a.wrapping_sub(b));
    let mut new_drift = Vec::new();
    for (acc, account, storage) in state {
        let is_empty = account.value.is_zero()
            && account.nonce.is_zero()
            && account.code.0.is_empty()
            && (storage.is_empty() || storage.iter().all(|(_, v)| v.is_zero()));
        if is_empty {
            continue;
        }

        let actual = cache.account(&acc).cloned().unwrap_or_default();
        if actual.code.0 != account.code.0 {
            violations.push(format!(
                "REVM: account {acc} code mismatch\n  want {} bytes\n  have {} bytes",
                account.code.0.len(),
                actual.code.0.len()
            ));
        }

        let acc_drift = drift.iter().filter(|(a, _)| *a == acc).fold(Int::ZERO, |s, (_, d)| wadd([s, *d]));
        let revm_value = wadd([account.value, acc_drift]);
        if actual.value != revm_value {
            if skip_value.contains(&acc) {
                new_drift.push((acc, wsub([actual.value, revm_value])));
            } else {
                let (sign, diff) = if actual.value >= revm_value {
                    ('+', wsub([actual.value, revm_value]))
                } else {
                    ('-', wsub([revm_value, actual.value]))
                };
                violations.push(format!(
                    "REVM: account {acc} value mismatch\n  want {}\n  have {} [{sign}{}]",
                    revm_value, actual.value, fmt_int(diff)
                ));
            }
        }

        if actual.nonce != account.nonce {
            violations.push(format!(
                "REVM: account {acc} nonce mismatch\n  want {}\n  have {}",
                account.nonce, actual.nonce
            ));
        }
        for (key, val) in storage {
            let (act, _) = cache.get(&acc, &key).unwrap_or_default();
            if act != val {
                violations.push(format!(
                    "REVM: account {acc} storage [{key}] mismatch:\n  want {val}\n  have {act}"
                ));
            }
        }
    }
    new_drift
}

pub struct RevmResult {
    pub call: CallResult,
    pub state: Env,
}

// TODO: run embedded database for acc/state storage
// consider: sqlite, leveldb, rocksdb, sled, yakvdb?

// TODO: for each processed block: generate hermetic env
// (containing all read storage cells by all transactions)
// (store it alongsize with block updates to allow reverting)
// (this allows re-running blocks on-demand without RPC calls)

mod live {
    use alloy_eip7702::{Authorization, SignedAuthorization};
    use alloy_primitives::map::FbBuildHasher;
    use alloy_primitives::{Address as AlloyAddress, U256 as AlloyU256};
    use alloy_provider::Provider;
    use revm::bytecode::opcode::OpCode;
    use revm::context::result::{ExecutionResult, HaltReason, Output};
    use revm::context::transaction::{AccessList, AccessListItem};
    use revm::context::{ContextTr, TxEnv};
    use revm::context_interface::result::ExecResultAndState;
    use revm::database::{AlloyDB, BlockId, CacheDB, WrapDatabaseAsync};
    use revm::interpreter::interpreter_types::{Immediates, Jumps};
    use revm::interpreter::{CallInputs, CallOutcome, CreateInputs, CreateOutcome};
    use revm::interpreter::{Interpreter, interpreter::EthInterpreter};
    use revm::primitives::{Address, B256, Bytes, TxKind, U256};
    use revm::{Context, ExecuteCommitEvm, InspectEvm, Inspector, MainBuilder, MainContext};

    use tokio::sync::mpsc;
    use yaevmi_base::{Acc, Int};
    use yaevmi_core::call::TxFull;
    use yaevmi_core::evm::Gas;
    use yaevmi_core::state::Account;
    use yaevmi_core::trace::Step;
    use yaevmi_core::{Call, Head, Tx};
    use yaevmi_misc::buf::Buf;

    use crate::RevmResult;

    fn signed_authorizations(tx: &Tx) -> Vec<SignedAuthorization> {
        tx.authorization_list
            .iter()
            .map(|item| {
                SignedAuthorization::new_unchecked(
                    Authorization {
                        chain_id: AlloyU256::from_be_bytes(
                            <[u8; 32]>::try_from(item.chain_id.as_ref()).unwrap(),
                        ),
                        address: AlloyAddress::from_slice(item.address.as_ref()),
                        nonce: item.nonce.as_u64(),
                    },
                    item.y_parity.as_u8(),
                    AlloyU256::from_be_bytes(<[u8; 32]>::try_from(item.r.as_ref()).unwrap()),
                    AlloyU256::from_be_bytes(<[u8; 32]>::try_from(item.s.as_ref()).unwrap()),
                )
            })
            .collect()
    }

    #[derive(Debug, Default)]
    pub struct Tracer {
        step: Option<Step>,
        refund: i64,
        gas: u64,
        depth: usize,
        tx: Option<mpsc::Sender<Step>>,
    }

    impl<CTX: ContextTr> Inspector<CTX, EthInterpreter> for Tracer {
        fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _ctx: &mut CTX) {
            let pc = interp.bytecode.pc();
            let op = interp.bytecode.opcode();
            let name = OpCode::new(op)
                .map(|op| op.as_str())
                .unwrap_or("INVALID")
                .to_owned();
            let data = if (0x60..=0x7f).contains(&op) {
                let n = (op - 0x60 + 1) as usize;
                let raw = interp.bytecode.read_slice(n + 1);
                Some(Buf(raw[1..].to_vec()))
            } else {
                None
            };

            let gas = interp.gas.remaining();
            let stack = interp.stack.len();
            let memory = interp.memory.len();
            self.step = Some(Step {
                pc,
                op,
                name,
                data,
                gas,
                stack,
                memory,
                debug: vec![],
            });
            self.gas = gas;

            if op == 0x55
                && let (Ok(key), Ok(val)) = (interp.stack.peek(0), interp.stack.peek(1))
                && let Some(step) = self.step.as_mut()
            {
                step.debug.push(format!("SSTORE: key={key:0x}"));
                step.debug.push(format!("SSTORE: val={val:0x}"));
            }
        }

        fn step_end(&mut self, interp: &mut Interpreter<EthInterpreter>, _ctx: &mut CTX) {
            let gas = interp.gas.remaining();
            let cost = self.gas - gas;

            let refund = interp.gas.refunded() - self.refund;
            self.refund = interp.gas.refunded();

            if let Some(mut step) = self.step.take() {
                step.gas = gas;
                step.stack = interp.stack.len();
                step.memory = interp.memory.len();
                step.debug.push(format!("cost={cost}"));
                step.debug.push(format!("gas_refund={}", self.refund));
                if refund > 0 {
                    step.debug.push(format!("refund={refund}"));
                }
                step.debug.push(format!("depth={}", self.depth));

                if step.name == "SSTORE"
                    && let (Ok(key), Ok(val)) = (interp.stack.peek(0), interp.stack.peek(1))
                {
                    step.debug.push(format!("SSTORE: key={key:?}"));
                    step.debug.push(format!("SSTORE: val={val:?}"));
                } else if step.name == "CALLER" {
                    let caller = interp.stack.peek(0).unwrap_or_default();
                    step.debug.push(format!("CALLER: {caller:0x}"));
                } else if step.name == "BALANCE" {
                    let balance = interp.stack.peek(0).unwrap_or_default();
                    step.debug.push(format!("BALANCE: {balance:0x}"));
                }

                if let Some(tx) = self.tx.as_ref()
                    && tx.blocking_send(step).is_err()
                {
                    self.tx = None;
                }
            }
        }

        fn call(&mut self, _: &mut CTX, _: &mut CallInputs) -> Option<CallOutcome> {
            self.depth += 1;
            None
        }

        fn call_end(&mut self, _: &mut CTX, _: &CallInputs, _: &mut CallOutcome) {
            self.depth -= 1;
        }

        fn create(&mut self, _: &mut CTX, _: &mut CreateInputs) -> Option<CreateOutcome> {
            self.depth += 1;
            None
        }

        fn create_end(&mut self, _: &mut CTX, _: &CreateInputs, _: &mut CreateOutcome) {
            self.depth -= 1;
        }

        fn selfdestruct(&mut self, _: Address, _: Address, _: U256) {
            self.depth -= 1;
        }
    }

    pub fn run_all(
        chain_id: u64,
        txs: &[TxFull],
        head: Head,
        sender: mpsc::Sender<Step>,
        result_sender: mpsc::Sender<RevmResult>,
        provider: impl Provider + Clone,
    ) -> eyre::Result<()> {
        let to_addr = |a: &Acc| Address::from(<[u8; 20]>::try_from(a.as_ref()).unwrap());
        let to_u256 = |i: &Int| U256::from_be_bytes(<[u8; 32]>::try_from(i.as_ref()).unwrap());
        let to_b256 = |i: &Int| B256::from(<[u8; 32]>::try_from(i.as_ref()).unwrap());

        let db = AlloyDB::new(provider, BlockId::from(to_b256(&head.parent_hash)));
        let db = WrapDatabaseAsync::new(db).unwrap();
        let mut db = CacheDB::new(db);

        if let Some(root) = head.parent_beacon_block_root {
            let beacon_roots =
                alloy_primitives::address!("000f3df6d732807ef1319fb7b8bb8522d0beac02");
            let timestamp = to_u256(&head.timestamp).to::<u64>();
            let slot = U256::from(timestamp % 8191);
            db.insert_account_storage(beacon_roots, slot, U256::from(timestamp))
                .map_err(|e| eyre::eyre!("{e:?}"))?;
            db.insert_account_storage(
                beacon_roots,
                slot + U256::from(8191u64),
                U256::from_be_bytes(to_b256(&root).0),
            )
            .map_err(|e| eyre::eyre!("{e:?}"))?;
        }

        let mut ctx = Context::mainnet().with_db(db);
        ctx.block.number = U256::from(head.number.as_u64());
        ctx.block.timestamp = to_u256(&head.timestamp);
        ctx.block.gas_limit = head.gas_limit.as_u64();
        ctx.block.beneficiary = to_addr(&head.coinbase);
        ctx.block.basefee = head.base_fee.as_u64();
        ctx.block.prevrandao = Some(to_b256(&head.prevrandao));
        ctx.cfg.chain_id = chain_id;
        // TODO: proper blob handling
        // if let Some(excess) = head.excess_blob_gas {
        //     let fraction = if head.number.as_u64() >= 22_431_084 { 5_007_716u64 } else { 3_338_477u64 };
        //     ctx.block.set_blob_excess_gas_and_price(excess.as_u64(), fraction);
        // }

        // let fork = revm::primitives::hardfork::SpecId::OSAKA;
        // ctx.cfg.set_spec_and_mainnet_gas_params(fork);

        let inspector = Tracer {
            tx: Some(sender),
            ..Tracer::default()
        };
        let mut evm = ctx.build_mainnet_with_inspector(inspector);

        for tx in txs {
            let (tx, call): (Tx, Call) = (tx.tx.clone(), tx.call.clone().into());
            // For legacy tx (max_fee_per_gas=0), use gas_price for effective fee
            let max_fee = if tx.max_fee_per_gas.is_zero() {
                tx.gas_price.as_u128()
            } else {
                tx.max_fee_per_gas.as_u128()
            };
            let priority_fee = if tx.max_fee_per_gas.is_zero() {
                tx.gas_price.as_u128()
            } else {
                tx.max_priority_fee_per_gas.as_u128()
            };

            let kind = if let Some(to) = call.to {
                TxKind::Call(to_addr(&to))
            } else {
                TxKind::Create
            };
            let tx = TxEnv::builder()
                .caller(to_addr(&call.by))
                .kind(kind)
                .gas_limit(call.gas)
                .gas_price(tx.gas_price.as_u128())
                .value(to_u256(&call.eth))
                .data(Bytes::from(call.data.0.clone()))
                .nonce(tx.nonce.as_u64())
                .access_list(AccessList::from(
                    tx.access_list
                        .iter()
                        .map(|item| AccessListItem {
                            address: to_addr(&item.address),
                            storage_keys: item
                                .storage_keys
                                .iter()
                                .map(to_b256)
                                .collect::<Vec<B256>>(),
                        })
                        .collect::<Vec<AccessListItem>>(),
                ))
                .max_fee_per_gas(max_fee)
                .gas_priority_fee(Some(priority_fee))
                .authorization_list_signed(signed_authorizations(&tx))
                .blob_hashes(
                    tx.blob_versioned_hashes
                        .iter()
                        .map(to_b256)
                        .collect::<Vec<B256>>(),
                )
                .max_fee_per_blob_gas(tx.max_fee_per_blob_gas.unwrap_or_default().as_u128())
                .build()
                .map_err(|e| eyre::eyre!("{e:?}"))?;

            let ExecResultAndState { result, state } = evm.inspect_tx(tx)?;
            evm.commit(state.clone());

            let revm_result = to_revm_result(result, state);
            result_sender.blocking_send(revm_result)?;
        }
        let _ = evm.inspector.tx.take();
        Ok(())
    }

    pub fn run_one(
        call: Call,
        tx: Tx,
        head: Head,
        network_chain_id: u64,
        sender: mpsc::Sender<Step>,
        result_sender: mpsc::Sender<RevmResult>,
        provider: impl Provider + Clone,
    ) -> eyre::Result<()> {
        let to_addr = |a: &Acc| Address::from(<[u8; 20]>::try_from(a.as_ref()).unwrap());
        let to_u256 = |i: &Int| U256::from_be_bytes(<[u8; 32]>::try_from(i.as_ref()).unwrap());
        let to_b256 = |i: &Int| B256::from(<[u8; 32]>::try_from(i.as_ref()).unwrap());

        let db = AlloyDB::new(provider, BlockId::from(to_b256(&head.parent_hash)));
        let db = WrapDatabaseAsync::new(db).unwrap();
        let mut db = CacheDB::new(db);

        if let Some(root) = head.parent_beacon_block_root {
            let beacon_roots =
                alloy_primitives::address!("000f3df6d732807ef1319fb7b8bb8522d0beac02");
            let timestamp = to_u256(&head.timestamp).to::<u64>();
            let slot = U256::from(timestamp % 8191);
            db.insert_account_storage(beacon_roots, slot, U256::from(timestamp))
                .map_err(|e| eyre::eyre!("{e:?}"))?;
            db.insert_account_storage(
                beacon_roots,
                slot + U256::from(8191u64),
                U256::from_be_bytes(to_b256(&root).0),
            )
            .map_err(|e| eyre::eyre!("{e:?}"))?;
        }

        let mut ctx = Context::mainnet().with_db(db);
        ctx.block.number = U256::from(head.number.as_u64());
        ctx.block.timestamp = to_u256(&head.timestamp);
        ctx.block.gas_limit = head.gas_limit.as_u64();
        ctx.block.beneficiary = to_addr(&head.coinbase);
        ctx.block.basefee = head.base_fee.as_u64();
        ctx.block.prevrandao = Some(to_b256(&head.prevrandao));
        ctx.cfg.chain_id = if tx.chain_id.is_zero() {
            network_chain_id
        } else {
            tx.chain_id.as_u64()
        };

        // For legacy tx (max_fee_per_gas=0), use gas_price for effective fee
        let max_fee = if tx.max_fee_per_gas.is_zero() {
            tx.gas_price.as_u128()
        } else {
            tx.max_fee_per_gas.as_u128()
        };
        let priority_fee = if tx.max_fee_per_gas.is_zero() {
            tx.gas_price.as_u128()
        } else {
            tx.max_priority_fee_per_gas.as_u128()
        };

        let kind = if let Some(to) = call.to {
            TxKind::Call(to_addr(&to))
        } else {
            TxKind::Create
        };
        let tx_env = TxEnv::builder()
            .caller(to_addr(&call.by))
            .kind(kind)
            .gas_limit(call.gas)
            .gas_price(tx.gas_price.as_u128())
            .value(to_u256(&call.eth))
            .data(Bytes::from(call.data.0.clone()))
            .nonce(tx.nonce.as_u64())
            .access_list(AccessList::from(
                tx.access_list
                    .iter()
                    .map(|item| AccessListItem {
                        address: to_addr(&item.address),
                        storage_keys: item.storage_keys.iter().map(to_b256).collect::<Vec<B256>>(),
                    })
                    .collect::<Vec<AccessListItem>>(),
            ))
            .max_fee_per_gas(max_fee)
            .gas_priority_fee(Some(priority_fee))
            .authorization_list_signed(signed_authorizations(&tx))
            .blob_hashes(
                tx.blob_versioned_hashes
                    .iter()
                    .map(to_b256)
                    .collect::<Vec<B256>>(),
            )
            .max_fee_per_blob_gas(tx.max_fee_per_blob_gas.unwrap_or_default().as_u128())
            .build()
            .map_err(|e| eyre::eyre!("{e:?}"))?;

        let inspector = Tracer {
            tx: Some(sender),
            ..Tracer::default()
        };
        let mut evm = ctx.build_mainnet_with_inspector(inspector);
        let ExecResultAndState { result, state } = evm.inspect_tx(tx_env)?;
        let revm_result = to_revm_result(result, state);
        result_sender.blocking_send(revm_result)?;
        let _ = evm.inspector.tx.take();
        Ok(())
    }

    fn to_revm_result(
        result: ExecutionResult<HaltReason>,
        state: revm::primitives::HashMap<Address, revm::state::Account, FbBuildHasher<20>>,
    ) -> RevmResult {
        RevmResult {
            call: match result {
                ExecutionResult::Success {
                    reason: _,
                    gas,
                    logs: _,
                    output: Output::Call(ret),
                } => yaevmi_core::exe::CallResult::Done {
                    status: Int::ONE,
                    ret: ret.to_vec().into(),
                    gas: Gas {
                        limit: 0,
                        spent: 0,
                        refund: 0,
                        finalized: gas.used() as i64,
                    },
                },
                ExecutionResult::Success {
                    reason: _,
                    gas,
                    logs: _,
                    output: Output::Create(code, Some(address)),
                } => yaevmi_core::exe::CallResult::Created {
                    acc: Acc::from(address.as_slice()),
                    code: code.to_vec().into(),
                    gas: Gas {
                        limit: 0,
                        spent: 0,
                        refund: 0,
                        finalized: gas.used() as i64,
                    },
                },
                ExecutionResult::Success {
                    reason: _,
                    gas,
                    logs: _,
                    output: Output::Create(code, None),
                } => yaevmi_core::exe::CallResult::Created {
                    acc: Acc::ZERO,
                    code: code.to_vec().into(),
                    gas: Gas {
                        limit: 0,
                        spent: 0,
                        refund: 0,
                        finalized: gas.used() as i64,
                    },
                },
                ExecutionResult::Revert {
                    gas,
                    logs: _,
                    output: ret,
                } => yaevmi_core::exe::CallResult::Done {
                    status: Int::ZERO,
                    ret: ret.to_vec().into(),
                    gas: Gas {
                        limit: 0,
                        spent: 0,
                        refund: 0,
                        finalized: gas.used() as i64,
                    },
                },
                ExecutionResult::Halt {
                    reason: _,
                    gas,
                    logs: _,
                } => yaevmi_core::exe::CallResult::Done {
                    status: Int::ZERO,
                    ret: vec![].into(),
                    gas: Gas {
                        limit: 0,
                        spent: 0,
                        refund: 0,
                        finalized: gas.used() as i64,
                    },
                },
            },
            state: state
                .into_iter()
                .filter(|(_, account)| !account.is_selfdestructed())
                .map(|(address, account)| {
                    let storage = account
                        .storage
                        .into_iter()
                        .map(|(slot, value)| {
                            (
                                Int::from(slot.to_be_bytes::<32>().as_slice()),
                                Int::from(value.present_value.to_be_bytes::<32>().as_slice()),
                            )
                        })
                        .collect();
                    let bytecode = account.info.code.unwrap_or_default();
                    let code = if bytecode.is_empty() {
                        Buf::default()
                    } else {
                        bytecode.original_byte_slice().to_vec().into()
                    };
                    let account = Account {
                        value: Int::from(account.info.balance.to_be_bytes::<32>().as_slice()),
                        nonce: account.info.nonce.into(),
                        code: (code, Int::ZERO),
                    };
                    let acc = Acc::from(address.as_slice());
                    (acc, account, storage)
                })
                .collect(),
        }
    }
}
