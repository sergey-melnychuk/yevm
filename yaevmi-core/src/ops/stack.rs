use crate::{
    Call, Int,
    evm::{Context, Evm, EvmResult, EvmYield, HaltReason},
    state::State,
};

pub fn push(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    let data = evm.data(evm.pc);
    let int = if data.is_empty() {
        evm.gas_charge(2)?;
        Int::ZERO
    } else {
        evm.gas_charge(3)?;
        Int::from(data.as_ref())
    };
    evm.push(int)?;
    evm.pc += data.len();
    Ok(())
}

pub fn dup(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(3)?;
    let op = evm.code[evm.pc];
    let n = idx(op) - 1;
    let Some(int) = evm.stack.iter().rev().nth(n).copied() else {
        return Err(EvmYield::Halt(HaltReason::StackUnderflow));
    };
    evm.push(int)?;
    Ok(())
}

pub fn swap(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    evm.gas_charge(3)?;
    let op = evm.code[evm.pc];
    let n = idx(op); // SWAP{k}: swap top with (k+1)th, distance = k = idx(op)
    if evm.stack.len() <= n {
        return Err(EvmYield::Halt(HaltReason::StackUnderflow));
    }
    let i = evm.stack.len() - 1;
    let j = i - n;
    evm.stack.swap(i, j);
    Ok(())
}

fn idx(op: u8) -> usize {
    match op {
        0x60..0x80 => op as usize - 0x60 + 1, // PUSH{1..32}
        0x80..0x90 => op as usize - 0x80 + 1, // DUP{1..16}
        0x90..0xA0 => op as usize - 0x90 + 1, // SWAP{1..16}
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{call, ctx, state};
    use super::*;
    use crate::call::Head;
    use crate::{
        Int,
        evm::{Evm, HaltReason},
    };

    fn int(val: u64) -> Int {
        Int::from(val)
    }

    fn is_halt(result: EvmResult<()>, expected: HaltReason) -> bool {
        match (result, expected) {
            (Err(EvmYield::Halt(HaltReason::StackUnderflow)), HaltReason::StackUnderflow) => true,
            (Err(EvmYield::Halt(HaltReason::BadOpcode(a))), HaltReason::BadOpcode(b)) => a == b,
            _ => false,
        }
    }

    // --- PUSH ---

    #[test]
    fn test_push0() {
        let head = Head::default();
        let mut evm = Evm::new(head, vec![0x5F], 1000, Int::ONE, Int::ONE, vec![]); // PUSH0
        push(&mut evm, &ctx(), &call(), &mut state()).unwrap();
        evm.apply(&mut state());
        assert_eq!(evm.stack, vec![Int::ZERO]);
        assert_eq!(evm.pc, 0);
    }

    #[test]
    fn test_push1() {
        let head = Head::default();
        let mut evm = Evm::new(head, vec![0x60, 0x42], 1000, Int::ONE, Int::ONE, vec![]); // PUSH1 0x42
        push(&mut evm, &ctx(), &call(), &mut state()).unwrap();
        evm.apply(&mut state());
        assert_eq!(evm.stack, vec![int(0x42)]);
        assert_eq!(evm.pc, 1);
    }

    #[test]
    fn test_push2() {
        let head = Head::default();
        let mut evm = Evm::new(
            head,
            vec![0x61, 0x01, 0x02],
            1000,
            Int::ONE,
            Int::ONE,
            vec![],
        ); // PUSH2 0x0102
        push(&mut evm, &ctx(), &call(), &mut state()).unwrap();
        evm.apply(&mut state());
        assert_eq!(evm.stack, vec![int(0x0102)]);
        assert_eq!(evm.pc, 2);
    }

    #[test]
    fn test_push32() {
        let mut code = vec![0x7F]; // PUSH32
        code.extend(1u8..=32);
        let head = Head::default();
        let mut evm = Evm::new(head, code, 1000, Int::ONE, Int::ONE, vec![]);
        push(&mut evm, &ctx(), &call(), &mut state()).unwrap();
        evm.apply(&mut state());
        assert_eq!(evm.stack.len(), 1);
        assert_eq!(evm.pc, 32);
        let expected: Vec<u8> = (1u8..=32).collect();
        assert_eq!(evm.stack[0], Int::from(expected.as_slice()));
    }

    #[test]
    fn test_push_truncated() {
        let head = Head::default();
        let mut evm = Evm::new(head, vec![0x61, 0xFF], 1000, Int::ONE, Int::ONE, vec![]);
        push(&mut evm, &ctx(), &call(), &mut state()).unwrap();
        evm.apply(&mut state());
        assert_eq!(evm.stack.len(), 1);
        assert_eq!(evm.pc, 2);
        assert_eq!(evm.stack[0], Int::from(0xFF00));
    }

    // --- DUP ---

    #[test]
    fn test_dup1() {
        let a = int(1);
        let head = Head::default();
        let mut evm = Evm::new(head, vec![0x80], 1000, Int::ONE, Int::ONE, vec![]); // DUP1
        evm.stack.push(a);
        dup(&mut evm, &ctx(), &call(), &mut state()).unwrap();
        evm.apply(&mut state());
        assert_eq!(evm.stack, vec![a, a]);
        assert_eq!(evm.pc, 0);
    }

    #[test]
    fn test_dup2() {
        let (a, b) = (int(1), int(2));
        let head = Head::default();
        let mut evm = Evm::new(head, vec![0x81], 1000, Int::ONE, Int::ONE, vec![]); // DUP2
        evm.stack.extend([a, b]);
        dup(&mut evm, &ctx(), &call(), &mut state()).unwrap();
        evm.apply(&mut state());
        assert_eq!(evm.stack, vec![a, b, a]); // copies 2nd from top (a)
        assert_eq!(evm.pc, 0);
    }

    #[test]
    fn test_dup16() {
        let vals: Vec<Int> = (1..=16).map(int).collect();
        let head = Head::default();
        let mut evm = Evm::new(head, vec![0x8F], 1000, Int::ONE, Int::ONE, vec![]); // DUP16
        evm.stack.extend(vals.iter().copied());
        dup(&mut evm, &ctx(), &call(), &mut state()).unwrap();
        evm.apply(&mut state());
        assert_eq!(evm.stack.len(), 17);
        assert_eq!(*evm.stack.last().unwrap(), int(1)); // copies bottom (16th from top)
    }

    #[test]
    fn test_dup_underflow() {
        let head = Head::default();
        let mut evm = Evm::new(head, vec![0x81], 1000, Int::ONE, Int::ONE, vec![]); // DUP2, but only 1 item on stack
        evm.stack.push(int(1));
        let result = dup(&mut evm, &ctx(), &call(), &mut state());
        assert!(is_halt(result, HaltReason::StackUnderflow));
    }

    #[test]
    fn test_dup_empty() {
        let head = Head::default();
        let mut evm = Evm::new(head, vec![0x80], 1000, Int::ONE, Int::ONE, vec![]); // DUP1 on empty stack
        let result = dup(&mut evm, &ctx(), &call(), &mut state());
        assert!(is_halt(result, HaltReason::StackUnderflow));
    }

    // --- SWAP ---

    #[test]
    fn test_swap1() {
        let (a, b) = (int(1), int(2));
        let head = Head::default();
        let mut evm = Evm::new(head, vec![0x90], 1000, Int::ONE, Int::ONE, vec![]); // SWAP1
        evm.stack.extend([a, b]);
        swap(&mut evm, &ctx(), &call(), &mut state()).unwrap();
        evm.apply(&mut state());
        assert_eq!(evm.stack, vec![b, a]); // top swapped with 2nd
        assert_eq!(evm.pc, 0);
    }

    #[test]
    fn test_swap2() {
        let (a, b, c) = (int(1), int(2), int(3));
        let head = Head::default();
        let mut evm = Evm::new(head, vec![0x91], 1000, Int::ONE, Int::ONE, vec![]); // SWAP2
        evm.stack.extend([a, b, c]);
        swap(&mut evm, &ctx(), &call(), &mut state()).unwrap();
        evm.apply(&mut state());
        assert_eq!(evm.stack, vec![c, b, a]); // top swapped with 3rd
        assert_eq!(evm.pc, 0);
    }

    #[test]
    fn test_swap16() {
        let vals: Vec<Int> = (1..=17).map(int).collect(); // need 17 items for SWAP16
        let head = Head::default();
        let mut evm = Evm::new(head, vec![0x9F], 1000, Int::ONE, Int::ONE, vec![]); // SWAP16
        evm.stack.extend(vals.iter().copied());
        swap(&mut evm, &ctx(), &call(), &mut state()).unwrap();
        evm.apply(&mut state());
        assert_eq!(*evm.stack.last().unwrap(), int(1)); // bottom is now on top
        assert_eq!(evm.stack[0], int(17)); // top is now on bottom
    }

    #[test]
    fn test_swap1_underflow_empty() {
        let head = Head::default();
        let mut evm = Evm::new(head, vec![0x90], 1000, Int::ONE, Int::ONE, vec![]); // SWAP1 on empty stack
        let result = swap(&mut evm, &ctx(), &call(), &mut state());
        assert!(is_halt(result, HaltReason::StackUnderflow));
    }

    #[test]
    fn test_swap1_underflow_one() {
        let head = Head::default();
        let mut evm = Evm::new(head, vec![0x90], 1000, Int::ONE, Int::ONE, vec![]); // SWAP1 needs 2, has 1
        evm.stack.push(int(1));
        let result = swap(&mut evm, &ctx(), &call(), &mut state());
        eprintln!("RESULT: {result:?}");
        assert!(is_halt(result, HaltReason::StackUnderflow));
    }

    #[test]
    fn test_swap2_underflow() {
        let head = Head::default();
        let mut evm = Evm::new(head, vec![0x91], 1000, Int::ONE, Int::ONE, vec![]); // SWAP2 needs 3, has 2
        evm.stack.extend([int(1), int(2)]);
        let result = swap(&mut evm, &ctx(), &call(), &mut state());
        assert!(is_halt(result, HaltReason::StackUnderflow));
    }
}
