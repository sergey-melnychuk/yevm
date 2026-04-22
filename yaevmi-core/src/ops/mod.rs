use crate::{
    Call,
    evm::{Context, Evm, EvmResult, EvmYield, HaltReason},
    state::State,
};

pub mod basic;
pub mod calls;
pub mod chain;
pub mod logs;
pub mod stack;
pub mod store;

fn invalid(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    let op = evm.code.get(evm.pc).copied().unwrap_or_default();
    Err(EvmYield::Halt(HaltReason::BadOpcode(op)))
}

pub fn text(code: &[u8]) -> Vec<String> {
    let mut ret = Vec::with_capacity(code.len());
    let mut pc = 0;
    while pc < code.len() {
        let op = code[pc];
        let name = OPS[op as usize];
        let data = match op {
            0x60..0x80 => {
                let len = op as usize - 0x60 + 1; // PUSH{1..32}
                let lo = (pc + 1).min(code.len());
                let hi = (pc + 1 + len).min(code.len());
                pc += len;
                Some(hex::encode(&code[lo..hi]))
            }
            _ => None,
        };
        let data = data.map(|d| format!(" [{d}]")).unwrap_or_default();
        ret.push(format!("{pc:04x}: {name:16}{data}"));
        pc += 1;
    }
    ret
}

pub fn dispatch<S: State>(
    op: u8,
    evm: &mut Evm,
    ctx: &Context,
    call: &Call,
    state: &mut S,
) -> EvmResult<()> {
    match op {
        0x00 => basic::stop(evm),
        0x01 => basic::add(evm),
        0x02 => basic::mul(evm),
        0x03 => basic::sub(evm),
        0x04 => basic::div(evm),
        0x05 => basic::sdiv(evm),
        0x06 => basic::r#mod(evm),
        0x07 => basic::smod(evm),
        0x08 => basic::addmod(evm),
        0x09 => basic::mulmod(evm),
        0x0A => basic::exp(evm),
        0x0B => basic::signextend(evm),
        0x10 => basic::lt(evm),
        0x11 => basic::gt(evm),
        0x12 => basic::slt(evm),
        0x13 => basic::sgt(evm),
        0x14 => basic::eq(evm),
        0x15 => basic::iszero(evm),
        0x16 => basic::and(evm),
        0x17 => basic::or(evm),
        0x18 => basic::xor(evm),
        0x19 => basic::not(evm),
        0x1A => basic::byte(evm),
        0x1B => basic::shl(evm),
        0x1C => basic::shr(evm),
        0x1D => basic::sar(evm),
        0x1E => basic::clz(evm),
        0x20 => basic::hash(evm),
        0x30 => chain::address(evm, ctx, call, state),
        0x31 => chain::balance(evm, ctx, call, state),
        0x32 => chain::origin(evm, ctx, call, state),
        0x33 => chain::caller(evm, ctx, call, state),
        0x34 => chain::callvalue(evm, ctx, call, state),
        0x35 => chain::calldataload(evm, ctx, call, state),
        0x36 => chain::calldatasize(evm, ctx, call, state),
        0x37 => chain::calldatacopy(evm, ctx, call, state),
        0x38 => chain::codesize(evm, ctx, call, state),
        0x39 => chain::codecopy(evm, ctx, call, state),
        0x3A => chain::gasprice(evm, ctx, call, state),
        0x3B => chain::extcodesize(evm, ctx, call, state),
        0x3C => chain::extcodecopy(evm, ctx, call, state),
        0x3D => chain::returndatasize(evm, ctx, call, state),
        0x3E => chain::returndatacopy(evm, ctx, call, state),
        0x3F => chain::extcodehash(evm, ctx, call, state),
        0x40 => chain::blockhash(evm, ctx, call, state),
        0x41 => chain::coinbase(evm, ctx, call, state),
        0x42 => chain::timestamp(evm, ctx, call, state),
        0x43 => chain::number(evm, ctx, call, state),
        0x44 => chain::prevrandao(evm, ctx, call, state),
        0x45 => chain::gaslimit(evm, ctx, call, state),
        0x46 => chain::chainid(evm, ctx, call, state),
        0x47 => chain::selfbalance(evm, ctx, call, state),
        0x48 => chain::basefee(evm, ctx, call, state),
        0x49 => chain::blobhash(evm, ctx, call, state),
        0x4A => chain::blobbasefee(evm, ctx, call, state),
        0x50 => store::pop(evm, ctx, call, state),
        0x51 => store::mload(evm, ctx, call, state),
        0x52 => store::mstore(evm, ctx, call, state),
        0x53 => store::mstore8(evm, ctx, call, state),
        0x54 => store::sload(evm, ctx, call, state),
        0x55 => store::sstore(evm, ctx, call, state),
        0x56 => store::jump(evm, ctx, call, state),
        0x57 => store::jumpi(evm, ctx, call, state),
        0x58 => store::pc(evm, ctx, call, state),
        0x59 => store::msize(evm, ctx, call, state),
        0x5A => store::gas(evm, ctx, call, state),
        0x5B => store::jumpdest(evm, ctx, call, state),
        0x5C => store::tload(evm, ctx, call, state),
        0x5D => store::tstore(evm, ctx, call, state),
        0x5E => store::mcopy(evm, ctx, call, state),
        0x5F..=0x7F => stack::push(evm, ctx, call, state),
        0x80..=0x8F => stack::dup(evm, ctx, call, state),
        0x90..=0x9F => stack::swap(evm, ctx, call, state),
        0xA0..=0xA4 => logs::log(evm, ctx, call, state),
        0xF0 => calls::create(evm, ctx, call, state),
        0xF1 => calls::call(evm, ctx, call, state),
        0xF2 => calls::callcode(evm, ctx, call, state),
        0xF3 => calls::r#return(evm, ctx, call, state),
        0xF4 => calls::delegatecall(evm, ctx, call, state),
        0xF5 => calls::create2(evm, ctx, call, state),
        0xFA => calls::staticcall(evm, ctx, call, state),
        0xFD => calls::revert(evm, ctx, call, state),
        0xFF => calls::selfdestruct(evm, ctx, call, state),
        _ => invalid(evm, ctx, call, state),
    }
}

pub const OPS: [&str; 256] = [
    // 0x00
    "STOP",
    "ADD",
    "MUL",
    "SUB",
    "DIV",
    "SDIV",
    "MOD",
    "SMOD",
    "ADDMOD",
    "MULMOD",
    "EXP",
    "SIGNEXTEND",
    "INVALID/0x0C",
    "INVALID/0x0D",
    "INVALID/0x0E",
    "INVALID/0x0F",
    // 0x10
    "LT",
    "GT",
    "SLT",
    "SGT",
    "EQ",
    "ISZERO",
    "AND",
    "OR",
    "XOR",
    "NOT",
    "BYTE",
    "SHL",
    "SHR",
    "SAR",
    "CLZ", // TODO: FIXME: make it work for live & test
    // "INVALID/0x1E", // CLZ is not in the Cancun spec
    "INVALID/0x1F",
    // 0x20
    "KECCAK256",
    "INVALID/0x21",
    "INVALID/0x22",
    "INVALID/0x23",
    "INVALID/0x24",
    "INVALID/0x25",
    "INVALID/0x26",
    "INVALID/0x27",
    "INVALID/0x28",
    "INVALID/0x29",
    "INVALID/0x2A",
    "INVALID/0x2B",
    "INVALID/0x2C",
    "INVALID/0x2D",
    "INVALID/0x2E",
    "INVALID/0x2F",
    // 0x30
    "ADDRESS",
    "BALANCE",
    "ORIGIN",
    "CALLER",
    "CALLVALUE",
    "CALLDATALOAD",
    "CALLDATASIZE",
    "CALLDATACOPY",
    "CODESIZE",
    "CODECOPY",
    "GASPRICE",
    "EXTCODESIZE",
    "EXTCODECOPY",
    "RETURNDATASIZE",
    "RETURNDATACOPY",
    "EXTCODEHASH",
    // 0x40
    "BLOCKHASH",
    "COINBASE",
    "TIMESTAMP",
    "NUMBER",
    /* "PREVRANDAO", */ "DIFFICULTY",
    "GASLIMIT",
    "CHAINID",
    "SELFBALANCE",
    "BASEFEE",
    "BLOBHASH",
    "BLOBBASEFEE",
    "INVALID/0x4B",
    "INVALID/0x4C",
    "INVALID/0x4D",
    "INVALID/0x4E",
    "INVALID/0x4F",
    // 0x50
    "POP",
    "MLOAD",
    "MSTORE",
    "MSTORE8",
    "SLOAD",
    "SSTORE",
    "JUMP",
    "JUMPI",
    "PC",
    "MSIZE",
    "GAS",
    "JUMPDEST",
    "TLOAD",
    "TSTORE",
    "MCOPY",
    "PUSH0",
    // 0x60
    "PUSH1",
    "PUSH2",
    "PUSH3",
    "PUSH4",
    "PUSH5",
    "PUSH6",
    "PUSH7",
    "PUSH8",
    "PUSH9",
    "PUSH10",
    "PUSH11",
    "PUSH12",
    "PUSH13",
    "PUSH14",
    "PUSH15",
    "PUSH16",
    // 0x70
    "PUSH17",
    "PUSH18",
    "PUSH19",
    "PUSH20",
    "PUSH21",
    "PUSH22",
    "PUSH23",
    "PUSH24",
    "PUSH25",
    "PUSH26",
    "PUSH27",
    "PUSH28",
    "PUSH29",
    "PUSH30",
    "PUSH31",
    "PUSH32",
    // 0x80
    "DUP1",
    "DUP2",
    "DUP3",
    "DUP4",
    "DUP5",
    "DUP6",
    "DUP7",
    "DUP8",
    "DUP9",
    "DUP10",
    "DUP11",
    "DUP12",
    "DUP13",
    "DUP14",
    "DUP15",
    "DUP16",
    // 0x90
    "SWAP1",
    "SWAP2",
    "SWAP3",
    "SWAP4",
    "SWAP5",
    "SWAP6",
    "SWAP7",
    "SWAP8",
    "SWAP9",
    "SWAP10",
    "SWAP11",
    "SWAP12",
    "SWAP13",
    "SWAP14",
    "SWAP15",
    "SWAP16",
    // 0xA0
    "LOG0",
    "LOG1",
    "LOG2",
    "LOG3",
    "LOG4",
    "INVALID/0xA5",
    "INVALID/0xA6",
    "INVALID/0xA7",
    "INVALID/0xA8",
    "INVALID/0xA9",
    "INVALID/0xAA",
    "INVALID/0xAB",
    "INVALID/0xAC",
    "INVALID/0xAD",
    "INVALID/0xAE",
    "INVALID/0xAF",
    // 0xB0
    "INVALID/0xB0",
    "INVALID/0xB1",
    "INVALID/0xB2",
    "INVALID/0xB3",
    "INVALID/0xB4",
    "INVALID/0xB5",
    "INVALID/0xB6",
    "INVALID/0xB7",
    "INVALID/0xB8",
    "INVALID/0xB9",
    "INVALID/0xBA",
    "INVALID/0xBB",
    "INVALID/0xBC",
    "INVALID/0xBD",
    "INVALID/0xBE",
    "INVALID/0xBF",
    // 0xC0
    "INVALID/0xC0",
    "INVALID/0xC1",
    "INVALID/0xC2",
    "INVALID/0xC3",
    "INVALID/0xC4",
    "INVALID/0xC5",
    "INVALID/0xC6",
    "INVALID/0xC7",
    "INVALID/0xC8",
    "INVALID/0xC9",
    "INVALID/0xCA",
    "INVALID/0xCB",
    "INVALID/0xCC",
    "INVALID/0xCD",
    "INVALID/0xCE",
    "INVALID/0xCF",
    // 0xD0
    "INVALID/0xD0",
    "INVALID/0xD1",
    "INVALID/0xD2",
    "INVALID/0xD3",
    "INVALID/0xD4",
    "INVALID/0xD5",
    "INVALID/0xD6",
    "INVALID/0xD7",
    "INVALID/0xD8",
    "INVALID/0xD9",
    "INVALID/0xDA",
    "INVALID/0xDB",
    "INVALID/0xDC",
    "INVALID/0xDD",
    "INVALID/0xDE",
    "INVALID/0xDF",
    // 0xE0
    "INVALID/0xE0",
    "INVALID/0xE1",
    "INVALID/0xE2",
    "INVALID/0xE3",
    "INVALID/0xE4",
    "INVALID/0xE5",
    "INVALID/0xE6",
    "INVALID/0xE7",
    "INVALID/0xE8",
    "INVALID/0xE9",
    "INVALID/0xEA",
    "INVALID/0xEB",
    "INVALID/0xEC",
    "INVALID/0xED",
    "INVALID/0xEE",
    "INVALID/0xEF",
    // 0xF0
    "CREATE",
    "CALL",
    "CALLCODE",
    "RETURN",
    "DELEGATECALL",
    "CREATE2",
    "INVALID/0xF6",
    "INVALID/0xF7",
    "INVALID/0xF8",
    "INVALID/0xF9",
    "STATICCALL",
    "INVALID/0xFB",
    "INVALID/0xFC",
    "REVERT",
    "INVALID/0xFE",
    "SELFDESTRUCT",
];

#[cfg(test)]
pub mod tests {
    use yaevmi_misc::buf::Buf;

    use crate::{
        Acc, Call, Head, Int,
        evm::Context,
        state::{Account, State},
        trace::Event,
    };

    pub fn ctx() -> Context {
        Context {
            origin: Acc::ZERO,
            is_static: false,
            depth: 0,
            this: Acc::ZERO,
        }
    }

    pub fn call() -> Call {
        Call {
            by: Acc::ZERO,
            to: Some(Acc::ONE),
            gas: 0,
            eth: Int::ZERO,
            data: vec![].into(),
        }
    }

    pub fn state() -> Empty {
        Empty
    }

    #[derive(Default)]
    pub struct Empty;

    impl State for Empty {
        fn get(&mut self, _: &Acc, _: &Int) -> Option<(Int, Int)> {
            None
        }
        fn put(&mut self, _: &Acc, _: &Int, _: Int) -> Option<Int> {
            None
        }
        fn init(&mut self, _: &Acc, _: &Int, _: Int) {}
        fn tget(&mut self, _: &Acc, _: &Int) -> Option<Int> {
            None
        }
        fn tput(&mut self, _: Acc, _: Int, _: Int) -> Option<Int> {
            None
        }
        fn inc_nonce(&mut self, _: &Acc, _: Int) -> Int {
            Int::ZERO
        }
        fn set_value(&mut self, _: &Acc, _: Int) -> Int {
            Int::ZERO
        }
        fn set_code(&mut self, _: &Acc, _: Buf, _: Int) -> Int {
            Int::ZERO
        }
        fn balance(&mut self, _: &Acc) -> Option<Int> {
            None
        }
        fn nonce(&mut self, _: &Acc) -> Option<Int> {
            None
        }
        fn code(&mut self, _: &Acc) -> Option<(Buf, Int)> {
            None
        }
        fn acc(&mut self, _: &Acc) -> Option<Account> {
            None
        }
        fn merge(&mut self, _: &Acc, _: Account) {}
        fn is_cold_acc(&self, _: &Acc) -> bool {
            true
        }
        fn is_cold_key(&self, _: &Acc, _: &Int) -> bool {
            true
        }
        fn warm_acc(&mut self, _: &Acc) -> bool {
            false
        }
        fn warm_key(&mut self, _: &Acc, _: &Int) -> bool {
            false
        }
        fn create(&mut self, _: Acc, _: Account) {}
        fn destroy(&mut self, _: &Acc) {}
        fn created(&self) -> Vec<Acc> {
            vec![]
        }
        fn destroyed(&self) -> Vec<Acc> {
            vec![]
        }
        fn head(&self, _: u64) -> Option<Head> {
            None
        }
        fn hash(&mut self, _: u64, _: Int) {}
        fn auth(&self, _: &Acc) -> Option<Acc> {
            None
        }
        fn log(&mut self, _: Buf, _: Vec<Int>) {}
        fn emit(&mut self, _: Event) -> usize {
            0
        }
        fn set_auth(&mut self, _: &Acc, _: &Acc) {}
        fn apply(&mut self) {}
    }
}
