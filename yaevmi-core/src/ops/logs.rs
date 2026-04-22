use crate::{
    Call,
    evm::{self, Context, Evm, EvmResult, EvmYield, HaltReason},
    state::State,
};

const LOG0: u8 = 0xA0;

pub fn log<S: State>(evm: &mut Evm, ctx: &Context, _: &Call, state: &mut S) -> EvmResult<()> {
    if ctx.is_static {
        return Err(EvmYield::Halt(HaltReason::NonStatic));
    }
    let n: usize = (evm.code[evm.pc] - LOG0) as usize;
    let total = 2 + n;
    if evm.stack.len() < total {
        return Err(EvmYield::Halt(HaltReason::StackUnderflow));
    }

    let [offset, size] = evm.peek()?;
    evm::mem_check_int(offset, size)?;
    let (offset, size) = (offset.as_usize(), size.as_usize());

    let base = 375 + 375 * n as i64;
    let max = (evm.gas_remaining() - base) / 8;
    if max < 0 || size > max as usize {
        evm.pending_gas_charge += evm.gas_remaining();
        return Err(EvmYield::Halt(HaltReason::OutOfGas));
    }

    let gas = base + 8 * size as i64;
    evm.gas_charge(gas)?;

    let data = evm.mem_get(offset, size)?.to_vec();

    let topics = evm.stack.iter().rev().skip(2).take(n).cloned().collect();

    evm.pending_stack_pops = total;
    state.log(data.into(), topics);
    Ok(())
}
