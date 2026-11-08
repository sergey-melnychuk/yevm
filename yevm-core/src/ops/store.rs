use yevm_base::Int;

use crate::{
    Call,
    evm::{Context, Evm, EvmResult, EvmYield, HaltReason, mem_check_int},
    state::State,
};

pub fn pop(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(2)?;
    let [_] = evm.peek()?;
    Ok(())
}

#[inline]
pub fn mload(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(3)?;
    let [offset] = evm.peek::<1>()?;
    mem_check_int(offset, Int::from(32u32))?;
    let offset = offset.as_usize();
    evm.mem_expand(offset, 32)?;
    let int = Int::from(&evm.memory[offset..offset + 32]);
    evm.push(int)?;
    Ok(())
}

#[inline]
pub fn mstore(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(3)?;
    let [offset, value] = evm.peek()?;
    mem_check_int(offset, Int::from(32u32))?;
    evm.mem_put(offset.as_usize(), 32, value.as_ref())?;
    Ok(())
}

pub fn mstore8(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(3)?;
    let [offset, value] = evm.peek()?;
    mem_check_int(offset, Int::from(1u32))?;
    let (offset, value) = (offset.as_usize(), value.as_u8());
    evm.mem_put(offset, 1, &[value])?;
    Ok(())
}

pub fn sload<S: State>(evm: &mut Evm, ctx: &Context, _: &Call, state: &mut S) -> EvmResult<()> {
    evm.gas_charge(100)?;
    let [key] = evm.peek()?;
    let acc = ctx.this;
    let Some((val, cold)) = state.get_warm(&acc, &key) else {
        return Err(EvmYield::Fetch(crate::evm::Fetch::StateCell(acc, key)));
    };
    if cold {
        evm.gas_charge(2000)?;
    }
    evm.push(val)?;
    Ok(())
}

// https://www.evm.codes/?fork=osaka#55
fn sstore_gas(val: Int, cur: Int, org: Int) -> (i64, i64) {
    // static_gas = 0
    // if value == current_value
    //     base_dynamic_gas = 100
    // else if current_value == original_value
    //     if original_value == 0
    //         base_dynamic_gas = 20000
    //     else
    //         base_dynamic_gas = 2900
    // else
    //     base_dynamic_gas = 100
    let g = if val == cur {
        100
    } else if cur == org {
        if org.is_zero() { 20_000 } else { 2_900 }
    } else {
        100
    };

    // if value != current_value
    //     if current_value == original_value
    //         if original_value != 0 and value == 0
    //             gas_refunds += 4800
    //     else
    //         if original_value != 0
    //             if current_value == 0
    //                 gas_refunds -= 4800
    //             else if value == 0
    //                 gas_refunds += 4800
    //         if value == original_value
    //             if original_value == 0
    //                 gas_refunds += 20000 - 100
    //             else
    //                 gas_refunds += 5000 - 2100 - 100
    let mut r = 0;
    if val != cur {
        if cur == org {
            if !org.is_zero() && val.is_zero() {
                r += 4_800;
            }
        } else {
            if !org.is_zero() {
                if cur.is_zero() {
                    r -= 4_800;
                } else if val.is_zero() {
                    r += 4_800;
                }
            }
            if val == org {
                if org.is_zero() {
                    r += 20_000 - 100;
                } else {
                    r += 5_000 - 2_100 - 100;
                }
            }
        }
    }

    (g, r)
}

pub fn sstore<S: State>(evm: &mut Evm, ctx: &Context, _: &Call, state: &mut S) -> EvmResult<()> {
    if ctx.is_static {
        return Err(EvmYield::Halt(HaltReason::NonStatic));
    }
    // EIP-2200: reentrancy sentinel - SSTORE fails if gasleft <= 2300 (call stipend)
    if evm.gas_remaining() <= 2300 {
        return Err(EvmYield::Halt(HaltReason::GasBelowStipend));
    }
    let [key, val] = evm.peek()?;
    let acc = ctx.this;
    let Some((cur, org, is_cold)) = state.get_with_warmth(&acc, &key) else {
        return Err(EvmYield::Fetch(crate::evm::Fetch::StateCell(acc, key)));
    };
    let (mut gas, refund) = sstore_gas(val, cur, org);
    if is_cold {
        gas += 2100;
    }
    evm.gas_charge(gas)?;
    evm.gas_refund(refund)?;
    if let Some(step) = evm.step.as_mut() {
        step.debug.push(format!("SSTORE: key={key:?}"));
        step.debug.push(format!("SSTORE: val={val:?}"));
        step.debug.push(format!("SSTORE: cur={cur:?}"));
        step.debug.push(format!("SSTORE: org={org:?}"));
        step.debug
            .push(format!("SSTORE: cold={is_cold} gas={gas} refund={refund}"));
    }
    // Nothing after this point can fail, so the slot is warmed together with the write.
    state.put_warm(&acc, &key, val);
    Ok(())
}

pub fn jumpdest(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(1)?;
    Ok(())
}

pub fn jump(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(8)?;
    let [dst] = evm.peek()?;
    let dst = jump_destination(evm, dst)?;
    evm.pc = dst;
    Ok(())
}

pub fn jumpi(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(10)?;
    let [dst, val] = evm.peek()?;
    if val.is_zero() {
        evm.pc += 1;
        return Ok(());
    }
    let dst = jump_destination(evm, dst)?;
    evm.pc = dst;
    Ok(())
}

/// Returns `dst` if it is a `JUMPDEST` instruction, i.e. not a `0x5B` byte inside PUSH data.
fn jump_destination(evm: &mut Evm, dst: Int) -> EvmResult<usize> {
    match dst.as_usize_checked() {
        Some(dst) if evm.is_jumpdest(dst) => Ok(dst),
        dst => Err(EvmYield::Halt(HaltReason::BadJump(
            dst.unwrap_or(usize::MAX),
        ))),
    }
}

pub fn pc(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(2)?;
    evm.push(evm.pc.into())?;
    Ok(())
}

pub fn msize(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(2)?;
    let len = evm.memory.len();
    evm.push(len.into())?;
    Ok(())
}

pub fn gas(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(2)?;
    let gas = evm.gas_remaining();
    evm.push((gas as usize).into())?;
    Ok(())
}

pub fn tload<S: State>(evm: &mut Evm, ctx: &Context, _: &Call, state: &mut S) -> EvmResult<()> {
    evm.gas_charge(100)?;
    let [key] = evm.peek()?;
    let val = state.tget(&ctx.this, &key).unwrap_or_default();
    evm.push(val)?;
    Ok(())
}

pub fn tstore<S: State>(evm: &mut Evm, ctx: &Context, _: &Call, state: &mut S) -> EvmResult<()> {
    if ctx.is_static {
        return Err(EvmYield::Halt(HaltReason::NonStatic));
    }
    evm.gas_charge(100)?;
    let [key, val] = evm.peek()?;
    state.tput(ctx.this, key, val);
    Ok(())
}

pub fn mcopy(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(3)?;
    let [dest_offset, offset, size] = evm.peek::<3>()?;
    mem_check_int(dest_offset, size)?;
    mem_check_int(offset, size)?;
    let (dest_offset, offset, size) = (dest_offset.as_usize(), offset.as_usize(), size.as_usize());
    evm.gas_charge(3 * size.div_ceil(32) as i64)?;
    let data = evm.mem_get(offset, size)?.to_vec();
    evm.mem_put(dest_offset, size, &data)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::tests::{call, ctx, state};
    use super::*;
    use crate::{call::Head, evm::Evm};

    fn evm(code: Vec<u8>, stack: Vec<Int>) -> Evm {
        let mut evm = Evm::new(Head::default(), code, 1000, Int::ONE, Int::ONE, vec![]);
        evm.stack = stack.into_iter().map(crate::evm::word).collect();
        evm
    }

    #[test]
    fn jump_into_push_data_halts() {
        // PUSH1 0x5B JUMPDEST: offset 1 is PUSH data, offset 2 is an instruction.
        let code = vec![0x60, 0x5B, 0x5B];
        let mut push_data = evm(code.clone(), vec![Int::from(1u32)]);
        assert!(matches!(
            jump(&mut push_data, &ctx(), &call(), &mut state()),
            Err(EvmYield::Halt(HaltReason::BadJump(1)))
        ));
        let mut instruction = evm(code, vec![Int::from(2u32)]);
        jump(&mut instruction, &ctx(), &call(), &mut state()).unwrap();
        assert_eq!(instruction.pc, 2);
    }

    #[test]
    fn jump_beyond_usize_halts() {
        // 2^64 + 1 would truncate to the JUMPDEST at offset 1.
        let mut dst = [0u8; 32];
        dst[23] = 1;
        dst[31] = 1;
        let mut evm = evm(vec![0x00, 0x5B], vec![Int::new(dst)]);
        assert!(matches!(
            jump(&mut evm, &ctx(), &call(), &mut state()),
            Err(EvmYield::Halt(HaltReason::BadJump(usize::MAX)))
        ));
    }
}
