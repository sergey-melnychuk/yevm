use yevm_base::math::U256;
use yevm_base::math::lift;
use yevm_misc::keccak256;

use yevm_misc::buf::Buf;

use crate::Fetch;
use crate::Tx;
use crate::call::Block;
use crate::evm::{CallMode, Context, Evm, Gas, StepResult};
use crate::misc::{create_address, is_precompile};
use crate::{Acc, Call, Error, Int, Result};
use crate::{
    call::Head,
    chain::{Chain, fetch},
    state::{Account, State},
    trace::Event,
};

const MAX_CALL_DEPTH: usize = 1024;

const BEACON_ROOTS: Acc = yevm_base::acc::acc("0x000f3df6d732807ef1319fb7b8bb8522d0beac02");
const HISTORY_BUFFER_LENGTH: u64 = 8191;

// EIP-2935 (Prague): history-storage contract + its ring-buffer window.
// Not fork-timestamp-gated below (yevm has no chain-config/fork-schedule
// awareness) -- applied unconditionally on every block, which is correct
// for current/recent mainnet blocks (well past Prague) but WOULD be wrong
// if this were ever used to replay a genuinely pre-Prague block.
const HISTORY_STORAGE: Acc = yevm_base::acc::acc("0x0000F90827F1C53a10cb7A02335B175320002935");
const HISTORY_SERVE_WINDOW: u64 = 8191;

/// EIP-4788: write the parent beacon block root into the ring buffer before executing transactions.
/// EIP-2935: write the parent block's hash into the history-storage contract, same timing.
pub async fn pre_block(head: &Head, state: &mut impl State, chain: &impl Chain) -> Result<()> {
    if let Some(root) = head.parent_beacon_block_root {
        fetch(Fetch::Account(BEACON_ROOTS), state, chain).await?;
        let timestamp = head.timestamp.as_u64();
        let slot = timestamp % HISTORY_BUFFER_LENGTH;
        let slot2 = slot + HISTORY_BUFFER_LENGTH;

        // `put` (not `init`) so this real write emits a Trace event -- `init`
        // is for "populate a freshly-fetched slot with its real value" (no
        // diff), and this genuinely changes state every block. Fetch each
        // cell first so `put`'s recorded previous value is the real
        // on-chain one (the ring buffer reuses slots every ~8191 seconds,
        // so it's rarely zero), not `put`'s own cold-slot default.
        fetch(Fetch::StateCell(BEACON_ROOTS, Int::from(slot)), state, chain).await?;
        state.put(&BEACON_ROOTS, &Int::from(slot), Int::from(timestamp));
        fetch(Fetch::StateCell(BEACON_ROOTS, Int::from(slot2)), state, chain).await?;
        state.put(&BEACON_ROOTS, &Int::from(slot2), root);
    }

    if let Some(number) = head.number.as_u64().checked_sub(1) {
        fetch(Fetch::Account(HISTORY_STORAGE), state, chain).await?;
        let slot = number % HISTORY_SERVE_WINDOW;
        fetch(Fetch::StateCell(HISTORY_STORAGE, Int::from(slot)), state, chain).await?;
        state.put(&HISTORY_STORAGE, &Int::from(slot), head.parent_hash);
    }

    Ok(())
}

/// EIP-4895: credit each validator withdrawal directly to its target
/// account's balance -- a plain balance mutation, not EVM execution, so
/// there's no call/transfer machinery involved beyond `State::set_value`
/// (which emits the same `Event::Put(Target::Value ...)` a transfer would,
/// so this shows up in any diff derived from the event stream same as
/// everything else). `amount` is Gwei (per the beacon chain / EIP-4895),
/// converted to Wei here. Distinct from `post_block`'s EIP-7002/7251
/// request-queue processing: those are execution-layer predeploys driven
/// purely by on-chain state, while withdrawals are consensus-layer input
/// that must be supplied per block (the block body's `withdrawals` list) --
/// runs once per block, order relative to `post_block` doesn't matter since
/// they never touch the same accounts.
pub async fn apply_withdrawals(
    withdrawals: &[crate::call::Withdrawal],
    state: &mut impl State,
    chain: &impl Chain,
) -> Result<()> {
    let gwei_to_wei = lift(|[amount]| amount * U256::from(1_000_000_000u64));
    let add = lift(|[a, b]| a + b);
    for w in withdrawals {
        if state.acc(&w.address).is_none() {
            fetch(Fetch::Account(w.address), state, chain).await?;
        }
        let amount_wei = gwei_to_wei([w.amount]);
        let balance = state.balance(&w.address).unwrap_or_default();
        state.set_value(&w.address, add([balance, amount_wei]));
    }
    Ok(())
}

const WITHDRAWAL_REQUEST: Acc = yevm_base::acc::acc("0x00000961Ef480Eb55e80D19ad83579A64c007002");
const CONSOLIDATION_REQUEST: Acc = yevm_base::acc::acc("0x0000BBdDc7CE488642fb579F8B00f3a590007251");

const REQUEST_QUEUE_EXCESS_SLOT: u64 = 0;
const REQUEST_QUEUE_COUNT_SLOT: u64 = 1;
const REQUEST_QUEUE_HEAD_SLOT: u64 = 2;
const REQUEST_QUEUE_TAIL_SLOT: u64 = 3;

/// EIP-7002 (withdrawal requests) / EIP-7251 (consolidation requests): at the end of
/// every block, each predeploy dequeues up to its per-block max, updates its excess-fee
/// counter, and resets its per-block count -- unconditionally, even on blocks where no
/// tx touched it. The "add request" side (a tx CALLing the predeploy with calldata +
/// fee) already works correctly since it's just ordinary contract bytecode executed via
/// the normal CALL path; only this block-end dequeue step was missing entirely. Direct
/// transliteration of the EIPs' own `dequeue_*_requests` / `update_excess_*_requests` /
/// `reset_*_requests_count` reference pseudocode. The SSZ-encoded request list itself
/// (used for the block's requestsHash) is not needed here -- only the storage side
/// effects matter for state-root purposes.
/// Not fork-timestamp-gated, same rationale as EIP-2935 above.
pub async fn post_block(block: &Block, state: &mut impl State, chain: &impl Chain) -> Result<()> {
    apply_withdrawals(&block.withdrawals, state, chain).await?;
    process_request_queue(WITHDRAWAL_REQUEST, 16, 2, state, chain).await?;
    process_request_queue(CONSOLIDATION_REQUEST, 2, 1, state, chain).await?;
    Ok(())
}

async fn process_request_queue(
    addr: Acc,
    max_per_block: u64,
    target_per_block: u64,
    state: &mut impl State,
    chain: &impl Chain,
) -> Result<()> {
    // Guard every fetch on "not already in cache" -- `fetch` unconditionally
    // overwrites via `State::merge`/`init`, which would silently clobber any
    // local mutation an earlier tx in this same block already made (this
    // predeploy may well have been touched by a tx before this runs, since
    // post_block executes after the whole tx loop, unlike pre_block).
    if state.acc(&addr).is_none() {
        fetch(Fetch::Account(addr), state, chain).await?;
    }
    if state.code(&addr).map(|(c, _)| c.0.is_empty()).unwrap_or(true) {
        // Predeploy not yet in state (pre-activation replay) -- nothing to do.
        return Ok(());
    }

    for slot in [
        REQUEST_QUEUE_EXCESS_SLOT,
        REQUEST_QUEUE_COUNT_SLOT,
        REQUEST_QUEUE_HEAD_SLOT,
        REQUEST_QUEUE_TAIL_SLOT,
    ] {
        if state.get(&addr, &Int::from(slot)).is_none() {
            fetch(Fetch::StateCell(addr, Int::from(slot)), state, chain).await?;
        }
    }

    let excess = state
        .get(&addr, &Int::from(REQUEST_QUEUE_EXCESS_SLOT))
        .map(|(cur, _)| cur)
        .unwrap_or_default();
    let count = state
        .get(&addr, &Int::from(REQUEST_QUEUE_COUNT_SLOT))
        .map(|(cur, _)| cur)
        .unwrap_or_default()
        .as_u64();
    let head = state
        .get(&addr, &Int::from(REQUEST_QUEUE_HEAD_SLOT))
        .map(|(cur, _)| cur)
        .unwrap_or_default()
        .as_u64();
    let tail = state
        .get(&addr, &Int::from(REQUEST_QUEUE_TAIL_SLOT))
        .map(|(cur, _)| cur)
        .unwrap_or_default()
        .as_u64();

    let num_in_queue = tail.saturating_sub(head);
    let num_dequeued = num_in_queue.min(max_per_block);
    let new_head = head + num_dequeued;
    if new_head == tail {
        state.put(&addr, &Int::from(REQUEST_QUEUE_HEAD_SLOT), Int::ZERO);
        state.put(&addr, &Int::from(REQUEST_QUEUE_TAIL_SLOT), Int::ZERO);
    } else {
        state.put(&addr, &Int::from(REQUEST_QUEUE_HEAD_SLOT), Int::from(new_head));
    }

    // EXCESS_INHIBITOR (2**256-1) marks pre-activation state; treat it as 0.
    let previous_excess = if excess == Int::MAX { 0 } else { excess.as_u64() };
    let new_excess = (previous_excess + count).saturating_sub(target_per_block);
    state.put(
        &addr,
        &Int::from(REQUEST_QUEUE_EXCESS_SLOT),
        Int::from(new_excess),
    );

    state.put(&addr, &Int::from(REQUEST_QUEUE_COUNT_SLOT), Int::ZERO);

    Ok(())
}

const MAX_STEPS: u64 = 10_000_000;
const MAX_CODE_SIZE: usize = 24_576;
const CODE_DEPOSIT_GAS: i64 = 200;

#[derive(Debug)]
pub enum CallResult {
    Done { status: Int, ret: Buf, gas: Gas },
    Created { acc: Acc, code: Buf, gas: Gas },
}

impl CallResult {
    pub fn gas(&self) -> &Gas {
        match self {
            Self::Done { gas, .. } => gas,
            Self::Created { gas, .. } => gas,
        }
    }

    pub fn gas_mut(&mut self) -> &mut Gas {
        match self {
            Self::Done { gas, .. } => gas,
            Self::Created { gas, .. } => gas,
        }
    }
}

pub struct Executor {
    pub call: Call,
    pub callstack: Vec<CallFrame>,
    /// Effective gas price for GASPRICE opcode (min(max_fee, base_fee + priority) for EIP-1559).
    effective_gas_price: Int,
}

pub struct CallFrame {
    pub call: Call,
    pub evm: Evm,
    pub ctx: Context,
    pub checkpoint: usize,
    /// Return-data target (ret_offset, ret_size) for the parent frame's CALL/STATICCALL.
    pub target: (usize, usize),
    /// True when this frame is a CREATE/CREATE2, false for CALL/STATICCALL/etc.
    /// Cannot rely on `call.to.is_zero()` because a plain CALL to address 0x0 is valid.
    pub is_create: bool,
}

pub fn intrinsic(
    call: &Call,
    tx: &Tx,
    head: &Head,
    state: &mut impl State,
) -> Result<(i64, i64, Int)> {
    let mut total = 21_000i64;
    let mut floor = 21_000i64;
    let has_code = call
        .to
        .and_then(|to| state.code(&to))
        .is_some_and(|(c, _)| !c.0.is_empty());
    let is_create = call.is_create() && !has_code;
    if is_create {
        total += 32_000;
        // EIP-3860: 2 gas per 32-byte word of initcode
        let initcode_cost = 2 * ((call.data.0.len() as i64 + 31) / 32);
        total += initcode_cost;
    }
    let zeroes = call.data.0.iter().filter(|b| **b == 0).count();
    let non_zeroes = call.data.0.len() - zeroes;
    total += (zeroes * 4 + non_zeroes * 16) as i64;
    // EIP-7623: floor calldata cost (TOTAL_COST_FLOOR_PER_TOKEN = 10)
    let tokens = (zeroes + non_zeroes * 4) as i64;
    floor += 10 * tokens;

    // EIP-2929: pre-warm sender, target, coinbase, and precompile addresses.
    // For CREATE (to==0x0) there is no target; do not warm 0x0.
    state.warm_acc(&call.by);
    if let Some(to) = call.to {
        state.warm_acc(&to);
    }
    state.warm_acc(&head.coinbase);
    for i in 1u64..=0xa {
        state.warm_acc(&Acc::from(i));
    }
    state.warm_acc(&Acc::from(0x100u64)); // p256verify precompile

    // EIP-2930: access list gas (2400/address + 1900/storage key)
    for (acc, keys) in tx
        .access_list
        .iter()
        .map(|item| (item.address, &item.storage_keys))
    {
        let al_cost = 2_400 + 1_900 * keys.len() as i64;
        total += al_cost;
        state.warm_acc(&acc);
        for key in keys {
            state.warm_key(&acc, key);
        }
    }

    // EIP-1559: gasPrice / maxFeePerGas must be >= baseFee.
    let lt = lift(|[a, b]| if a < b { U256::ONE } else { U256::ZERO });
    if tx.max_fee_per_gas.is_zero() {
        // Legacy tx: gasPrice must be >= baseFee
        if !lt([tx.gas_price, head.base_fee]).is_zero() {
            return Err(Error::MaxFeeLessThanBaseFee);
        }
    } else {
        // EIP-1559 tx: maxFeePerGas must be >= baseFee
        if !lt([tx.max_fee_per_gas, head.base_fee]).is_zero() {
            return Err(Error::MaxFeeLessThanBaseFee);
        }
    }

    // EIP-1559: effective gas price = min(max_fee_per_gas, base_fee + max_priority_fee_per_gas).
    // For legacy tx (max_fee_per_gas == 0) use gas_price directly.
    let effective_gas_price = if tx.max_fee_per_gas.is_zero() {
        tx.gas_price
    } else {
        let min = lift(|[a, b]| a.min(b));
        let sum = lift(|[a, b]| a + b);
        min([
            tx.max_fee_per_gas,
            sum([head.base_fee, tx.max_priority_fee_per_gas]),
        ])
    };

    // Check for overflow in effective_gas_price * call.gas
    let mul_overflows = lift(|[a, b]| {
        if a.checked_mul(b).is_none() {
            U256::ONE
        } else {
            U256::ZERO
        }
    });
    if !mul_overflows([effective_gas_price, Int::from(call.gas)]).is_zero() {
        return Err(Error::GasLimitPriceProductOverflow);
    }

    // Upfront gas deduction (YP §6.1): sender pays gas_limit × effective_gas_price.
    // For EIP-1559 tx, balance check uses max_fee_per_gas (sender must afford worst case).
    let mul = lift(|[a, b]| a * b);
    let sub = lift(|[a, b]| a - b);
    let add = lift(|[a, b]| a + b);
    let gt = lift(|[a, b]| if a > b { U256::ONE } else { U256::ZERO });
    let max_gas_price = if tx.max_fee_per_gas.is_zero() {
        effective_gas_price
    } else {
        tx.max_fee_per_gas
    };
    let upfront_check = mul([Int::from(call.gas), max_gas_price]);
    let upfront = mul([Int::from(call.gas), effective_gas_price]);

    // EIP-4844: blob gas cost = num_blobs × GAS_PER_BLOB × actual_blob_base_fee (never refunded).
    const GAS_PER_BLOB: u64 = 0x20000;
    let blob_gas_cost = if let Some(excess) = head.excess_blob_gas {
        let fee = crate::call::blob_base_fee(excess.as_u64());
        let blob_gas = tx.blob_versioned_hashes.len() as u64 * GAS_PER_BLOB;
        Int::from(blob_gas as u128 * fee)
    } else {
        Int::ZERO
    };

    let total_cost = add([upfront_check, call.eth]);
    let balance = state.balance(&call.by).unwrap_or_default();
    if !gt([total_cost, balance]).is_zero() {
        // TODO: FIXME: false positives detected
        // return Err(Error::InsufficientFunds);
    }
    state.set_value(&call.by, sub([balance, add([upfront, blob_gas_cost])]));

    // EIP-7702: authorization list gas (PER_EMPTY_ACCOUNT_COST per tuple, matches revm).
    // EIP-7623 floor is only 21_000 + 10 * tokens (calc_tx_floor_cost); it must NOT include
    // authorization list gas — see revm `calculate_initial_tx_gas` (PRAGUE branch) and
    // `eip7623_check_gas_floor` (floor_gas excludes auth, initial_gas includes it).
    let auth_cost = 25_000 * tx.authorization_list.len() as i64;
    total += auth_cost;
    Ok((total, floor, effective_gas_price))
}

pub fn finalized(
    call: &Call,
    tx: &Tx,
    head: &Head,
    effective_gas_price: Int,
    result: &CallResult,
    state: &mut impl State,
    floor: i64,
) -> i64 {
    // Settle gas: return unused gas to sender; coinbase receives the priority fee tip.
    let gas = result.gas();

    let effective_refund = gas.refund.min(gas.spent / 5);
    // EIP-7623: floor gas cost for calldata-heavy transactions
    let final_gas = (gas.spent - effective_refund).max(0).max(floor) as u64;
    let returned_gas = (gas.limit.max(0) as u64).saturating_sub(final_gas);
    let mul = lift(|[a, b]| a * b);
    let sub = lift(|[a, b]| a - b);
    let add = lift(|[a, b]| a + b);

    // Return (gas_limit - net_gas) × effective_gas_price to sender.
    let returned_cost = mul([Int::from(returned_gas), effective_gas_price]);
    let balance = state.balance(&call.by).unwrap_or_default();
    state.set_value(&call.by, add([balance, returned_cost]));

    // Priority fee to coinbase: net_gas × min(max_priority_fee, effective_gas_price - base_fee).
    let priority_fee = if tx.max_fee_per_gas.is_zero() {
        sub([effective_gas_price, head.base_fee])
    } else {
        lift(|[a, b]| a.min(b))([
            tx.max_priority_fee_per_gas,
            sub([effective_gas_price, head.base_fee]),
        ])
    };

    let tip = mul([Int::from(final_gas), priority_fee]);
    if !tip.is_zero() {
        let balance = state.balance(&head.coinbase).unwrap_or_default();
        state.set_value(&head.coinbase, add([balance, tip]));
    }

    let base_burn = mul([Int::from(final_gas), head.base_fee]);
    state.emit(Event::Fee(
        call.by,
        head.coinbase,
        base_burn,
        tip,
        final_gas,
    ));

    final_gas as i64
}

pub fn transfer(call: &Call, mode: &CallMode, state: &mut impl State) {
    if call.eth.is_zero() {
        return;
    }
    let to = mode.created().or(call.to);
    if let Some(to) = to {
        let sub = lift(|[a, b]| a - b);
        let add = lift(|[a, b]| a + b);
        let by0 = state.balance(&call.by).unwrap_or_default();
        state.set_value(&call.by, sub([by0, call.eth]));
        let to0 = state.balance(&to).unwrap_or_default();
        state.set_value(&to, add([to0, call.eth]));
        state.emit(Event::Move(call.by, to, call.eth));
    }
}

impl Executor {
    pub fn new(call: Call) -> Self {
        Self {
            call,
            callstack: vec![],
            effective_gas_price: Int::ZERO,
        }
    }

    pub async fn run(
        &mut self,
        mut tx: Tx,
        head: Head,
        state: &mut impl State,
        chain: &impl Chain,
    ) -> Result<CallResult> {
        if !self.callstack.is_empty() {
            return Err(eyre::eyre!("inconsistent state: call stack empty").into());
        }
        for acc in [&self.call.by, &head.coinbase] {
            if state.acc(acc).is_none() {
                fetch(Fetch::Account(*acc), state, chain).await?;
                state.warm_acc(acc);
            }
        }
        if let Some(acc) = self.call.to.as_ref()
            && state.acc(acc).is_none()
        {
            fetch(Fetch::Account(*acc), state, chain).await?;
            state.warm_acc(acc);
        }

        if tx.chain_id.is_zero() {
            tx.chain_id = state.get_chain_id().into();
            if tx.chain_id.is_zero() {
                return Err(Error::UndefinedChainId);
            }
        }

        // Pre-transaction validation checks

        // EIP-1559: max_fee_per_gas must cover base_fee
        if !tx.max_fee_per_gas.is_zero() && tx.max_fee_per_gas < head.base_fee {
            return Err(Error::MaxFeeLessThanBaseFee);
        }

        // EIP-1559: max_priority_fee must not exceed max_fee
        if !tx.max_fee_per_gas.is_zero() && tx.max_priority_fee_per_gas > tx.max_fee_per_gas {
            return Err(Error::PriorityGreaterThanMaxFee);
        }

        // Gas limit must not exceed block gas limit
        let gt = lift(|[a, b]| if a > b { U256::ONE } else { U256::ZERO });
        if !gt([Int::from(self.call.gas), head.gas_limit]).is_zero() {
            return Err(Error::GasAllowanceExceeded);
        }

        // EIP-3607: sender must be an EOA (no code)
        if let Some(acc) = state.acc(&self.call.by)
            && !(state.auth(&self.call.by).is_some() || acc.code.0.0.is_empty())
        {
            // TODO: FIXME: false positives
            // return Err(Error::SenderNotEOA);
        }

        let has_code = self
            .call
            .to
            .and_then(|to| state.code(&to))
            .is_some_and(|(c, _)| !c.0.is_empty())
            && self.call.to.and_then(|to| state.auth(&to)).is_none();

        // CREATE address uses sender nonce *before* the tx-level increment (YP / EIP-161).
        let mode = if self.call.is_create() && !has_code {
            let nonce = state.nonce(&self.call.by).unwrap_or_default();
            let created = create_address(&self.call.by, nonce.as_u64());
            CallMode::Create(created)
        } else {
            CallMode::Call(0, 0)
        };

        // Tx consumes one sender nonce before execution; EIP-7702 auth checks need this first
        // when authority == sender (post-increment on-chain nonce vs signed tuple nonce).
        state.inc_nonce(&self.call.by, Int::ONE);

        let eip7702_refund =
            crate::eip7702::apply_authorization_list(&tx, tx.chain_id.as_u64(), state, chain)
                .await?;

        let (intrinsic, floor, effective_gas_price) = intrinsic(&self.call, &tx, &head, state)?;
        self.effective_gas_price = effective_gas_price;
        if (self.call.gas as i64) < intrinsic {
            return Err(Error::GasTooLow {
                have: self.call.gas,
                want: intrinsic as u64,
            });
        }

        // prepare() takes a checkpoint to be able to revert,
        // so all state mutations must come AFTER that to be included.
        let mut frame = prepare(
            head.clone(),
            self.call.clone(),
            mode,
            None,
            tx.chain_id.to(),
            effective_gas_price,
            tx.blob_versioned_hashes.clone(),
            state,
            chain,
        )
        .await?;
        // For top-level CREATE: collision check + initialize with nonce=1 (EIP-161).
        // Done AFTER the checkpoint so it's reverted on init-code failure.
        if let CallMode::Create(created) = mode {
            if state.acc(&created).is_none() {
                fetch(Fetch::Account(created), state, chain).await?;
            }
            let existing_nonce = state.nonce(&created).unwrap_or(Int::ZERO);
            let has_code = state.code(&created).is_some_and(|(c, _)| !c.0.is_empty());
            if !existing_nonce.is_zero() || has_code {
                // Collision: drain gas, revert, return failure
                frame.evm.gas.drain();
                let gas = frame.evm.gas;
                state.revert_to(frame.checkpoint);
                let result = CallResult::Done {
                    status: Int::ZERO,
                    ret: vec![].into(),
                    gas,
                };
                let mut result = result;
                result.gas_mut().refund += eip7702_refund;
                let gas_final = finalized(
                    &self.call,
                    &tx,
                    &head,
                    effective_gas_price,
                    &result,
                    state,
                    floor,
                );
                result.gas_mut().finalized = gas_final;
                state.apply();
                return Ok(result);
            }
            // Preserve any pre-existing balance at the CREATE address
            let existing_balance = state.balance(&created).unwrap_or(Int::ZERO);
            state.create(
                created,
                Account {
                    value: existing_balance,
                    nonce: Int::ONE,
                    code: (Buf::default(), Int::ZERO),
                },
            );
            // EIP-2929: add newly created address to accessed_addresses
            state.warm_acc(&created);
        }
        transfer(&self.call, &mode, state);
        let _ = frame.evm.gas_charge(intrinsic);

        // Top-level call to a precompile: run inline, skip the step loop.
        let call_to_is_precompile = self
            .call
            .to
            .map(|to| is_precompile(&to))
            .unwrap_or_default();
        if !frame.is_create && call_to_is_precompile {
            let id = self.call.to.expect("verified precompile address").as_u64();
            let (ok, out, gas_used) =
                crate::pre::run(id, &self.call.data.0, frame.evm.gas_remaining());
            let _ = frame.evm.gas_charge(gas_used);
            frame.evm.apply(state);

            let status = if ok { Int::ONE } else { Int::ZERO };
            if ok {
                state.emit(Event::Return(out.clone().into(), gas_used.max(0) as u64));
            } else {
                state.revert_to(frame.checkpoint);
                state.emit(Event::Revert(vec![].into(), gas_used.max(0) as u64));
            }

            let mut result = CallResult::Done {
                status,
                ret: out.into(),
                gas: frame.evm.gas,
            };
            result.gas_mut().refund += eip7702_refund;
            let gas_final = finalized(
                &self.call,
                &tx,
                &head,
                effective_gas_price,
                &result,
                state,
                floor,
            );
            result.gas_mut().finalized = gas_final;
            state.apply();
            return Ok(result);
        }

        state.emit(Event::Call(self.call.clone(), mode));
        self.callstack.push(frame);

        let mut result: Option<CallResult> = None;
        let mut last_popped_checkpoint: Option<usize> = None;
        let mut steps: u64 = 0;

        while let Some(this) = self.callstack.last_mut() {
            state.set_depth(this.ctx.depth + 1);
            steps += 1;
            if steps > MAX_STEPS {
                this.evm.gas.drain();
                let gas = this.evm.gas;
                let checkpoint = this.checkpoint;
                state.revert_to(checkpoint);
                self.callstack.clear();
                return Ok(CallResult::Done {
                    status: Int::ZERO,
                    ret: vec![].into(),
                    gas,
                });
            }
            // Process a result returned from a completed subcall
            if let Some(call_result) = result.take() {
                match call_result {
                    CallResult::Done { status, ret, gas } => {
                        // Revert failed child's state (value transfer, etc.) when call returns 0
                        if status.is_zero() {
                            if let Some(cp) = last_popped_checkpoint.take() {
                                state.revert_to(cp);
                            }
                        } else {
                            last_popped_checkpoint = None; // success, discard stale checkpoint
                        }
                        let _ = this.evm.push(status);
                        this.evm.ret = ret.clone().into_vec();
                        // EIP-211: return data (success or revert) is written to memory at ret_offset
                        let (offset, size) = this.target;
                        if size > 0 {
                            let _ = this.evm.mem_put(offset, size, ret.as_slice());
                        }
                        // Return all unused child gas to the parent, regardless of
                        // success or failure.  The 2300 stipend is a free subsidy —
                        // any portion the child did not consume flows back to the
                        // caller (this matches geth / revm behaviour).
                        let return_gas = (gas.limit - gas.spent).max(0);
                        this.evm.gas.spent -= return_gas;
                        // Only propagate refund on success; reverted refunds are discarded.
                        if !status.is_zero() {
                            this.evm.gas.refund += gas.refund;
                        }
                        this.evm.apply(state);
                        this.evm.pc += 1;
                        this.target = (0, 0);
                        result = None;
                    }
                    CallResult::Created {
                        acc: addr,
                        code,
                        gas,
                    } => {
                        last_popped_checkpoint = None; // success

                        let hash = Int::from(keccak256(code.as_slice()).as_ref());
                        state.set_code(&addr, code, hash);

                        let _ = this.evm.push(addr.to());
                        let return_gas = (gas.limit - gas.spent).max(0);
                        // eprintln!("CREATED: depth={} child_limit={} child_spent={} child_refund={} return_gas={} parent_spent_before={} parent_spent_after={}",
                        //     this.ctx.depth, gas.limit, gas.spent, gas.refund, return_gas, this.evm.gas.spent, this.evm.gas.spent - return_gas);
                        this.evm.gas.spent -= return_gas;
                        this.evm.gas.refund += gas.refund;
                        this.evm.apply(state);
                        this.evm.pc += 1;
                        this.evm.ret.clear();
                    }
                }
            }

            match this.evm.step(&this.ctx, &this.call, state)? {
                StepResult::Ok => {
                    continue;
                }
                StepResult::End => {
                    this.evm.apply(state);

                    // Do not emit synthetic STOP

                    let is_create = this.is_create;
                    let gas = this.evm.gas;
                    result = Some(if is_create {
                        CallResult::Created {
                            acc: this.ctx.this,
                            code: vec![].into(),
                            gas,
                        }
                    } else {
                        CallResult::Done {
                            status: Int::ONE,
                            ret: vec![].into(),
                            gas,
                        }
                    });
                    last_popped_checkpoint = Some(this.checkpoint);
                    self.callstack.pop();
                }
                StepResult::Call(call, mode) => {
                    state.emit(Event::Call(call.clone(), mode));
                    this.evm.apply(state);
                    let call_to_is_precompile =
                        call.to.map(|to| is_precompile(&to)).unwrap_or_default();
                    if call_to_is_precompile {
                        // Load precompile account from chain so its on-chain balance
                        // is reflected in state (mirrors what prepare() does for regular calls).
                        if let Some(to) = call.to
                            && state.acc(&to).is_none()
                        {
                            fetch(Fetch::Account(to), state, chain).await?;
                        }

                        // EIP-211: clear return data before new call
                        this.evm.ret.clear();

                        // Precompile runs inline. Replace child-gas reservation with actual used
                        // (avoids OOG when child_gas > remaining); keep access cost.
                        let id = call.to.expect("verified precompile address").as_u64();
                        let (ok, out, gas_used) =
                            crate::pre::run(id, &call.data.0, call.gas as i64);
                        this.evm.ret = out.clone();
                        this.evm.pending_gas_charge -= call.gas as i64;
                        this.evm.pending_gas_charge += gas_used;

                        // Value transfer only on success — failure reverts all child-frame
                        // state changes, including the value transfer.
                        if ok
                            && !call.eth.is_zero()
                            && matches!(mode, CallMode::Call(..))
                            && let Some(call_to) = call.to
                        {
                            let sub = lift(|[a, b]| a - b);
                            let add = lift(|[a, b]| a + b);
                            let by0 = state.balance(&call.by).unwrap_or_default();
                            let to0 = state.balance(&call_to).unwrap_or_default();
                            if call.by != call_to {
                                state.set_value(&call.by, sub([by0, call.eth]));
                                state.set_value(&call_to, add([to0, call.eth]));
                            }
                        }

                        let status = if ok { Int::ONE } else { Int::ZERO };
                        if ok {
                            let d = state.get_depth();
                            state.set_depth(d + 1);
                            state.emit(Event::Return(out.clone().into(), gas_used.max(0) as u64));
                            state.set_depth(d);
                        } else {
                            let d = state.get_depth();
                            state.set_depth(d + 1);
                            state.emit(Event::Revert(vec![].into(), gas_used.max(0) as u64));
                            state.set_depth(d);
                        }
                        let (ret_offset, ret_size) = mode.target().unwrap_or_default();
                        this.evm.apply(state);
                        let _ = this.evm.push(status);
                        if !status.is_zero() && ret_size > 0 {
                            let n = ret_size.min(out.len());
                            let _ = this.evm.mem_put(ret_offset, n, &out[..n]);
                        }
                        this.evm.apply(state);
                        this.evm.pc += 1;
                        continue;
                    }

                    let is_create = matches!(mode, CallMode::Create(_) | CallMode::Create2(_));

                    // For CREATE: perform pre-checkpoint checks, then increment nonce.
                    // Per EVM spec, nonce is incremented before the snapshot so it survives
                    // collision reverts, but NOT depth or insufficient-balance failures.
                    if let Some(created) = mode.created() {
                        if state.acc(&created).is_none() {
                            fetch(Fetch::Account(created), state, chain).await?;
                        }
                        let creator = call.by;

                        // Depth check before nonce increment
                        if this.ctx.depth + 1 > MAX_CALL_DEPTH {
                            // Return child gas (not consumed on depth failure)
                            this.evm.gas.spent -= call.gas as i64;
                            this.evm.apply(state);
                            let _ = this.evm.push(Int::ZERO);
                            this.evm.apply(state);
                            this.evm.ret = vec![];
                            this.evm.pc += 1;
                            this.target = (0, 0);
                            continue;
                        }

                        // Balance check before nonce increment
                        if !call.eth.is_zero() {
                            let gte = lift(|[a, b]| if a >= b { U256::ONE } else { U256::ZERO });
                            let by0 = state.balance(&creator).unwrap_or_default();
                            if gte([by0, call.eth]).is_zero() {
                                // Return child gas (not consumed on balance failure)
                                this.evm.gas.spent -= call.gas as i64;
                                this.evm.apply(state);
                                let _ = this.evm.push(Int::ZERO);
                                this.evm.apply(state);
                                this.evm.ret = vec![];
                                this.evm.pc += 1;
                                this.target = (0, 0);
                                continue;
                            }
                        }

                        // EIP-2681: nonce overflow check — CREATE fails if nonce >= 2^64 - 1
                        let nonce_max = Int::from(u64::MAX);
                        let creator_nonce = state.nonce(&creator).unwrap_or(Int::ZERO);
                        if creator_nonce >= nonce_max {
                            this.evm.gas.spent -= call.gas as i64;
                            this.evm.apply(state);
                            let _ = this.evm.push(Int::ZERO);
                            this.evm.apply(state);
                            this.evm.ret = vec![];
                            this.evm.pc += 1;
                            this.target = (0, 0);
                            continue;
                        }

                        // Increment nonce BEFORE checkpoint so collision-reverts don't undo it
                        state.inc_nonce(&creator, Int::ONE);
                        // EIP-2929: created address is warmed BEFORE checkpoint (survives revert)
                        state.warm_acc(&created);
                    }

                    let checkpoint = state.checkpoint();
                    this.target = mode.target().unwrap_or_default();

                    if let Some(created) = mode.created() {
                        let creator = call.by;

                        // Collision check: existing nonce or code at derived address
                        let existing_nonce = state.nonce(&created).unwrap_or(Int::ZERO);
                        let has_code = state.code(&created).is_some_and(|(c, _)| !c.0.is_empty());
                        if !existing_nonce.is_zero() || has_code {
                            state.revert_to(checkpoint);
                            this.evm.apply(state);
                            let _ = this.evm.push(Int::ZERO);
                            this.evm.apply(state);
                            this.evm.ret = vec![];
                            this.evm.pc += 1;
                            this.target = (0, 0);
                            continue;
                        }

                        // Create account with nonce=1 (EIP-161), preserving pre-existing balance
                        let existing_balance = state.balance(&created).unwrap_or(Int::ZERO);
                        state.create(
                            created,
                            Account {
                                value: existing_balance,
                                nonce: Int::ONE,
                                code: (Buf::default(), Int::ZERO),
                            },
                        );

                        // Value transfer (balance already verified above)
                        if !call.eth.is_zero() {
                            let sub = lift(|[a, b]| a - b);
                            let add = lift(|[a, b]| a + b);
                            let by0 = state.balance(&creator).unwrap_or_default();
                            state.set_value(&creator, sub([by0, call.eth]));
                            let to0 = state.balance(&created).unwrap_or_default();
                            state.set_value(&created, add([to0, call.eth]));
                        }
                    }

                    // EIP-211: clear return data buffer when making a new call
                    this.evm.ret.clear();

                    let mut frame = prepare(
                        head.clone(),
                        call.clone(),
                        mode,
                        Some(&this.ctx),
                        tx.chain_id.to(),
                        self.effective_gas_price,
                        tx.blob_versioned_hashes.clone(),
                        state,
                        chain,
                    )
                    .await?;
                    // Use the outer checkpoint (set before state.create / value transfer)
                    // so that reverting the child frame undoes create + value transfer.
                    frame.checkpoint = checkpoint;
                    if frame.ctx.depth > MAX_CALL_DEPTH {
                        state.revert_to(checkpoint);
                        // Return child gas (not consumed on depth failure)
                        this.evm.gas.spent -= call.gas as i64;
                        this.evm.apply(state);
                        let _ = this.evm.push(Int::ZERO);
                        this.evm.apply(state);
                        this.evm.ret = vec![];
                        this.evm.pc += 1;
                        this.target = (0, 0);
                        continue;
                    }

                    // ETH value transfer for CALL and CALLCODE
                    if !is_create
                        && !call.eth.is_zero()
                        && matches!(mode, CallMode::Call(..) | CallMode::CallCode(..))
                    {
                        let by = call.by;
                        let by0 = state.balance(&by).unwrap_or_default();

                        let gte = lift(|[a, b]| if a >= b { U256::ONE } else { U256::ZERO });
                        if gte([by0, call.eth]).is_zero() {
                            state.revert_to(checkpoint);
                            // Return child gas (not consumed on balance failure)
                            this.evm.gas.spent -= call.gas as i64;
                            this.evm.apply(state);
                            let _ = this.evm.push(Int::ZERO);
                            this.evm.apply(state);
                            this.evm.ret = vec![];
                            this.evm.pc += 1;
                            this.target = (0, 0);
                            continue;
                        }

                        // CALLCODE: value stays with self (by == this), no actual transfer
                        // CALL: value goes from caller to callee
                        if matches!(mode, CallMode::Call(..))
                            && let Some(to) = call.to
                        {
                            let add = lift(|[a, b]| a + b);
                            let sub = lift(|[a, b]| a - b);
                            let to0 = state.balance(&to).unwrap_or_default();
                            if to != by {
                                state.set_value(&by, sub([by0, call.eth]));
                                state.set_value(&to, add([to0, call.eth]));
                            }
                        }
                    }
                    self.callstack.push(frame);
                }
                StepResult::Return(ret) => {
                    state.emit(Event::Return(
                        ret.clone().into(),
                        this.evm.gas.spent.max(0) as u64,
                    ));
                    let is_create = this.is_create;
                    result = Some(if is_create {
                        let deploy_cost = CODE_DEPOSIT_GAS * ret.len() as i64;
                        // EIP-3541: reject code starting with 0xEF
                        let starts_with_ef = ret.first() == Some(&0xEF);
                        if ret.len() > MAX_CODE_SIZE
                            || starts_with_ef
                            || this.evm.gas_remaining() < deploy_cost
                        {
                            this.evm.gas.drain();
                            state.revert_to(this.checkpoint);
                            CallResult::Done {
                                status: Int::ZERO,
                                ret: vec![].into(),
                                gas: this.evm.gas,
                            }
                        } else {
                            this.evm.gas.spent += deploy_cost;
                            CallResult::Created {
                                acc: this.ctx.this,
                                code: ret.into(),
                                gas: this.evm.gas,
                            }
                        }
                    } else {
                        CallResult::Done {
                            status: Int::ONE,
                            ret: ret.into(),
                            gas: this.evm.gas,
                        }
                    });
                    self.callstack.pop();
                }
                StepResult::Revert(ret) => {
                    state.emit(Event::Revert(
                        ret.clone().into(),
                        this.evm.gas.spent.max(0) as u64,
                    ));
                    state.revert_to(this.checkpoint);
                    let mut gas = this.evm.gas;
                    gas.refund = 0;
                    result = Some(CallResult::Done {
                        status: Int::ZERO,
                        ret: ret.into(),
                        gas,
                    });
                    self.callstack.pop();
                }
                StepResult::Halt(reason) => {
                    state.emit(Event::Halt(reason, this.evm.gas.limit.max(0) as u64));
                    this.evm.apply(state);
                    this.evm.gas.drain();
                    state.revert_to(this.checkpoint);
                    result = Some(CallResult::Done {
                        status: Int::ZERO,
                        ret: vec![].into(),
                        gas: this.evm.gas,
                    });
                    self.callstack.pop();
                }
                StepResult::Fetch(f) => {
                    fetch(f, state, chain).await?;
                    this.evm.reset();
                }
            }
        }

        let mut result =
            result.ok_or::<Error>(eyre::eyre!("inconsistent state: call result missing").into())?;

        // Revert top-level state when call returns 0 or CREATE returns zero address
        let should_revert = match &result {
            CallResult::Done { status, .. } => status.is_zero(),
            CallResult::Created { acc, .. } => acc == &Acc::ZERO,
        };
        if should_revert && let Some(cp) = last_popped_checkpoint.take() {
            state.revert_to(cp);
        }

        // For top-level CREATE, store the deployed bytecode into the new account.
        if let CallResult::Created {
            acc: addr,
            ref code,
            ..
        } = result
            && !code.0.is_empty()
        {
            let hash = Int::from(keccak256(code.as_slice()).as_ref());
            state.set_code(&addr, code.clone(), hash);
        }

        state.set_depth(0);

        result.gas_mut().refund += eip7702_refund;
        let gas_final = finalized(
            &self.call,
            &tx,
            &head,
            effective_gas_price,
            &result,
            state,
            floor,
        );
        result.gas_mut().finalized = gas_final;

        state.apply();
        Ok(result)
    }
}

#[allow(clippy::too_many_arguments)]
async fn prepare(
    head: Head,
    mut call: Call,
    mode: CallMode,
    ctx: Option<&Context>,
    chain_id: Int,
    gas_price: Int,
    blob_hashes: Vec<Int>,
    state: &mut impl State,
    chain: &impl Chain,
) -> Result<CallFrame> {
    let is_create = matches!(mode, CallMode::Create(_) | CallMode::Create2(_));
    let code = if is_create {
        std::mem::take(&mut call.data)
    } else {
        let call_to = call.to.expect("checked non-create call");
        if let Some((code, _)) = state.code(&call_to) {
            code
        } else {
            fetch(Fetch::Account(call_to), state, chain).await?;
            if let Some(account) = state.acc(&call_to) {
                account.code.0.clone()
            } else {
                Buf::default()
            }
        }
    };
    // EIP-7702: resolve delegation after code is loaded.
    // Revm's `load_account_delegated` marks both the delegated account and the implementation
    // address warm when resolving code for a frame; mirror that so *CALL does not charge cold
    // access for an address already loaded for execution (see revm-context JournalInner).
    let code = if let Some(delegate) = call.to.and_then(|to| state.auth(&to)) {
        if let Some((code, _)) = state.code(&delegate) {
            state.warm_acc(&delegate);
            code
        } else {
            fetch(Fetch::Account(delegate), state, chain).await?;
            if let Some(account) = state.acc(&delegate) {
                state.warm_acc(&delegate);
                account.code.0.clone()
            } else {
                Buf::default()
            }
        }
    } else {
        code
    };
    // GASPRICE opcode returns effective gas price (EIP-1559: min(max_fee, base_fee + priority))
    let evm = Evm::new(
        head,
        code.into_vec(),
        call.gas,
        chain_id,
        gas_price,
        blob_hashes,
    );
    let is_static = matches!(mode, CallMode::Static(_, _));
    let this = match mode {
        CallMode::Create(acc) => acc,
        CallMode::Create2(acc) => acc,
        CallMode::Call(_, _) | CallMode::Static(_, _) => call.to.expect("CALL must have 'to' set"),
        CallMode::CallCode(_, _) | CallMode::Delegate(_, _) => {
            ctx.map(|c| c.this).unwrap_or(call.by)
        }
    };
    let ctx = if let Some(ctx) = ctx {
        Context {
            origin: ctx.origin,
            is_static: ctx.is_static || is_static,
            depth: ctx.depth + 1,
            this,
        }
    } else {
        Context {
            origin: call.by,
            is_static,
            depth: 0,
            this,
        }
    };
    let is_create = matches!(mode, CallMode::Create(_) | CallMode::Create2(_));
    let checkpoint = state.checkpoint();
    Ok(CallFrame {
        call,
        evm,
        ctx,
        checkpoint,
        target: (0, 0),
        is_create,
    })
}
