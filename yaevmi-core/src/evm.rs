use crate::call::Head;
use crate::trace::{Event, Step};
use serde::{Deserialize, Serialize};
use yaevmi_base::math::lift;

use crate::{Acc, Call, Int, Result, ops, state::State};

const K: usize = 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum HaltReason {
    OutOfGas,
    OutOfMemory,
    BadCopyRange,
    BadJump(usize),
    BadOpcode(u8),
    NonStatic,
    StackUnderflow,
    StackOverflow,
    GasBelowStipend,
}

#[derive(Debug)]
pub enum Fetch {
    Code(Acc),
    Nonce(Acc),
    Balance(Acc),
    Account(Acc),
    BlockHash(u64),
    StateCell(Acc, Int),
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub enum CallMode {
    Call(usize, usize),
    Static(usize, usize),
    Delegate(usize, usize),
    CallCode(usize, usize),
    Create(Acc),
    Create2(Acc),
}

impl CallMode {
    pub fn target(&self) -> Option<(usize, usize)> {
        match self {
            Self::Call(offset, size) => Some((*offset, *size)),
            Self::Static(offset, size) => Some((*offset, *size)),
            Self::Delegate(offset, size) => Some((*offset, *size)),
            Self::CallCode(offset, size) => Some((*offset, *size)),
            _ => None,
        }
    }

    pub fn created(&self) -> Option<Acc> {
        match self {
            Self::Create(acc) => Some(*acc),
            Self::Create2(acc) => Some(*acc),
            _ => None,
        }
    }
}

pub enum StepResult {
    End,
    Ok,
    Call(Call, CallMode),
    Return(Vec<u8>),
    Revert(Vec<u8>),
    Halt(HaltReason),
    Fetch(Fetch),
}

impl From<HaltReason> for StepResult {
    fn from(reason: HaltReason) -> Self {
        StepResult::Halt(reason)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Gas {
    pub limit: i64,
    pub spent: i64,
    pub refund: i64,
    pub finalized: i64,
}

impl Gas {
    pub fn new(gas: u64) -> Self {
        Self {
            limit: gas as i64,
            spent: 0,
            refund: 0,
            finalized: 0,
        }
    }

    pub fn remaining(&self) -> i64 {
        self.limit - self.spent //+ self.refund
    }

    pub fn refund(&mut self, gas: i64) -> EvmResult<()> {
        if self.refund + gas >= 0 {
            self.refund += gas;
            Ok(())
        } else {
            Err(EvmYield::Halt(HaltReason::OutOfGas))
        }
    }

    pub fn charge(&mut self, gas: i64) -> EvmResult<i64> {
        let rem = self.remaining();
        if rem >= gas {
            self.spent += gas;
            Ok(rem - gas)
        } else {
            self.spent += rem;
            Err(EvmYield::Halt(HaltReason::OutOfGas))
        }
    }

    pub fn drain(&mut self) {
        self.spent = self.limit;
        self.refund = 0;
    }
}

pub struct Context {
    pub origin: Acc,
    pub is_static: bool,
    pub depth: usize,
    pub this: Acc,
}

#[derive(Debug)]
pub enum EvmYield {
    Halt(HaltReason),
    Fetch(Fetch),
    Return(Vec<u8>),
    Revert(Vec<u8>),
    Call(Call, CallMode),
}

pub type EvmResult<T> = std::result::Result<T, EvmYield>;

pub struct Evm {
    pub pc: usize,
    pub gas: Gas,
    pub stack: Vec<Int>,
    pub memory: Vec<u8>,
    pub code: Vec<u8>,
    pub head: Head,
    pub ret: Vec<u8>,

    pub chain_id: Int,
    pub gas_price: Int,
    pub blob_hashes: Vec<Int>,
    pub(crate) pending_stack_pops: usize,
    pub(crate) pending_stack_push: Vec<Int>,
    pub(crate) pending_gas_charge: i64,
    pub(crate) pending_gas_refund: i64,
    pub(crate) pending_acc_warmup: [Acc; 2],
    pub(crate) pending_acc_count: usize,
    pub(crate) pending_key_warmup: Option<(Acc, Int)>,

    pub(crate) step: Option<Step>,
}

impl Evm {
    pub const STACK_SIZE_LIMIT: usize = 1024;
    // Max memory size in bytes: (2^32 - 1) = 4Gb.
    // Sanity check: (16 * 2^20) = 16Mb in practice.
    pub const MEMORY_SIZE_LIMIT: usize = (1_usize << 24);

    pub fn new(
        head: Head,
        code: Vec<u8>,
        gas: u64,
        chain_id: Int,
        gas_price: Int,
        blob_hashes: Vec<Int>,
    ) -> Self {
        Self {
            pc: 0,
            gas: Gas::new(gas),
            stack: Vec::with_capacity(Self::STACK_SIZE_LIMIT),
            memory: Vec::with_capacity(4 * K),
            code,
            head,
            ret: Vec::new(),
            chain_id,
            gas_price,
            blob_hashes,
            pending_stack_pops: 0,
            pending_stack_push: Vec::new(),
            pending_gas_charge: 0,
            pending_gas_refund: 0,
            pending_acc_warmup: [Acc::ZERO; 2],
            pending_acc_count: 0,
            pending_key_warmup: None,
            step: None,
        }
    }

    pub fn peek_usize<const N: usize>(&mut self) -> EvmResult<[usize; N]> {
        let mut ret = [0usize; N];
        let pop = self.peek::<N>()?;
        for (i, item) in ret.iter_mut().enumerate() {
            *item = pop[i].as_usize();
        }
        Ok(ret)
    }

    pub fn peek<const N: usize>(&mut self) -> EvmResult<[Int; N]> {
        let mut ret = [Int::ZERO; N];
        if self.stack.len() < N {
            return Err(EvmYield::Halt(HaltReason::StackUnderflow));
        }
        for (slot, value) in ret.iter_mut().zip(self.stack.iter().rev()) {
            *slot = *value;
        }
        self.pending_stack_pops = N;
        Ok(ret)
    }

    pub fn apply(&mut self, state: &mut impl State) {
        for _ in 0..self.pending_stack_pops {
            let _ = self.stack.pop();
        }
        self.pending_stack_pops = 0;

        for int in self.pending_stack_push.drain(..) {
            self.stack.push(int);
        }
        assert!(self.pending_stack_push.is_empty());

        self.gas.spent += self.pending_gas_charge;
        self.pending_gas_charge = 0;

        self.gas.refund += self.pending_gas_refund;
        self.pending_gas_refund = 0;

        for i in 0..self.pending_acc_count {
            state.warm_acc(&self.pending_acc_warmup[i]);
        }
        self.pending_acc_count = 0;

        if let Some((acc, key)) = self.pending_key_warmup.take() {
            state.warm_key(&acc, &key);
        }
    }

    pub fn reset(&mut self) {
        self.pending_stack_pops = 0;
        self.pending_stack_push.clear();
        self.pending_gas_charge = 0;
        self.pending_gas_refund = 0;
        self.pending_acc_count = 0;
        self.pending_key_warmup = None;
    }

    pub fn push(&mut self, int: Int) -> EvmResult<()> {
        let effective = self
            .stack
            .len()
            .saturating_sub(self.pending_stack_pops)
            .saturating_add(self.pending_stack_push.len());
        if effective >= Self::STACK_SIZE_LIMIT {
            return Err(EvmYield::Halt(HaltReason::StackOverflow));
        }
        self.pending_stack_push.push(int);
        Ok(())
    }

    pub fn warm_acc(&mut self, acc: &Acc) {
        self.pending_acc_warmup[self.pending_acc_count] = *acc;
        self.pending_acc_count += 1;
    }

    pub fn warm_key(&mut self, acc: &Acc, key: &Int) {
        self.pending_key_warmup = Some((*acc, *key));
    }

    pub fn gas_remaining(&self) -> i64 {
        self.gas.remaining() - self.pending_gas_charge
    }

    pub fn gas_charge(&mut self, gas: i64) -> EvmResult<()> {
        if gas > self.gas_remaining() {
            self.pending_gas_charge += self.gas.remaining();
            return Err(EvmYield::Halt(HaltReason::OutOfGas));
        }
        self.pending_gas_charge += gas;
        Ok(())
    }

    pub fn gas_refund(&mut self, gas: i64) -> EvmResult<()> {
        self.pending_gas_refund += gas;
        Ok(())
    }

    pub fn mem_expand(&mut self, offset: usize, size: usize) -> EvmResult<()> {
        if size == 0 {
            return Ok(());
        }
        mem_check(offset, size)?;
        let len = self.memory.len();
        let end = (offset + size).div_ceil(32) * 32;
        if end > len {
            let old_words = (len / 32) as i64;
            let new_words = (end / 32) as i64;
            let cost = (new_words * new_words / 512 + 3 * new_words)
                - (old_words * old_words / 512 + 3 * old_words);
            self.gas_charge(cost)?;
            self.memory.resize(end, 0);
        }
        Ok(())
    }

    /// Expand memory to cover all given regions and charge gas once for the combined expansion.
    /// Per EVM spec, CALL/DELEGATECALL/etc. charge for expansion to max(args, ret) in one step.
    pub fn mem_expand_max(&mut self, regions: &[(usize, usize)]) -> EvmResult<()> {
        let mut max_end = self.memory.len();
        for (offset, size) in regions {
            if *size > 0 {
                mem_check(*offset, *size)?;
                let end = (offset + size).div_ceil(32) * 32;
                max_end = max_end.max(end);
            }
        }
        let len = self.memory.len();
        if max_end > len {
            let old_words = (len / 32) as i64;
            let new_words = (max_end / 32) as i64;
            let cost = (new_words * new_words / 512 + 3 * new_words)
                - (old_words * old_words / 512 + 3 * old_words);
            self.gas_charge(cost)?;
            self.memory.resize(max_end, 0);
        }
        Ok(())
    }

    pub fn mem_put(&mut self, offset: usize, size: usize, source: &[u8]) -> EvmResult<()> {
        self.mem_expand(offset, size)?;
        if size > 0 && !source.is_empty() {
            let len = source.len().min(size);
            self.memory[offset..offset + len].copy_from_slice(&source[..len]);
        }
        Ok(())
    }

    pub fn mem_get(&mut self, offset: usize, size: usize) -> EvmResult<Vec<u8>> {
        self.mem_expand(offset, size)?;
        let lo = offset.min(self.memory.len());
        let hi = (offset + size).min(self.memory.len());
        let mut ret = vec![0u8; size];
        ret[..hi - lo].copy_from_slice(&self.memory[lo..hi]);
        Ok(ret)
    }

    pub fn data(&self, pc: usize) -> Vec<u8> {
        let op = self.code[pc];
        let len = match op {
            0x60..0x80 => op as usize - 0x60 + 1, // PUSH{1..32}
            _ => 0,
        };
        let lo = (pc + 1).min(self.code.len());
        let hi = (pc + 1 + len).min(self.code.len());
        let mut ret = vec![0; len];
        let len = hi - lo;
        ret[0..len].copy_from_slice(&self.code[lo..hi]);
        ret
    }

    pub fn step(
        &mut self,
        ctx: &Context,
        call: &Call,
        state: &mut impl State,
    ) -> Result<StepResult> {
        let Some(op) = self.code.get(self.pc).copied() else {
            return Ok(StepResult::End);
        };
        let name = ops::OPS[op as usize];

        if state.is_tracing() {
            let pc = self.pc;
            let name = if name.starts_with("INVALID/") {
                "INVALID".to_string()
            } else {
                name.to_string()
            };
            let data = self.data(pc);
            let data = if data.is_empty() {
                None
            } else {
                Some(data.into())
            };
            let gas = self.gas.remaining().max(0) as u64;
            self.step = Some(Step {
                pc,
                op,
                name,
                data,
                gas,
                stack: self.stack.len(),
                memory: self.memory.len(),
                debug: vec![],
            });
        }

        // let balance = state.balance(&ctx.this).unwrap_or_default();
        // if let Some(step) = self.step.as_mut() {
        //     step.debug
        //         .push(format!("balance[{:?}]={:?}", ctx.this, balance.as_u128()));
        // };

        match ops::dispatch(op, self, ctx, call, state) {
            Ok(()) => {
                self.apply(state);
                if !is_jump(op) {
                    self.pc += 1;
                }
                if let Some(mut step) = self.step.take() {
                    let cost = step.gas - self.gas.remaining().max(0) as u64;
                    step.gas = self.gas.remaining().max(0) as u64;
                    step.stack = self.stack.len();
                    step.memory = self.memory.len();
                    step.debug.push(format!("cost={cost}"));
                    if self.gas.refund != 0 {
                        step.debug.push(format!("gas_refund={}", self.gas.refund));
                    }
                    state.emit(Event::Step(step));
                }
                Ok(StepResult::Ok)
            }
            Err(EvmYield::Fetch(fetch)) => {
                self.reset();
                Ok(StepResult::Fetch(fetch))
            }
            Err(EvmYield::Halt(reason)) => {
                self.apply(state);
                if let Some(mut step) = self.step.take() {
                    step.gas = self.gas.remaining().max(0) as u64;
                    step.stack = self.stack.len();
                    step.memory = self.memory.len();
                    step.debug.push(format!("HALT:{:?}", reason));
                    state.emit(Event::Step(step));
                }
                Ok(StepResult::Halt(reason))
            }
            Err(EvmYield::Return(ret)) => {
                self.apply(state);
                if let Some(mut step) = self.step.take() {
                    step.gas = self.gas.remaining().max(0) as u64;
                    step.stack = self.stack.len();
                    step.memory = self.memory.len();
                    step.debug.push(format!("RETURN:size={}", ret.len()));
                    state.emit(Event::Step(step));
                }
                Ok(StepResult::Return(ret))
            }
            Err(EvmYield::Revert(ret)) => {
                self.apply(state);
                if let Some(mut step) = self.step.take() {
                    step.gas = self.gas.remaining().max(0) as u64;
                    step.stack = self.stack.len();
                    step.memory = self.memory.len();
                    step.debug.push(format!("REVERT:size={}", ret.len()));
                    state.emit(Event::Step(step));
                }
                Ok(StepResult::Revert(ret))
            }
            Err(EvmYield::Call(call, mode)) => {
                self.apply(state);
                if let Some(mut step) = self.step.take() {
                    step.gas = self.gas.remaining().max(0) as u64;
                    step.stack = self.stack.len();
                    step.memory = self.memory.len();
                    step.debug.push(format!("CALL:to={},gas={}", call.to, call.gas));
                    if !call.eth.is_zero() {
                        step.debug.push(format!("CALL:eth={}", call.eth));
                    }
                    step.debug.push(format!("CALL:mode={mode:?}"));
                    state.emit(Event::Step(step));
                }
                Ok(StepResult::Call(call, mode))
            }
        }
    }
}

/// Check memory range using 256-bit values (avoids truncation before check).
pub fn mem_check_int(offset: Int, size: Int) -> EvmResult<()> {
    let limit = Int::from(Evm::MEMORY_SIZE_LIMIT);
    let add = lift(|[a, b]| a + b);
    let gt = lift(|[a, b]| {
        if a > b {
            yaevmi_base::math::U256::ONE
        } else {
            yaevmi_base::math::U256::ZERO
        }
    });
    if !gt([size, limit]).is_zero() {
        return Err(EvmYield::Halt(HaltReason::OutOfMemory));
    }
    let end = add([offset, size]);
    if !gt([end, limit]).is_zero() {
        return Err(EvmYield::Halt(HaltReason::OutOfMemory));
    }
    Ok(())
}

pub fn mem_check(offset: usize, size: usize) -> EvmResult<()> {
    let limit = Evm::MEMORY_SIZE_LIMIT;
    if size <= limit && offset <= limit.saturating_sub(size) {
        return Ok(());
    }
    Err(EvmYield::Halt(HaltReason::OutOfMemory))
}

fn is_jump(op: u8) -> bool {
    op == 0x56 || op == 0x57
}
