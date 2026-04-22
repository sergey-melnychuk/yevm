use yaevmi_base::{
    Int,
    math::{ONE, ZERO, lift},
};
use yaevmi_misc::keccak256;

use crate::evm::{self, Evm, EvmResult, EvmYield};

pub fn stop(_: &mut Evm) -> EvmResult<()> {
    Err(EvmYield::Return(vec![]))
}

#[inline]
pub fn add(evm: &mut Evm) -> EvmResult<()> {
    evm.gas_charge(3)?;
    let [a, b] = evm.peek()?;
    let f = lift(|[a, b]| a + b);
    let r = f([a, b]);
    evm.push(r)?;
    Ok(())
}

pub fn mul(evm: &mut Evm) -> EvmResult<()> {
    evm.gas_charge(5)?;
    let [a, b] = evm.peek()?;
    let f = lift(|[a, b]| a * b);
    let r = f([a, b]);
    evm.push(r)?;
    Ok(())
}

pub fn sub(evm: &mut Evm) -> EvmResult<()> {
    evm.gas_charge(3)?;
    let [a, b] = evm.peek()?;
    let f = lift(|[a, b]| a - b);
    let r = f([a, b]);
    evm.push(r)?;
    Ok(())
}

pub fn div(evm: &mut Evm) -> EvmResult<()> {
    evm.gas_charge(5)?;
    let [a, b] = evm.peek()?;
    if b.is_zero() {
        evm.push(Int::ZERO)?;
        return Ok(());
    }
    let f = lift(|[a, b]| a / b);
    let r = f([a, b]);
    evm.push(r)?;
    Ok(())
}

pub fn sdiv(evm: &mut Evm) -> EvmResult<()> {
    use yaevmi_base::math::U256;
    evm.gas_charge(5)?;
    let [a, b] = evm.peek()?;
    if b.is_zero() {
        evm.push(Int::ZERO)?;
        return Ok(());
    }
    let f = lift(|[a, b]| {
        let neg = |x: U256| (!x) + U256::ONE;
        let sign_a = a.bit(255);
        let sign_b = b.bit(255);
        let abs_a = if sign_a { neg(a) } else { a };
        let abs_b = if sign_b { neg(b) } else { b };
        let result = abs_a / abs_b;
        if sign_a != sign_b {
            neg(result)
        } else {
            result
        }
    });
    let r = f([a, b]);
    evm.push(r)?;
    Ok(())
}

pub fn r#mod(evm: &mut Evm) -> EvmResult<()> {
    evm.gas_charge(5)?;
    let [a, b] = evm.peek()?;
    if b.is_zero() {
        evm.push(Int::ZERO)?;
        return Ok(());
    }
    let f = lift(|[a, b]| a % b);
    let r = f([a, b]);
    evm.push(r)?;
    Ok(())
}

pub fn smod(evm: &mut Evm) -> EvmResult<()> {
    use yaevmi_base::math::U256;
    evm.gas_charge(5)?;
    let [a, b] = evm.peek()?;
    let f = lift(|[a, b]| {
        if b.is_zero() {
            return U256::ZERO;
        }
        let neg = |x: U256| (!x) + U256::ONE;
        let sign_a = a.bit(255);
        let abs_a = if sign_a { neg(a) } else { a };
        let abs_b = if b.bit(255) { neg(b) } else { b };
        let result = abs_a % abs_b;
        if sign_a && !result.is_zero() {
            neg(result)
        } else {
            result
        }
    });
    let r = f([a, b]);
    evm.push(r)?;
    Ok(())
}

pub fn addmod(evm: &mut Evm) -> EvmResult<()> {
    evm.gas_charge(8)?;
    let [a, b, m] = evm.peek()?;
    let f = lift(|[a, b, m]| a.add_mod(b, m));
    let r = f([a, b, m]);
    evm.push(r)?;
    Ok(())
}

pub fn mulmod(evm: &mut Evm) -> EvmResult<()> {
    evm.gas_charge(8)?;
    let [a, b, m] = evm.peek()?;
    let f = lift(|[a, b, m]| a.mul_mod(b, m));
    let r = f([a, b, m]);
    evm.push(r)?;
    Ok(())
}

pub fn exp(evm: &mut Evm) -> EvmResult<()> {
    let [a, b] = evm.peek()?;
    // EIP-160: 10 + 50 * exponent_byte_size
    let exp_cost = if b.is_zero() {
        10
    } else {
        use yaevmi_base::math::U256;
        let b_u = U256::from_be_slice(b.as_ref());
        let bit_len = 256 - b_u.leading_zeros();
        10 + 50 * bit_len.div_ceil(8) as i64
    };
    evm.gas_charge(exp_cost)?;
    let f = lift(|[a, b]| a.pow(b));
    let r = f([a, b]);
    evm.push(r)?;
    Ok(())
}

pub fn signextend(evm: &mut Evm) -> EvmResult<()> {
    use yaevmi_base::math::U256;
    evm.gas_charge(5)?;
    let [b, x] = evm.peek()?;
    let f = lift(|[b, x]| {
        if b >= U256::from(32u32) {
            return x;
        }
        let b = b.saturating_to::<usize>();
        let sign_bit = b * 8 + 7;
        let low_bits = b * 8 + 8; // == (b+1)*8
        let mask = if low_bits == 256 {
            U256::MAX
        } else {
            (U256::ONE << low_bits) - U256::ONE
        };
        if x.bit(sign_bit) { x | !mask } else { x & mask }
    });
    let r = f([b, x]);
    evm.push(r)?;
    Ok(())
}

pub fn lt(evm: &mut Evm) -> EvmResult<()> {
    evm.gas_charge(3)?;
    let [a, b] = evm.peek()?;
    let f = lift(|[a, b]| if a < b { ONE } else { ZERO });
    let r = f([a, b]);
    evm.push(r)?;
    Ok(())
}

pub fn gt(evm: &mut Evm) -> EvmResult<()> {
    evm.gas_charge(3)?;
    let [a, b] = evm.peek()?;
    let f = lift(|[a, b]| if a > b { ONE } else { ZERO });
    let r = f([a, b]);
    evm.push(r)?;
    Ok(())
}

pub fn slt(evm: &mut Evm) -> EvmResult<()> {
    evm.gas_charge(3)?;
    let [a, b] = evm.peek()?;
    let f = lift(|[a, b]| {
        // Signed: flip the sign bit so the unsigned order matches signed order
        let key = |x| x ^ (ONE << 255);
        if key(a) < key(b) { ONE } else { ZERO }
    });
    let r = f([a, b]);
    evm.push(r)?;
    Ok(())
}

pub fn sgt(evm: &mut Evm) -> EvmResult<()> {
    evm.gas_charge(3)?;
    let [a, b] = evm.peek()?;
    let f = lift(|[a, b]| {
        let key = |x| x ^ (ONE << 255);
        if key(a) > key(b) { ONE } else { ZERO }
    });
    let r = f([a, b]);
    evm.push(r)?;
    Ok(())
}

pub fn eq(evm: &mut Evm) -> EvmResult<()> {
    evm.gas_charge(3)?;
    let [a, b] = evm.peek()?;
    let f = lift(|[a, b]| if a == b { ONE } else { ZERO });
    let r = f([a, b]);
    evm.push(r)?;
    Ok(())
}

pub fn iszero(evm: &mut Evm) -> EvmResult<()> {
    evm.gas_charge(3)?;
    let [x] = evm.peek()?;
    let f = lift(|[x]| if x.is_zero() { ONE } else { ZERO });
    let r = f([x]);
    evm.push(r)?;
    Ok(())
}

pub fn and(evm: &mut Evm) -> EvmResult<()> {
    evm.gas_charge(3)?;
    let [a, b] = evm.peek()?;
    let f = lift(|[a, b]| a & b);
    let r = f([a, b]);
    evm.push(r)?;
    Ok(())
}

pub fn or(evm: &mut Evm) -> EvmResult<()> {
    evm.gas_charge(3)?;
    let [a, b] = evm.peek()?;
    let f = lift(|[a, b]| a | b);
    let r = f([a, b]);
    evm.push(r)?;
    Ok(())
}

pub fn xor(evm: &mut Evm) -> EvmResult<()> {
    evm.gas_charge(3)?;
    let [a, b] = evm.peek()?;
    let f = lift(|[a, b]| a ^ b);
    let r = f([a, b]);
    evm.push(r)?;
    Ok(())
}

pub fn not(evm: &mut Evm) -> EvmResult<()> {
    evm.gas_charge(3)?;
    let [x] = evm.peek()?;
    let f = lift(|[x]| !x);
    let r = f([x]);
    evm.push(r)?;
    Ok(())
}

pub fn byte(evm: &mut Evm) -> EvmResult<()> {
    evm.gas_charge(3)?;
    let [idx, int] = evm.peek()?;
    let byte = int
        .as_ref()
        .get(idx.as_usize())
        .copied()
        .unwrap_or_default();
    evm.push(Int::from(byte))?;
    Ok(())
}

pub fn shl(evm: &mut Evm) -> EvmResult<()> {
    evm.gas_charge(3)?;
    let [shift, val] = evm.peek()?;
    let f = lift(|[shift, val]| val << shift);
    let r = f([shift, val]);
    evm.push(r)?;
    Ok(())
}

pub fn shr(evm: &mut Evm) -> EvmResult<()> {
    evm.gas_charge(3)?;
    let [shift, val] = evm.peek()?;
    let f = lift(|[shift, val]| val >> shift);
    let r = f([shift, val]);
    evm.push(r)?;
    Ok(())
}

pub fn sar(evm: &mut Evm) -> EvmResult<()> {
    evm.gas_charge(3)?;
    let [shift, val] = evm.peek()?;
    let f = lift(|[shift, val]| val.arithmetic_shr(shift.saturating_to::<usize>()));
    let r = f([shift, val]);
    evm.push(r)?;
    Ok(())
}

pub fn clz(evm: &mut Evm) -> EvmResult<()> {
    use yaevmi_base::math::U256;
    evm.gas_charge(5)?;
    let [x] = evm.peek()?;
    let f = lift(|[x]| U256::from(x.leading_zeros()));
    let r = f([x]);
    evm.push(r)?;
    Ok(())
}

pub fn hash(evm: &mut Evm) -> EvmResult<()> {
    evm.gas_charge(30)?;
    let [offset, size] = evm.peek()?;
    evm::mem_check_int(offset, size)?;
    let (offset, size) = (offset.as_usize(), size.as_usize());
    evm.gas_charge(6 * size.div_ceil(32) as i64)?;
    let data = evm.mem_get(offset, size)?;
    let hash = keccak256(&data);
    evm.push(hash)?;
    Ok(())
}
