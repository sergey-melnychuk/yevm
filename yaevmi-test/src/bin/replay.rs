use std::{
    fs::File,
    io::{BufReader, BufWriter, Read, Write},
    path::Path,
    time::Instant,
};

use alloy_provider::ProviderBuilder;
use eyre::OptionExt;
use futures::{StreamExt, channel::mpsc};
use yaevmi_base::int;
use yaevmi_core::{
    cache::Cache,
    call::Receipt,
    chain::{Chain, Fetched},
    exe::{CallResult, Executor},
    rpc::Rpc,
    state::State,
};
use yaevmi_misc::hex::parse_vec;

const YAEVMI_RPC_URL: &str = "YAEVMI_RPC_URL";

/*
cargo build --release --bin replay
./target/release/replay >replay.log 2>&1
*/
#[tokio::main]
async fn main() -> eyre::Result<()> {
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

    std::fs::create_dir_all("fetch")?;
    let path = format!("fetch/{}.json", block);
    let fetches = Path::new(&path);
    let block = if fetches.exists() && index.is_none() {
        let file = File::open(fetches)?;
        let mut reader = BufReader::new(file);
        let mut content = String::new();
        reader.read_to_string(&mut content)?;
        let fetched: Vec<Fetched> = serde_json::from_str(&content)?;
        let Some(Fetched::Block(block)) = fetched.first().cloned() else {
            eyre::bail!("Cannot find stored block");
        };
        cache.prefetched(fetched);
        block
    } else {
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

    let provider = ProviderBuilder::new().connect(&url).await?;
    tokio::task::spawn_blocking(move || {
        let (txs, head, index, network_chain_id) = pack;
        if let Some(i) = index {
            let tx = &txs[i];
            let (call, tx) = (tx.call.clone().into(), tx.tx.clone());
            if let Err(e) = live::run_one(call, tx, head, network_chain_id, rtx, provider) {
                eprintln!("REVM replay error (no trace steps): {e:#}");
            }
        } else if let Err(e) = live::run_all(network_chain_id, &txs, head, rtx, provider) {
            eprintln!("REVM replay error (no trace steps): {e:#}");
        }
    });

    let txs = if let Some(i) = index {
        vec![txs[i].clone()]
    } else {
        txs
    };

    let n = txs.len();
    let mut ok = 0;
    let mut gas_total = 0;
    let mut ms_total = 0;
    for (i, tx) in txs.into_iter().enumerate() {
        if std::env::var("TRACE").is_ok() {
            println!("{}", serde_json::to_string_pretty(&tx).unwrap());
        }

        let hash = tx.tx.hash;
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

        let violations = check(result, receipt);
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

fn check(result: CallResult, receipt: Receipt) -> Vec<String> {
    let mut ret = Vec::new();
    let used_gas = receipt.gas_used.as_u64() as i64;
    match result {
        CallResult::Done {
            status,
            ret: _,
            gas,
        } => {
            if status != receipt.status {
                ret.push(format!(
                    " ok: have {} want {}",
                    status.as_u8(),
                    receipt.status.as_u8()
                ));
            }
            if gas.finalized != used_gas {
                ret.push(format!("gas: have {} want {}", gas.finalized, used_gas));
            }
        }
        CallResult::Created { acc, code: _, gas } => {
            if Some(acc) != receipt.contract_address {
                ret.push(format!(
                    "new: have {} want {}",
                    acc,
                    receipt.contract_address.unwrap_or_default()
                ));
            }
            if gas.finalized != used_gas {
                ret.push(format!("gas: have {} want {}", gas.finalized, used_gas));
            }
        }
    }
    ret
}

// TODO: run embedded database for acc/state storage
// consider: sqlite, leveldb, rocksdb, sled, yakvdb?

// TODO: for each processed block: generate hermetic env
// (containing all read storage cells by all transactions)
// (store it alongsize with block updates to allow reverting)
// (this allows re-running blocks on-demand without RPC calls)

mod live {
    use alloy_eip7702::{Authorization, SignedAuthorization};
    use alloy_primitives::{Address as AlloyAddress, U256 as AlloyU256};
    use alloy_provider::Provider;
    use revm::bytecode::opcode::OpCode;
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
    use yaevmi_core::trace::Step;
    use yaevmi_core::{Call, Head, Tx};
    use yaevmi_misc::buf::Buf;

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
                }

                if step.name == "CALLER" {
                    let caller = interp.stack.peek(0).unwrap_or_default();
                    step.debug.push(format!("CALLER: {caller:0x}"));
                }
                if step.name == "BALANCE" {
                    let balance = interp.stack.peek(0).unwrap_or_default();
                    step.debug.push(format!("BALANCE: {balance:0x}"));
                }
                // let target = interp.input.target_address;
                // let balance = ctx
                //     .balance(interp.input.target_address)
                //     .map(|state| state.data)
                //     .unwrap_or_default();
                // step.debug
                //     .push(format!("TARGET: {target:0x} (balance={balance:0x})"));

                if let Some(tx) = self.tx.as_ref()
                    && tx.blocking_send(step).is_err() {
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
        provider: impl Provider + Clone,
    ) -> eyre::Result<()> {
        let to_addr = |a: &Acc| Address::from(<[u8; 20]>::try_from(a.as_ref()).unwrap());
        let to_u256 = |i: &Int| U256::from_be_bytes(<[u8; 32]>::try_from(i.as_ref()).unwrap());
        let to_b256 = |i: &Int| B256::from(<[u8; 32]>::try_from(i.as_ref()).unwrap());

        let db = AlloyDB::new(provider, BlockId::from(to_b256(&head.parent_hash)));
        let db = WrapDatabaseAsync::new(db).unwrap();
        let db = CacheDB::new(db);

        let mut ctx = Context::mainnet().with_db(db);
        ctx.block.number = U256::from(head.number.as_u64());
        ctx.block.timestamp = to_u256(&head.timestamp);
        ctx.block.gas_limit = head.gas_limit.as_u64();
        ctx.block.beneficiary = to_addr(&head.coinbase);
        ctx.block.basefee = head.base_fee.as_u64();
        ctx.block.prevrandao = Some(to_b256(&head.prevrandao));
        ctx.cfg.chain_id = chain_id;

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

            let kind = if call.is_create() {
                TxKind::Create
            } else {
                TxKind::Call(to_addr(&call.to))
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

            let ExecResultAndState { result: _, state } = evm.inspect_tx(tx)?;
            evm.commit(state);
        }
        let _ = evm.inspector.tx.take();
        Ok(())
    }

    #[allow(dead_code)]
    pub fn run_one(
        call: Call,
        tx: Tx,
        head: Head,
        network_chain_id: u64,
        sender: mpsc::Sender<Step>,
        provider: impl Provider + Clone,
    ) -> eyre::Result<()> {
        let to_addr = |a: &Acc| Address::from(<[u8; 20]>::try_from(a.as_ref()).unwrap());
        let to_u256 = |i: &Int| U256::from_be_bytes(<[u8; 32]>::try_from(i.as_ref()).unwrap());
        let to_b256 = |i: &Int| B256::from(<[u8; 32]>::try_from(i.as_ref()).unwrap());

        let db = AlloyDB::new(provider, BlockId::from(to_b256(&head.parent_hash)));
        let db = WrapDatabaseAsync::new(db).unwrap();
        let db = CacheDB::new(db);

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

        let kind = if call.is_create() {
            TxKind::Create
        } else {
            TxKind::Call(to_addr(&call.to))
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
        let ExecResultAndState {
            result: _,
            state: _,
        } = evm.inspect_tx(tx_env)?;
        let _ = evm.inspector.tx.take();
        Ok(())
    }
}
