use revm::bytecode::Bytecode;
use revm::bytecode::opcode::OpCode;
use revm::context::transaction::{AccessList, AccessListItem};
use revm::context::{ContextTr, TxEnv};
use revm::context_interface::result::{ExecResultAndState, ExecutionResult, Output};
use revm::database::InMemoryDB;
use revm::inspector::InspectorEvmTr;
use revm::interpreter::interpreter_types::{Immediates, Jumps};
use revm::interpreter::{CallInputs, CallOutcome, CreateInputs, CreateOutcome};
use revm::interpreter::{Interpreter, interpreter::EthInterpreter};
use revm::primitives::{Address, B256, Bytes, TxKind, U256, hardfork::SpecId};
use revm::state::AccountInfo;
use revm::{Context, InspectEvm, Inspector, MainBuilder, MainContext};
use yaevmi_base::{Acc, Int};
use yaevmi_core::state::Account;
use yaevmi_core::trace::Step;
use yaevmi_core::{Call, Head, Tx};
use yaevmi_misc::buf::Buf;

use yaevmi_core::cache::Env;

#[derive(Debug, Default)]
pub struct Tracer {
    pub traces: Vec<Step>,
    step: Option<Step>,
    refund: i64,
    gas: u64,
    depth: usize,
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
            if refund > 0 {
                step.debug.push(format!("refund={refund}"));
            }
            step.debug.push(format!("depth={}", self.depth));
            self.traces.push(step);
        }
    }

    fn call(&mut self, _: &mut CTX, _: &mut CallInputs) -> Option<CallOutcome> {
        self.depth += 1;
        None
    }

    fn call_end(&mut self, _: &mut CTX, _: &CallInputs, _: &mut CallOutcome) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn create(&mut self, _: &mut CTX, _: &mut CreateInputs) -> Option<CreateOutcome> {
        self.depth += 1;
        None
    }

    fn create_end(&mut self, _: &mut CTX, _: &CreateInputs, _: &mut CreateOutcome) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn selfdestruct(&mut self, _: Address, _: Address, _: U256) {
        self.depth = self.depth.saturating_sub(1);
    }
}

pub async fn run(
    call: Call,
    head: Head,
    env: Env,
    tx: Tx,
) -> eyre::Result<(Int, Buf, i64, Vec<Step>, Env)> {
    let to_addr = |a: &Acc| Address::from(<[u8; 20]>::try_from(a.as_ref()).unwrap());
    let to_u256 = |i: &Int| U256::from_be_bytes(<[u8; 32]>::try_from(i.as_ref()).unwrap());
    let to_b256 = |i: &Int| B256::from(<[u8; 32]>::try_from(i.as_ref()).unwrap());

    let mut db = InMemoryDB::default();
    for (acc, account, storage) in &env {
        let addr = to_addr(acc);
        let code = if account.code.0.0.is_empty() {
            None
        } else {
            Some(Bytecode::new_raw(Bytes::from(account.code.0.0.clone())))
        };
        db.insert_account_info(
            addr,
            AccountInfo {
                balance: to_u256(&account.value),
                nonce: account.nonce.as_u64(),
                code_hash: to_b256(&account.code.1),
                code,
                account_id: None,
            },
        );
        for (slot, value) in storage {
            db.insert_account_storage(addr, to_u256(slot), to_u256(value))
                .unwrap();
        }
    }

    let mut ctx = Context::mainnet().with_db(db);
    ctx.block.number = U256::from(head.number.as_u64());
    ctx.block.timestamp = to_u256(&head.timestamp);
    ctx.block.gas_limit = head.gas_limit.as_u64();
    ctx.block.beneficiary = to_addr(&head.coinbase);
    ctx.block.basefee = head.base_fee.as_u64();
    ctx.block.prevrandao = Some(to_b256(&head.prevrandao));

    // RPC legacy txs often omit chainId → tx.chain_id == 0; REVM rejects cfg.chain_id == 0.
    ctx.cfg.chain_id = if tx.chain_id.is_zero() {
        1
    } else {
        tx.chain_id.as_u64()
    };
    ctx.cfg.set_spec_and_mainnet_gas_params(SpecId::OSAKA);

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
                    storage_keys: item.storage_keys.iter().map(to_b256).collect::<Vec<B256>>(),
                })
                .collect::<Vec<AccessListItem>>(),
        ))
        .max_fee_per_gas(max_fee)
        .gas_priority_fee(Some(priority_fee))
        .authorization_list(vec![]) // TODO: FIXME
        .blob_hashes(
            tx.blob_versioned_hashes
                .iter()
                .map(to_b256)
                .collect::<Vec<B256>>(),
        )
        .max_fee_per_blob_gas(tx.max_fee_per_blob_gas.unwrap_or_default().as_u128())
        .build()
        .map_err(|e| eyre::eyre!("{e:?}"))?;

    let mut evm = ctx.build_mainnet_with_inspector(Tracer::default());
    let ExecResultAndState { result, state } = evm.inspect_tx(tx)?;
    let steps = std::mem::take(&mut evm.inspector().traces);

    let from_u256 = |u: U256| -> Int { Int::from(&u.to_be_bytes::<32>()[..]) };
    let from_b256 = |b: B256| -> Int { Int::from(b.as_slice()) };

    // Build snapshot: state from inspect_tx contains only modified accounts.
    // Merge with pre-state (env) so we include untouched accounts.
    let mut snapshot: std::collections::HashMap<Acc, (Account, Vec<(Int, Int)>)> = env
        .iter()
        .map(|(acc, account, storage)| {
            let mut kv: Vec<_> = storage.iter().map(|(k, v)| (*k, *v)).collect();
            kv.sort_by_key(|(k, _)| *k);
            (
                *acc,
                (
                    Account {
                        value: account.value,
                        nonce: account.nonce,
                        code: account.code.clone(),
                    },
                    kv,
                ),
            )
        })
        .collect();

    let selfdestructed: std::collections::HashSet<_> = state
        .iter()
        .filter(|(_, a)| a.is_selfdestructed())
        .map(|(addr, _)| Acc::from(addr.as_slice()))
        .collect();
    for addr in &selfdestructed {
        snapshot.remove(addr);
    }

    for (addr, account) in &state {
        if account.is_selfdestructed() {
            continue;
        }
        let acc = Acc::from(addr.as_slice());
        let code_bytes = account
            .info
            .code
            .as_ref()
            .map(|c| c.original_bytes().to_vec())
            .unwrap_or_default();
        let code_hash = if code_bytes.is_empty() {
            Int::ZERO
        } else {
            from_b256(account.info.code_hash)
        };
        let state_storage: std::collections::HashMap<_, _> = account
            .storage
            .iter()
            .map(|(slot, val)| (from_u256(*slot), from_u256(val.present_value)))
            .collect();
        // Merge storage: state may only have modified slots; overlay on pre-state
        let mut kv: Vec<_> = snapshot
            .get(&acc)
            .map(|(_, s)| s.clone())
            .unwrap_or_default()
            .into_iter()
            .collect();
        for (k, v) in &state_storage {
            kv.retain(|(kk, _)| kk != k);
            kv.push((*k, *v));
        }
        kv.sort_by_key(|(k, _)| *k);
        snapshot.insert(
            acc,
            (
                Account {
                    value: from_u256(account.info.balance),
                    nonce: Int::from(account.info.nonce),
                    code: (Buf(code_bytes), code_hash),
                },
                kv,
            ),
        );
    }

    let mut snapshot: Env = snapshot
        .into_iter()
        .map(|(acc, (account, storage))| (acc, account, storage))
        .filter(|(_, account, storage)| {
            let is_empty = account.value.is_zero()
                && account.nonce.is_zero()
                && account.code.0.is_empty()
                && (storage.is_empty() || storage.iter().all(|(_, v)| v.is_zero()));
            !is_empty
        })
        .collect();
    snapshot.sort_by_key(|(acc, _, _)| *acc);

    match result {
        ExecutionResult::Success { gas, output, .. } => match output {
            Output::Create(code, Some(addr)) => Ok((
                Acc::from(addr.as_slice()).to::<32>(),
                Buf(code.to_vec()),
                gas.used() as i64,
                steps,
                snapshot,
            )),
            Output::Create(_, None) => Err(eyre::eyre!("contract creation: no address")),
            Output::Call(bytes) => Ok((
                Int::from(1u64),
                Buf(bytes.to_vec()),
                gas.used() as i64,
                steps,
                snapshot,
            )),
        },
        ExecutionResult::Revert { gas, output, .. } => Ok((
            Int::from(0u64),
            Buf(output.to_vec()),
            gas.used() as i64,
            steps,
            snapshot,
        )),
        ExecutionResult::Halt { gas, .. } => Ok((
            Int::from(0u64),
            Buf::default(),
            gas.used() as i64,
            steps,
            snapshot,
        )),
    }
}
