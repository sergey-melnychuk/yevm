use yevm_misc::hex::{Hex, parse};

pub type Acc = Hex<20>;

pub const fn acc(s: &str) -> Acc {
    Acc::new(parse(s))
}
