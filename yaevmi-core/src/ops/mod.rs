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

pub type Handler = fn(&mut Evm, &Context, &Call, &mut dyn State) -> EvmResult<()>;

pub fn invalid(evm: &mut Evm, _: &Context, _: &Call, _: &mut dyn State) -> EvmResult<()> {
    let op = evm.code.get(evm.pc).copied().unwrap_or_default();
    Err(EvmYield::Halt(HaltReason::BadOpcode(op)))
}

pub fn text(code: &[u8]) -> Vec<String> {
    let mut ret = Vec::with_capacity(code.len());
    let mut pc = 0;
    while pc < code.len() {
        let op = code[pc];
        let (name, _) = OPS[op as usize];
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

pub const OPS: [(&str, Handler); 256] = [
    // 0x00
    ("STOP", basic::stop),
    ("ADD", basic::add),
    ("MUL", basic::mul),
    ("SUB", basic::sub),
    ("DIV", basic::div),
    ("SDIV", basic::sdiv),
    ("MOD", basic::r#mod),
    ("SMOD", basic::smod),
    ("ADDMOD", basic::addmod),
    ("MULMOD", basic::mulmod),
    ("EXP", basic::exp),
    ("SIGNEXTEND", basic::signextend),
    ("INVALID/0x0C", invalid),
    ("INVALID/0x0D", invalid),
    ("INVALID/0x0E", invalid),
    ("INVALID/0x0F", invalid),
    // 0x10
    ("LT", basic::lt),
    ("GT", basic::gt),
    ("SLT", basic::slt),
    ("SGT", basic::sgt),
    ("EQ", basic::eq),
    ("ISZERO", basic::iszero),
    ("AND", basic::and),
    ("OR", basic::or),
    ("XOR", basic::xor),
    ("NOT", basic::not),
    ("BYTE", basic::byte),
    ("SHL", basic::shl),
    ("SHR", basic::shr),
    ("SAR", basic::sar),
    ("CLZ", basic::clz), // TODO: FIXME: make it work for live & test
    // ("INVALID/0x1E", invalid), // CLZ is not in the Cancun spec
    ("INVALID/0x1F", invalid),
    // 0x20
    ("KECCAK256", basic::hash),
    ("INVALID/0x21", invalid),
    ("INVALID/0x22", invalid),
    ("INVALID/0x23", invalid),
    ("INVALID/0x24", invalid),
    ("INVALID/0x25", invalid),
    ("INVALID/0x26", invalid),
    ("INVALID/0x27", invalid),
    ("INVALID/0x28", invalid),
    ("INVALID/0x29", invalid),
    ("INVALID/0x2A", invalid),
    ("INVALID/0x2B", invalid),
    ("INVALID/0x2C", invalid),
    ("INVALID/0x2D", invalid),
    ("INVALID/0x2E", invalid),
    ("INVALID/0x2F", invalid),
    // 0x30
    ("ADDRESS", chain::address),
    ("BALANCE", chain::balance),
    ("ORIGIN", chain::origin),
    ("CALLER", chain::caller),
    ("CALLVALUE", chain::callvalue),
    ("CALLDATALOAD", chain::calldataload),
    ("CALLDATASIZE", chain::calldatasize),
    ("CALLDATACOPY", chain::calldatacopy),
    ("CODESIZE", chain::codesize),
    ("CODECOPY", chain::codecopy),
    ("GASPRICE", chain::gasprice),
    ("EXTCODESIZE", chain::extcodesize),
    ("EXTCODECOPY", chain::extcodecopy),
    ("RETURNDATASIZE", chain::returndatasize),
    ("RETURNDATACOPY", chain::returndatacopy),
    ("EXTCODEHASH", chain::extcodehash),
    // 0x40
    ("BLOCKHASH", chain::blockhash),
    ("COINBASE", chain::coinbase),
    ("TIMESTAMP", chain::timestamp),
    ("NUMBER", chain::number),
    /* ("PREVRANDAO", chain::prevrandao), */ ("DIFFICULTY", chain::prevrandao),
    ("GASLIMIT", chain::gaslimit),
    ("CHAINID", chain::chainid),
    ("SELFBALANCE", chain::selfbalance),
    ("BASEFEE", chain::basefee),
    ("BLOBHASH", chain::blobhash),
    ("BLOBBASEFEE", chain::blobbasefee),
    ("INVALID/0x4B", invalid),
    ("INVALID/0x4C", invalid),
    ("INVALID/0x4D", invalid),
    ("INVALID/0x4E", invalid),
    ("INVALID/0x4F", invalid),
    // 0x50
    ("POP", store::pop),
    ("MLOAD", store::mload),
    ("MSTORE", store::mstore),
    ("MSTORE8", store::mstore8),
    ("SLOAD", store::sload),
    ("SSTORE", store::sstore),
    ("JUMP", store::jump),
    ("JUMPI", store::jumpi),
    ("PC", store::pc),
    ("MSIZE", store::msize),
    ("GAS", store::gas),
    ("JUMPDEST", store::jumpdest),
    ("TLOAD", store::tload),
    ("TSTORE", store::tstore),
    ("MCOPY", store::mcopy),
    ("PUSH0", stack::push),
    // 0x60
    ("PUSH1", stack::push),
    ("PUSH2", stack::push),
    ("PUSH3", stack::push),
    ("PUSH4", stack::push),
    ("PUSH5", stack::push),
    ("PUSH6", stack::push),
    ("PUSH7", stack::push),
    ("PUSH8", stack::push),
    ("PUSH9", stack::push),
    ("PUSH10", stack::push),
    ("PUSH11", stack::push),
    ("PUSH12", stack::push),
    ("PUSH13", stack::push),
    ("PUSH14", stack::push),
    ("PUSH15", stack::push),
    ("PUSH16", stack::push),
    // 0x70
    ("PUSH17", stack::push),
    ("PUSH18", stack::push),
    ("PUSH19", stack::push),
    ("PUSH20", stack::push),
    ("PUSH21", stack::push),
    ("PUSH22", stack::push),
    ("PUSH23", stack::push),
    ("PUSH24", stack::push),
    ("PUSH25", stack::push),
    ("PUSH26", stack::push),
    ("PUSH27", stack::push),
    ("PUSH28", stack::push),
    ("PUSH29", stack::push),
    ("PUSH30", stack::push),
    ("PUSH31", stack::push),
    ("PUSH32", stack::push),
    // 0x80
    ("DUP1", stack::dup),
    ("DUP2", stack::dup),
    ("DUP3", stack::dup),
    ("DUP4", stack::dup),
    ("DUP5", stack::dup),
    ("DUP6", stack::dup),
    ("DUP7", stack::dup),
    ("DUP8", stack::dup),
    ("DUP9", stack::dup),
    ("DUP10", stack::dup),
    ("DUP11", stack::dup),
    ("DUP12", stack::dup),
    ("DUP13", stack::dup),
    ("DUP14", stack::dup),
    ("DUP15", stack::dup),
    ("DUP16", stack::dup),
    // 0x90
    ("SWAP1", stack::swap),
    ("SWAP2", stack::swap),
    ("SWAP3", stack::swap),
    ("SWAP4", stack::swap),
    ("SWAP5", stack::swap),
    ("SWAP6", stack::swap),
    ("SWAP7", stack::swap),
    ("SWAP8", stack::swap),
    ("SWAP9", stack::swap),
    ("SWAP10", stack::swap),
    ("SWAP11", stack::swap),
    ("SWAP12", stack::swap),
    ("SWAP13", stack::swap),
    ("SWAP14", stack::swap),
    ("SWAP15", stack::swap),
    ("SWAP16", stack::swap),
    // 0xA0
    ("LOG0", logs::log),
    ("LOG1", logs::log),
    ("LOG2", logs::log),
    ("LOG3", logs::log),
    ("LOG4", logs::log),
    ("INVALID/0xA5", invalid),
    ("INVALID/0xA6", invalid),
    ("INVALID/0xA7", invalid),
    ("INVALID/0xA8", invalid),
    ("INVALID/0xA9", invalid),
    ("INVALID/0xAA", invalid),
    ("INVALID/0xAB", invalid),
    ("INVALID/0xAC", invalid),
    ("INVALID/0xAD", invalid),
    ("INVALID/0xAE", invalid),
    ("INVALID/0xAF", invalid),
    // 0xB0
    ("INVALID/0xB0", invalid),
    ("INVALID/0xB1", invalid),
    ("INVALID/0xB2", invalid),
    ("INVALID/0xB3", invalid),
    ("INVALID/0xB4", invalid),
    ("INVALID/0xB5", invalid),
    ("INVALID/0xB6", invalid),
    ("INVALID/0xB7", invalid),
    ("INVALID/0xB8", invalid),
    ("INVALID/0xB9", invalid),
    ("INVALID/0xBA", invalid),
    ("INVALID/0xBB", invalid),
    ("INVALID/0xBC", invalid),
    ("INVALID/0xBD", invalid),
    ("INVALID/0xBE", invalid),
    ("INVALID/0xBF", invalid),
    // 0xC0
    ("INVALID/0xC0", invalid),
    ("INVALID/0xC1", invalid),
    ("INVALID/0xC2", invalid),
    ("INVALID/0xC3", invalid),
    ("INVALID/0xC4", invalid),
    ("INVALID/0xC5", invalid),
    ("INVALID/0xC6", invalid),
    ("INVALID/0xC7", invalid),
    ("INVALID/0xC8", invalid),
    ("INVALID/0xC9", invalid),
    ("INVALID/0xCA", invalid),
    ("INVALID/0xCB", invalid),
    ("INVALID/0xCC", invalid),
    ("INVALID/0xCD", invalid),
    ("INVALID/0xCE", invalid),
    ("INVALID/0xCF", invalid),
    // 0xD0
    ("INVALID/0xD0", invalid),
    ("INVALID/0xD1", invalid),
    ("INVALID/0xD2", invalid),
    ("INVALID/0xD3", invalid),
    ("INVALID/0xD4", invalid),
    ("INVALID/0xD5", invalid),
    ("INVALID/0xD6", invalid),
    ("INVALID/0xD7", invalid),
    ("INVALID/0xD8", invalid),
    ("INVALID/0xD9", invalid),
    ("INVALID/0xDA", invalid),
    ("INVALID/0xDB", invalid),
    ("INVALID/0xDC", invalid),
    ("INVALID/0xDD", invalid),
    ("INVALID/0xDE", invalid),
    ("INVALID/0xDF", invalid),
    // 0xE0
    ("INVALID/0xE0", invalid),
    ("INVALID/0xE1", invalid),
    ("INVALID/0xE2", invalid),
    ("INVALID/0xE3", invalid),
    ("INVALID/0xE4", invalid),
    ("INVALID/0xE5", invalid),
    ("INVALID/0xE6", invalid),
    ("INVALID/0xE7", invalid),
    ("INVALID/0xE8", invalid),
    ("INVALID/0xE9", invalid),
    ("INVALID/0xEA", invalid),
    ("INVALID/0xEB", invalid),
    ("INVALID/0xEC", invalid),
    ("INVALID/0xED", invalid),
    ("INVALID/0xEE", invalid),
    ("INVALID/0xEF", invalid),
    // 0xF0
    ("CREATE", calls::create),
    ("CALL", calls::call),
    ("CALLCODE", calls::callcode),
    ("RETURN", calls::r#return),
    ("DELEGATECALL", calls::delegatecall),
    ("CREATE2", calls::create2),
    ("INVALID/0xF6", invalid),
    ("INVALID/0xF7", invalid),
    ("INVALID/0xF8", invalid),
    ("INVALID/0xF9", invalid),
    ("STATICCALL", calls::staticcall),
    ("INVALID/0xFB", invalid),
    ("INVALID/0xFC", invalid),
    ("REVERT", calls::revert),
    ("INVALID/0xFE", invalid),
    ("SELFDESTRUCT", calls::selfdestruct),
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
            to: Acc::ZERO,
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
