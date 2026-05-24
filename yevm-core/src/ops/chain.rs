use yevm_base::{Acc, Int};

use crate::{
    Call,
    evm::{Context, Evm, EvmResult, EvmYield, Fetch, HaltReason, mem_check_int},
    state::State,
};

pub fn address(evm: &mut Evm, ctx: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(2)?;
    let r = ctx.this.to();
    evm.push(r)?;
    Ok(())
}

pub fn balance<S: State>(evm: &mut Evm, _: &Context, _: &Call, state: &mut S) -> EvmResult<()> {
    evm.gas_charge(100)?;
    let [acc] = evm.peek()?;
    let acc: Acc = acc.to();
    let Some(balance) = state.balance(&acc) else {
        return Err(EvmYield::Fetch(Fetch::Balance(acc)));
    };

    if state.is_cold_acc(&acc) {
        evm.warm_acc(&acc);
        evm.gas_charge(2_500)?;
    }
    evm.push(balance)?;
    if let Some(step) = evm.step.as_mut() {
        step.debug.push(format!("BALANCE: {balance:?}"));
    }
    Ok(())
}

pub fn origin(evm: &mut Evm, ctx: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(2)?;
    evm.push(ctx.origin.to())?;
    Ok(())
}

pub fn caller(evm: &mut Evm, _: &Context, call: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(2)?;
    let caller = call.by;
    evm.push(caller.to())?;
    if let Some(step) = evm.step.as_mut() {
        step.debug.push(format!("CALLER: {caller:?}"));
    }
    Ok(())
}

pub fn callvalue(evm: &mut Evm, _: &Context, call: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(2)?;
    evm.push(call.eth)?;
    Ok(())
}

pub fn calldataload(evm: &mut Evm, _: &Context, call: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(3)?;
    let [offset] = evm.peek_usize()?;
    let mut word = [0u8; 32];
    if offset < call.data.0.len() {
        let copy = (call.data.0.len() - offset).min(32);
        word[..copy].copy_from_slice(&call.data.0[offset..offset + copy]);
    }
    evm.push(yevm_base::Int::from(&word[..]))?;
    Ok(())
}

pub fn calldatasize(evm: &mut Evm, _: &Context, call: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(2)?;
    evm.push(call.data.0.len().into())?;
    Ok(())
}

pub fn calldatacopy(evm: &mut Evm, _: &Context, call: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(3)?;
    let [dest_offset, offset, size] = evm.peek::<3>()?;
    mem_check_int(dest_offset, size)?;
    let (dest_offset, size) = (dest_offset.as_usize(), size.as_usize());
    evm.mem_expand(dest_offset, size)?;
    evm.gas_charge(3 * size.div_ceil(32) as i64)?;
    let data_len = call.data.0.len();
    let (lo, hi) = if offset >= Int::from(data_len) {
        (data_len, data_len)
    } else {
        let o = offset.as_usize();
        let h = o.saturating_add(size).min(data_len);
        (o.min(data_len), h)
    };
    let len = (lo..hi).len();
    if len > 0 {
        let data = &call.data.0[lo..hi];
        evm.mem_put(dest_offset, len, data)?;
    }
    if len < size {
        let pad = size - (lo..hi).len();
        let data = vec![0; pad];
        evm.mem_put(dest_offset + len, size - len, &data)?;
    }
    Ok(())
}

pub fn codesize(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(2)?;
    let size = evm.code.len();
    evm.push(size.into())?;
    Ok(())
}

pub fn codecopy(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(3)?;
    let [dest_offset, offset, size] = evm.peek::<3>()?;
    mem_check_int(dest_offset, size)?;
    let (dest_offset, size) = (dest_offset.as_usize(), size.as_usize());
    evm.mem_expand(dest_offset, size)?;
    evm.gas_charge(3 * size.div_ceil(32) as i64)?;
    let code_len = evm.code.len();
    // Use full 256-bit comparison: offset >= code_len means no overlap (avoids truncation bug)
    let (lo, hi) = if offset >= Int::from(code_len) {
        (code_len, code_len)
    } else {
        let o = offset.as_usize();
        let h = o.saturating_add(size).min(code_len);
        (o.min(code_len), h)
    };
    let len = (lo..hi).len();
    let mut ret = vec![0u8; size];
    ret[0..len].copy_from_slice(&evm.code[lo..hi]);
    evm.mem_put(dest_offset, size, &ret)?;
    Ok(())
}

pub fn gasprice(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(2)?;
    evm.push(evm.gas_price)?;
    Ok(())
}

pub fn extcodesize<S: State>(evm: &mut Evm, _: &Context, _: &Call, state: &mut S) -> EvmResult<()> {
    evm.gas_charge(100)?;
    let [acc] = evm.peek()?;
    let acc: Acc = acc.to();
    let Some((code, _)) = state.code(&acc) else {
        return Err(EvmYield::Fetch(Fetch::Code(acc)));
    };
    if state.is_cold_acc(&acc) {
        evm.warm_acc(&acc);
        evm.gas_charge(2_500)?;
    }
    evm.push(code.0.len().into())?;
    Ok(())
}

pub fn extcodecopy<S: State>(evm: &mut Evm, _: &Context, _: &Call, state: &mut S) -> EvmResult<()> {
    evm.gas_charge(100)?;
    let [acc, dest_offset, offset, size] = evm.peek()?;
    let acc: Acc = acc.to();
    mem_check_int(dest_offset, size)?;
    let (dest_offset, size) = (dest_offset.as_usize(), size.as_usize());

    let Some((code, _)) = state.code(&acc) else {
        return Err(EvmYield::Fetch(Fetch::Code(acc)));
    };
    if state.is_cold_acc(&acc) {
        evm.warm_acc(&acc);
        evm.gas_charge(2_500)?;
    }
    evm.mem_expand(dest_offset, size)?;
    evm.gas_charge(3 * size.div_ceil(32) as i64)?;

    let code_len = code.0.len();
    let (lo, hi) = if offset >= Int::from(code_len) {
        (code_len, code_len)
    } else {
        let o = offset.as_usize();
        let h = o.saturating_add(size).min(code_len);
        (o.min(code_len), h)
    };
    let len = (lo..hi).len();
    if len > 0 {
        let data = &code.0[lo..hi];
        evm.mem_put(dest_offset, len, data)?;
    }
    if len < size {
        let pad = size - (lo..hi).len();
        let data = vec![0; pad];
        evm.mem_put(dest_offset + len, size - len, &data)?;
    }
    Ok(())
}

pub fn returndatasize(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(2)?;
    evm.push(evm.ret.len().into())?;
    Ok(())
}

pub fn returndatacopy(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(3)?;
    let [dest_offset, offset, size] = evm.peek::<3>()?;
    mem_check_int(offset, size)?;
    mem_check_int(dest_offset, size)?;
    // EVM spec: halt if copy range exceeds return data buffer (256-bit check)
    // Fast path: when offset+size fits in usize and <= ret.len(), no need for 256-bit.
    // Otherwise use 256-bit comparison (avoids BadCopyRange when offset/size truncate).
    let ret_len = evm.ret.len();
    let ret_len_int = Int::from(ret_len);
    let (offset_u, size_u) = (offset.as_usize(), size.as_usize());
    let ok_fast = offset_u
        .checked_add(size_u)
        .map(|end| end <= ret_len)
        .unwrap_or(false);
    if !ok_fast {
        let add = yevm_base::math::lift(|[a, b]| a + b);
        let gt = yevm_base::math::lift(|[a, b]| {
            if a > b {
                yevm_base::math::U256::ONE
            } else {
                yevm_base::math::U256::ZERO
            }
        });
        let end = add([offset, size]);
        if !gt([end, ret_len_int]).is_zero() {
            return Err(EvmYield::Halt(HaltReason::BadCopyRange));
        }
    }
    let (dest_offset, offset, size) = (dest_offset.as_usize(), offset.as_usize(), size.as_usize());
    evm.gas_charge(3 * size.div_ceil(32) as i64)?;

    let ret_len = evm.ret.len();
    let (lo, copy_len) = if offset >= ret_len {
        (ret_len, 0)
    } else {
        let copy_len = size.min(ret_len - offset);
        (offset, copy_len)
    };
    if copy_len > 0 {
        let data = evm.ret[lo..lo + copy_len].to_vec();
        evm.mem_put(dest_offset, copy_len, &data)?;
    }
    if copy_len < size {
        let pad = size - copy_len;
        let data = vec![0; pad];
        evm.mem_put(dest_offset + copy_len, pad, &data)?;
    }
    Ok(())
}

pub fn extcodehash<S: State>(evm: &mut Evm, _: &Context, _: &Call, state: &mut S) -> EvmResult<()> {
    evm.gas_charge(100)?;
    let [acc] = evm.peek()?;
    let acc: Acc = acc.to();
    let Some(account) = state.acc(&acc) else {
        return Err(EvmYield::Fetch(Fetch::Code(acc)));
    };
    if state.is_cold_acc(&acc) {
        evm.warm_acc(&acc);
        evm.gas_charge(2_500)?;
    }
    // EIP-1052 / EIP-161: return 0 for empty accounts (balance=0, nonce=0, code=empty)
    let hash = if account.code.0.0.is_empty() && account.nonce.is_zero() && account.value.is_zero()
    {
        Int::ZERO
    } else if account.code.1.is_zero() && account.code.0.0.is_empty() {
        // Non-empty account (has balance or nonce) with no code: return keccak256("")
        yevm_misc::keccak256(&[])
    } else {
        account.code.1
    };
    evm.push(hash)?;
    Ok(())
}

pub fn blockhash<S: State>(evm: &mut Evm, _: &Context, _: &Call, state: &mut S) -> EvmResult<()> {
    evm.gas_charge(20)?;
    let [number] = evm.peek()?;
    let number = number.as_u64();
    let current = evm.head.number.as_u64();
    // EVM spec: return 0 for current/future blocks and blocks older than 256
    if number >= current || current - number > 256 {
        evm.push(Int::ZERO)?;
        return Ok(());
    }
    let Some(head) = state.head(number) else {
        return Err(EvmYield::Fetch(Fetch::BlockHash(number)));
    };
    evm.push(head.hash)?;
    Ok(())
}

pub fn coinbase(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(2)?;
    evm.push(evm.head.coinbase.to())?;
    Ok(())
}

pub fn timestamp(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(2)?;
    evm.push(evm.head.timestamp)?;
    Ok(())
}

pub fn number(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(2)?;
    evm.push(evm.head.number.to())?;
    Ok(())
}

pub fn prevrandao(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(2)?;
    evm.push(evm.head.prevrandao)?;
    Ok(())
}

pub fn gaslimit(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(2)?;
    evm.push(evm.head.gas_limit)?;
    Ok(())
}

pub fn chainid(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(2)?;
    evm.push(evm.chain_id)?;
    Ok(())
}

pub fn selfbalance<S: State>(
    evm: &mut Evm,
    ctx: &Context,
    _: &Call,
    state: &mut S,
) -> EvmResult<()> {
    evm.gas_charge(5)?;
    let acc: Acc = ctx.this;
    let Some(balance) = state.balance(&acc) else {
        return Err(EvmYield::Fetch(Fetch::Balance(acc)));
    };
    evm.push(balance)?;
    Ok(())
}

pub fn basefee(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(2)?;
    evm.push(evm.head.base_fee)?;
    Ok(())
}

pub fn blobhash(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(3)?;
    let [index] = evm.peek()?;
    let hash = index
        .as_usize_checked()
        .and_then(|i| evm.blob_hashes.get(i))
        .copied()
        .unwrap_or(Int::ZERO);
    evm.push(hash)?;
    Ok(())
}

pub fn blobbasefee(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(2)?;
    // TODO: proper blob handling
    // context: 0xd6859d613c7ffcfb371ec3c3eebc8ad37dc9480df0e6a304c80864b104d6c962/24935686:49
    // excess = 0x0da161cb must lead to blob_base_fee = 0x12d4542f (source: chain state)
    // yet it does not fit `fake_exponent` calculations described in the spec!

    // let fee = evm.head.excess_blob_gas.map_or(Int::ZERO, |excess| {
    //     crate::call::blob_base_fee(evm.head.number.as_u64(), excess)
    // });
    // evm.push(fee)?;

    evm.push(Int::ONE)?;
    Ok(())
}
