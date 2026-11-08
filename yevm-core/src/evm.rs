use std::sync::Arc;

use crate::call::Head;
use crate::trace::{Event, Step};
use serde::{Deserialize, Serialize};
use yevm_base::math::{U256, lift};
use yevm_misc::keccak256;

use crate::{Acc, Call, Int, Result, ops, state::State};

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
    /// The transaction's sender cannot pay its value (only when the host skips the balance
    /// check, as `eth_call` does); the transaction fails without executing.
    OutOfFunds,
    /// A top-level create targets an address that already has a nonce or code.
    CreateCollision,
    /// A create returned code larger than the EIP-170 limit.
    CodeSizeLimit,
    /// A create returned code starting with `0xEF` (EIP-3541).
    InvalidCodePrefix,
    /// A precompile called by the transaction failed.
    PrecompileFailure,
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

/// Result of [`Evm::fast_run`].
enum Fast {
    /// An instruction halted the frame.
    Halt(HaltReason),
    /// The step limit was reached.
    Limit,
    /// The next instruction must go through [`Evm::step`].
    Generic,
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
    /// The stack as native words; instructions read and push [`Int`]s through [`Self::peek`]
    /// and [`Self::push`], see [`word`] and [`int`].
    pub stack: Vec<U256>,
    pub memory: Vec<u8>,
    /// The frame's code, shared with other frames running the same code.
    pub code: Arc<[u8]>,
    /// The `JUMPDEST`s of `code`, analysed on the first jump unless set by the executor.
    jumpdests: Option<Arc<JumpTable>>,
    /// The block header, shared by every frame of the transaction.
    pub head: Arc<Head>,
    pub ret: Vec<u8>,

    pub chain_id: Int,
    pub gas_price: Int,
    pub blob_hashes: Vec<Int>,
    pub(crate) pending_stack_pops: usize,
    /// Values pushed by the instruction being executed; no instruction pushes more than one.
    pub(crate) pending_stack_push: [U256; 1],
    pub(crate) pending_stack_push_len: usize,
    pub(crate) pending_gas_charge: i64,
    pub(crate) pending_gas_refund: i64,
    pub(crate) pending_acc_warmup: [Acc; 2],
    pub(crate) pending_acc_count: usize,

    pub(crate) step: Option<Step>,
}

impl Evm {
    pub const STACK_SIZE_LIMIT: usize = 1024;
    // Max memory size in bytes: (2^32 - 1) = 4Gb.
    // Sanity check: (16 * 2^20) = 16Mb in practice.
    pub const MEMORY_SIZE_LIMIT: usize = (1_usize << 24);

    pub fn new(
        head: impl Into<Arc<Head>>,
        code: impl Into<Arc<[u8]>>,
        gas: u64,
        chain_id: Int,
        gas_price: Int,
        blob_hashes: Vec<Int>,
    ) -> Self {
        let code: Arc<[u8]> = code.into();
        Self {
            pc: 0,
            gas: Gas::new(gas),
            // Frames without code need no stack; others grow theirs on demand.
            stack: if code.is_empty() {
                Vec::new()
            } else {
                Vec::with_capacity(64)
            },
            memory: Vec::new(),
            jumpdests: None,
            code,
            head: head.into(),
            ret: Vec::new(),
            chain_id,
            gas_price,
            blob_hashes,
            pending_stack_pops: 0,
            pending_stack_push: [U256::ZERO; 1],
            pending_stack_push_len: 0,
            pending_gas_charge: 0,
            pending_gas_refund: 0,
            pending_acc_warmup: [Acc::ZERO; 2],
            pending_acc_count: 0,
            step: None,
        }
    }

    /// Returns whether `acc` is cold: neither warm in `state` nor warmed by the instruction being
    /// executed (whose warm-ups reach `state` only once it completes).
    pub fn is_cold_acc(&self, state: &(impl State + ?Sized), acc: &Acc) -> bool {
        state.is_cold_acc(acc) && !self.pending_acc_warmup[..self.pending_acc_count].contains(acc)
    }

    /// Uses `jumpdests`, the analysis of this frame's code, instead of analysing it again.
    pub fn set_jump_table(&mut self, jumpdests: Arc<JumpTable>) {
        self.jumpdests = Some(jumpdests);
    }

    /// Returns whether a jump to `pc` lands on a `JUMPDEST` instruction.
    pub fn is_jumpdest(&mut self, pc: usize) -> bool {
        let code = &self.code;
        self.jumpdests
            .get_or_insert_with(|| Arc::new(JumpTable::new(code)))
            .contains(pc)
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
            *slot = int(*value);
        }
        self.pending_stack_pops = N;
        Ok(ret)
    }

    #[inline]
    pub fn apply(&mut self, state: &mut impl State) {
        // `peek` checked that the stack holds the popped values.
        let len = self.stack.len().saturating_sub(self.pending_stack_pops);
        self.stack.truncate(len);
        self.pending_stack_pops = 0;

        self.stack
            .extend_from_slice(&self.pending_stack_push[..self.pending_stack_push_len]);
        self.pending_stack_push_len = 0;

        self.gas.spent += self.pending_gas_charge;
        self.pending_gas_charge = 0;

        self.gas.refund += self.pending_gas_refund;
        self.pending_gas_refund = 0;

        for i in 0..self.pending_acc_count {
            state.warm_acc(&self.pending_acc_warmup[i]);
        }
        self.pending_acc_count = 0;
    }

    pub fn reset(&mut self) {
        self.pending_stack_pops = 0;
        self.pending_stack_push_len = 0;
        self.pending_gas_charge = 0;
        self.pending_gas_refund = 0;
        self.pending_acc_count = 0;
    }

    #[inline]
    pub fn push(&mut self, int: Int) -> EvmResult<()> {
        let effective = self
            .stack
            .len()
            .saturating_sub(self.pending_stack_pops)
            .saturating_add(self.pending_stack_push_len);
        if effective >= Self::STACK_SIZE_LIMIT {
            return Err(EvmYield::Halt(HaltReason::StackOverflow));
        }
        // Indexing enforces that an instruction pushes at most one value.
        self.pending_stack_push[self.pending_stack_push_len] = word(int);
        self.pending_stack_push_len += 1;
        Ok(())
    }

    pub fn warm_acc(&mut self, acc: &Acc) {
        self.pending_acc_warmup[self.pending_acc_count] = *acc;
        self.pending_acc_count += 1;
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

    /// Executes instructions until one needs the executor (a call, a return, a revert, a halt or
    /// a fetch) or `limit` instructions ran, and returns its result (`StepResult::Ok` when the
    /// limit was reached) with the number of instructions executed.
    pub fn run(
        &mut self,
        ctx: &Context,
        call: &Call,
        state: &mut impl State,
        limit: u64,
    ) -> Result<(StepResult, u64)> {
        // The executor charges some costs (a transaction's intrinsic gas) through the pending
        // effects; settle them, since the fast path reads the gas and stack directly.
        self.apply(state);
        // Step and trace events need the generic path for every instruction.
        let fast = !state.records_trace();
        let mut steps = 0;
        loop {
            if fast {
                match self.fast_run(ctx, call, limit, &mut steps) {
                    Fast::Halt(reason) => return Ok((StepResult::Halt(reason), steps)),
                    Fast::Limit => return Ok((StepResult::Ok, steps)),
                    Fast::Generic => {}
                }
            }
            if steps >= limit {
                return Ok((StepResult::Ok, steps));
            }
            steps += 1;
            match self.step(ctx, call, state)? {
                StepResult::Ok => {}
                result => return Ok((result, steps)),
            }
        }
    }

    /// Executes the instructions starting at `pc` directly on the stack for as long as they are
    /// stack, arithmetic, comparison, bitwise, memory, call-context, hashing or jump
    /// instructions, which never read state, so they need none of the deferred effects that let
    /// [`Self::step`] retry an instruction after a fetch. Mirrors the instructions in [`ops`]
    /// exactly. Counts executed instructions in `steps`, up to `limit`.
    ///
    /// Returns [`Fast::Generic`] at the first instruction it does not execute, with `pc` at it.
    #[inline(always)]
    fn fast_run(&mut self, ctx: &Context, call: &Call, limit: u64, steps: &mut u64) -> Fast {
        // Disjoint field borrows: the loop below never calls methods on `self`.
        let code: &[u8] = &self.code;
        let gas_limit = self.gas.limit;
        let mut spent = self.gas.spent;
        let mut pc = self.pc;
        let stack = &mut self.stack;
        let memory = &mut self.memory;
        let jumpdests = &mut self.jumpdests;

        macro_rules! charge {
            ($gas:expr) => {{
                let gas: i64 = $gas;
                if gas_limit - spent < gas {
                    break Fast::Halt(HaltReason::OutOfGas);
                }
                spent += gas;
            }};
        }
        macro_rules! operands {
            ($n:expr) => {{
                let len = stack.len();
                if len < $n {
                    break Fast::Halt(HaltReason::StackUnderflow);
                }
                len
            }};
        }
        // Replaces the top two words with `f(top, second)`.
        macro_rules! binary {
            ($gas:expr, |$a:ident, $b:ident| $result:expr) => {{
                charge!($gas);
                let len = operands!(2);
                let $a = stack[len - 1];
                let $b = stack[len - 2];
                stack[len - 2] = $result;
                stack.truncate(len - 1);
                pc += 1;
            }};
        }
        // Replaces the top word with `f(top)`.
        macro_rules! unary {
            ($gas:expr, |$x:ident| $result:expr) => {{
                charge!($gas);
                let len = operands!(1);
                let $x = stack[len - 1];
                stack[len - 1] = $result;
                pc += 1;
            }};
        }
        macro_rules! push {
            ($value:expr) => {{
                if stack.len() >= Self::STACK_SIZE_LIMIT {
                    break Fast::Halt(HaltReason::StackOverflow);
                }
                stack.push($value);
            }};
        }
        // Expands memory to cover `size` bytes at `offset` (checked by `memory_range`),
        // charging like `Evm::mem_expand`.
        macro_rules! expand {
            ($offset:expr, $size:expr) => {{
                let end = ($offset + $size).div_ceil(32) * 32;
                let len = memory.len();
                if end > len {
                    let (old, new) = ((len / 32) as i64, (end / 32) as i64);
                    charge!((new * new / 512 + 3 * new) - (old * old / 512 + 3 * old));
                    memory.resize(end, 0);
                }
            }};
        }
        // Resolves a jump destination, or halts with the destination `BadJump` reports.
        macro_rules! jump_target {
            ($dst:expr) => {{
                let dst = usize::try_from($dst).unwrap_or(usize::MAX);
                let table = jumpdests.get_or_insert_with(|| Arc::new(JumpTable::new(code)));
                if dst == usize::MAX || !table.contains(dst) {
                    break Fast::Halt(HaltReason::BadJump(dst));
                }
                dst
            }};
        }
        macro_rules! memory_range {
            ($offset:expr, $size:expr) => {{
                match memory_offset($offset, $size) {
                    Some(offset) => offset,
                    None => break Fast::Halt(HaltReason::OutOfMemory),
                }
            }};
        }
        // PUSHn: the next `n` code bytes, zero-padded past the end of the code.
        macro_rules! push_n {
            ($n:literal) => {{
                charge!(3);
                const N: usize = $n;
                let lo = pc + 1;
                let value = if let Some(data) = code.get(lo..lo + N) {
                    if N <= 8 {
                        U256::from(
                            data.iter()
                                .fold(0u64, |word, &byte| (word << 8) | u64::from(byte)),
                        )
                    } else {
                        let mut bytes = [0u8; 32];
                        bytes[32 - N..].copy_from_slice(data);
                        U256::from_be_bytes(bytes)
                    }
                } else {
                    let data = code.get(lo..).unwrap_or_default();
                    let mut bytes = [0u8; 32];
                    bytes[32 - N..32 - N + data.len()].copy_from_slice(data);
                    U256::from_be_bytes(bytes)
                };
                push!(value);
                pc += N + 1;
            }};
        }
        macro_rules! dup_n {
            ($n:literal) => {{
                charge!(3);
                let len = operands!($n);
                push!(stack[len - $n]);
                pc += 1;
            }};
        }
        macro_rules! swap_n {
            ($n:literal) => {{
                charge!(3);
                let len = operands!($n + 1);
                stack.swap(len - 1, len - 1 - $n);
                pc += 1;
            }};
        }
        let flag = |b: bool| U256::from(b as u8);
        // Signed order is unsigned order with the sign bit flipped.
        let signed = |x: U256| x ^ (U256::ONE << 255);

        let result = loop {
            if *steps >= limit {
                break Fast::Limit;
            }
            let Some(&op) = code.get(pc) else {
                break Fast::Generic;
            };
            *steps += 1;
            match op {
                0x01 => binary!(3, |a, b| a.wrapping_add(b)),
                0x02 => binary!(5, |a, b| a.wrapping_mul(b)),
                0x03 => binary!(3, |a, b| a.wrapping_sub(b)),
                0x04 => binary!(5, |a, b| if b.is_zero() { U256::ZERO } else { a / b }),
                0x06 => binary!(5, |a, b| if b.is_zero() { U256::ZERO } else { a % b }),
                0x0B => binary!(5, |b, x| {
                    if b >= U256::from(32u32) {
                        x
                    } else {
                        let b = b.saturating_to::<usize>();
                        let mask = if b == 31 {
                            U256::MAX
                        } else {
                            (U256::ONE << (b * 8 + 8)) - U256::ONE
                        };
                        if x.bit(b * 8 + 7) {
                            x | !mask
                        } else {
                            x & mask
                        }
                    }
                }),
                0x10 => binary!(3, |a, b| flag(a < b)),
                0x11 => binary!(3, |a, b| flag(a > b)),
                0x12 => binary!(3, |a, b| flag(signed(a) < signed(b))),
                0x13 => binary!(3, |a, b| flag(signed(a) > signed(b))),
                0x14 => binary!(3, |a, b| flag(a == b)),
                0x15 => unary!(3, |x| flag(x.is_zero())),
                0x16 => binary!(3, |a, b| a & b),
                0x17 => binary!(3, |a, b| a | b),
                0x18 => binary!(3, |a, b| a ^ b),
                0x19 => unary!(3, |x| !x),
                0x1A => binary!(3, |index, value| match usize::try_from(index) {
                    Ok(index) if index < 32 => U256::from(value.byte(31 - index)),
                    _ => U256::ZERO,
                }),
                0x1B => binary!(3, |shift, value| value << shift),
                0x1C => binary!(3, |shift, value| value >> shift),
                0x1D => binary!(3, |shift, value| value
                    .arithmetic_shr(shift.saturating_to::<usize>())),
                // KECCAK256
                0x20 => {
                    charge!(30);
                    let len = operands!(2);
                    let (offset, size) = (stack[len - 1], stack[len - 2]);
                    let Some(size) = usize::try_from(size)
                        .ok()
                        .filter(|&size| size <= Self::MEMORY_SIZE_LIMIT)
                    else {
                        break Fast::Halt(HaltReason::OutOfMemory);
                    };
                    let offset = if size == 0 {
                        0
                    } else {
                        memory_range!(offset, size)
                    };
                    charge!(6 * size.div_ceil(32) as i64);
                    let hash = if size == 0 {
                        keccak256(&[])
                    } else {
                        expand!(offset, size);
                        keccak256(&memory[offset..offset + size])
                    };
                    stack[len - 2] = word(hash);
                    stack.truncate(len - 1);
                    pc += 1;
                }
                // ADDRESS
                0x30 => {
                    charge!(2);
                    push!(word(ctx.this.to()));
                    pc += 1;
                }
                // CALLER
                0x33 => {
                    charge!(2);
                    push!(word(call.by.to()));
                    pc += 1;
                }
                // CALLVALUE
                0x34 => {
                    charge!(2);
                    push!(word(call.eth));
                    pc += 1;
                }
                // CALLDATALOAD, zero-padded past the end of the calldata
                0x35 => unary!(3, |offset| {
                    let data = &call.data.0;
                    let mut bytes = [0u8; 32];
                    if let Ok(offset) = usize::try_from(offset)
                        && offset < data.len()
                    {
                        let len = (data.len() - offset).min(32);
                        bytes[..len].copy_from_slice(&data[offset..offset + len]);
                    }
                    U256::from_be_bytes(bytes)
                }),
                // CALLDATASIZE
                0x36 => {
                    charge!(2);
                    push!(U256::from(call.data.0.len()));
                    pc += 1;
                }
                // POP
                0x50 => {
                    charge!(2);
                    let len = operands!(1);
                    stack.truncate(len - 1);
                    pc += 1;
                }
                // MLOAD
                0x51 => {
                    charge!(3);
                    let len = operands!(1);
                    let offset = memory_range!(stack[len - 1], 32);
                    expand!(offset, 32);
                    let mut bytes = [0u8; 32];
                    bytes.copy_from_slice(&memory[offset..offset + 32]);
                    stack[len - 1] = U256::from_be_bytes(bytes);
                    pc += 1;
                }
                // MSTORE
                0x52 => {
                    charge!(3);
                    let len = operands!(2);
                    let offset = memory_range!(stack[len - 1], 32);
                    expand!(offset, 32);
                    memory[offset..offset + 32]
                        .copy_from_slice(&stack[len - 2].to_be_bytes::<32>());
                    stack.truncate(len - 2);
                    pc += 1;
                }
                // MSTORE8
                0x53 => {
                    charge!(3);
                    let len = operands!(2);
                    let offset = memory_range!(stack[len - 1], 1);
                    expand!(offset, 1);
                    memory[offset] = stack[len - 2].byte(0);
                    stack.truncate(len - 2);
                    pc += 1;
                }
                // JUMP
                0x56 => {
                    charge!(8);
                    let len = operands!(1);
                    let dst = jump_target!(stack[len - 1]);
                    stack.truncate(len - 1);
                    pc = dst;
                }
                // JUMPI
                0x57 => {
                    charge!(10);
                    let len = operands!(2);
                    if stack[len - 2].is_zero() {
                        pc += 1;
                    } else {
                        pc = jump_target!(stack[len - 1]);
                    }
                    stack.truncate(len - 2);
                }
                // PC
                0x58 => {
                    charge!(2);
                    push!(U256::from(pc));
                    pc += 1;
                }
                // MSIZE
                0x59 => {
                    charge!(2);
                    push!(U256::from(memory.len()));
                    pc += 1;
                }
                // GAS
                0x5A => {
                    charge!(2);
                    push!(U256::from((gas_limit - spent).max(0) as u64));
                    pc += 1;
                }
                // JUMPDEST
                0x5B => {
                    charge!(1);
                    pc += 1;
                }
                // PUSH0
                0x5F => {
                    charge!(2);
                    push!(U256::ZERO);
                    pc += 1;
                }
                // PUSH1..PUSH32, DUP1..DUP16 and SWAP1..SWAP16 get an arm per opcode, so that
                // they share the match's jump table and their sizes are constants.
                0x60 => push_n!(1),
                0x61 => push_n!(2),
                0x62 => push_n!(3),
                0x63 => push_n!(4),
                0x64 => push_n!(5),
                0x65 => push_n!(6),
                0x66 => push_n!(7),
                0x67 => push_n!(8),
                0x68 => push_n!(9),
                0x69 => push_n!(10),
                0x6A => push_n!(11),
                0x6B => push_n!(12),
                0x6C => push_n!(13),
                0x6D => push_n!(14),
                0x6E => push_n!(15),
                0x6F => push_n!(16),
                0x70 => push_n!(17),
                0x71 => push_n!(18),
                0x72 => push_n!(19),
                0x73 => push_n!(20),
                0x74 => push_n!(21),
                0x75 => push_n!(22),
                0x76 => push_n!(23),
                0x77 => push_n!(24),
                0x78 => push_n!(25),
                0x79 => push_n!(26),
                0x7A => push_n!(27),
                0x7B => push_n!(28),
                0x7C => push_n!(29),
                0x7D => push_n!(30),
                0x7E => push_n!(31),
                0x7F => push_n!(32),
                0x80 => dup_n!(1),
                0x81 => dup_n!(2),
                0x82 => dup_n!(3),
                0x83 => dup_n!(4),
                0x84 => dup_n!(5),
                0x85 => dup_n!(6),
                0x86 => dup_n!(7),
                0x87 => dup_n!(8),
                0x88 => dup_n!(9),
                0x89 => dup_n!(10),
                0x8A => dup_n!(11),
                0x8B => dup_n!(12),
                0x8C => dup_n!(13),
                0x8D => dup_n!(14),
                0x8E => dup_n!(15),
                0x8F => dup_n!(16),
                0x90 => swap_n!(1),
                0x91 => swap_n!(2),
                0x92 => swap_n!(3),
                0x93 => swap_n!(4),
                0x94 => swap_n!(5),
                0x95 => swap_n!(6),
                0x96 => swap_n!(7),
                0x97 => swap_n!(8),
                0x98 => swap_n!(9),
                0x99 => swap_n!(10),
                0x9A => swap_n!(11),
                0x9B => swap_n!(12),
                0x9C => swap_n!(13),
                0x9D => swap_n!(14),
                0x9E => swap_n!(15),
                0x9F => swap_n!(16),
                _ => {
                    *steps -= 1;
                    break Fast::Generic;
                }
            }
        };
        self.pc = pc;
        self.gas.spent = spent;
        result
    }

    #[inline]
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
                    step.debug.push(format!(
                        "CALL:to={},gas={}",
                        call.to.unwrap_or_default(),
                        call.gas
                    ));
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

/// Returns `offset` if `size` bytes at it fit in [`Evm::MEMORY_SIZE_LIMIT`], like
/// [`mem_check_int`] for a nonzero `size`.
#[inline(always)]
fn memory_offset(offset: U256, size: usize) -> Option<usize> {
    usize::try_from(offset)
        .ok()
        .filter(|&offset| offset <= Evm::MEMORY_SIZE_LIMIT - size)
}

/// Converts an [`Int`] to a stack word.
#[inline(always)]
pub fn word(int: Int) -> U256 {
    U256::from_be_bytes(int.to_bytes())
}

/// Converts a stack word to an [`Int`].
#[inline(always)]
pub fn int(word: U256) -> Int {
    Int::new(word.to_be_bytes())
}

/// Check memory range using 256-bit values (avoids truncation before check).
pub fn mem_check_int(offset: Int, size: Int) -> EvmResult<()> {
    // A zero-length range does not touch memory, so its offset is irrelevant.
    if size.is_zero() {
        return Ok(());
    }
    let limit = Int::from(Evm::MEMORY_SIZE_LIMIT);
    // Bounding both operands first keeps their 256-bit sum from wrapping.
    if size > limit || offset > limit {
        return Err(EvmYield::Halt(HaltReason::OutOfMemory));
    }
    let add = lift(|[a, b]| a + b);
    if add([offset, size]) > limit {
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

/// The `JUMPDEST` instructions of a contract's code, which excludes `0x5B` bytes in PUSH data.
#[derive(Debug)]
pub struct JumpTable(Vec<u64>);

impl JumpTable {
    /// Analyses `code`, skipping the immediate data of PUSH1-PUSH32.
    pub fn new(code: &[u8]) -> Self {
        const JUMPDEST: u8 = 0x5B;
        const PUSH1: u8 = 0x60;
        const PUSH32: u8 = 0x7F;
        let mut bits = vec![0u64; code.len().div_ceil(64)];
        let mut pc = 0;
        while let Some(&op) = code.get(pc) {
            if op == JUMPDEST {
                bits[pc / 64] |= 1 << (pc % 64);
            }
            pc += 1;
            if (PUSH1..=PUSH32).contains(&op) {
                pc += usize::from(op - PUSH1) + 1;
            }
        }
        Self(bits)
    }

    /// Returns whether `pc` is a `JUMPDEST` instruction.
    pub fn contains(&self, pc: usize) -> bool {
        self.0
            .get(pc / 64)
            .is_some_and(|bits| bits & (1 << (pc % 64)) != 0)
    }
}

fn is_jump(op: u8) -> bool {
    op == 0x56 || op == 0x57
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_warm_ups_are_warm() {
        let acc = Acc::from(7u64);
        let mut evm = Evm::new(Head::default(), vec![], 1000, Int::ONE, Int::ONE, vec![]);
        let state = crate::ops::tests::state();
        assert!(evm.is_cold_acc(&state, &acc));
        evm.warm_acc(&acc);
        assert!(!evm.is_cold_acc(&state, &acc));
    }

    #[test]
    fn zero_length_ranges_accept_any_offset() {
        assert!(mem_check_int(Int::MAX, Int::ZERO).is_ok());
        assert!(matches!(
            mem_check_int(Int::MAX, Int::ONE),
            Err(EvmYield::Halt(HaltReason::OutOfMemory))
        ));
    }

    #[test]
    fn jumpdests_skip_push_data() {
        // JUMPDEST PUSH2 0x5B5B JUMPDEST PUSH1 (truncated)
        let table = JumpTable::new(&[0x5B, 0x61, 0x5B, 0x5B, 0x5B, 0x60]);
        let jumpdests: Vec<_> = (0..7).map(|pc| table.contains(pc)).collect();
        assert_eq!(jumpdests, [true, false, false, false, true, false, false]);
    }
}
