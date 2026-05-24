use yevm_misc::hex::{Hex, parse};

pub type Int = Hex<32>;

pub const fn int(s: &str) -> Int {
    Int::new(parse(s))
}

pub mod math {
    use ruint::Uint;

    use crate::Int;

    pub type U256 = Uint<256, 4>;

    pub struct Val(U256);

    pub const ONE: U256 = U256::ONE;
    pub const ZERO: U256 = U256::ZERO;

    impl From<Int> for Val {
        fn from(int: Int) -> Self {
            Val(U256::from_be_slice(int.as_ref()))
        }
    }

    impl From<Val> for Int {
        fn from(val: Val) -> Self {
            let buffer: [u8; 32] = val.0.to_be_bytes();
            Int::from(&buffer[..])
        }
    }

    pub fn lift<const N: usize>(f: impl Fn([U256; N]) -> U256) -> impl Fn([Int; N]) -> Int {
        move |xs: [Int; N]| {
            let mut ys = [U256::ZERO; N];
            for i in 0..N {
                let v: Val = xs[i].into();
                ys[i] = v.0;
            }
            let r = f(ys);
            Val(r).into()
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn test_add() {
            let f = lift(|[a, b]| a + b);
            let a = Int::from(41u32);
            let b = Int::ONE;
            let c = Int::from(42u32);
            assert_eq!(f([a, b]), c);
        }
    }
}
