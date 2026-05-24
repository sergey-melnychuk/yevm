use serde::{Deserialize, Serialize};
use yevm_base::{Acc, Int};
use yevm_misc::buf::Buf;

use crate::{
    Call,
    evm::{CallMode, HaltReason},
};

pub mod filter {
    pub const NONE: u32 = 0;
    pub const STEP: u32 = 1 << 0;
    pub const CALL: u32 = 1 << 1;
    pub const RETURN: u32 = 1 << 2;
    pub const REVERT: u32 = 1 << 3;
    pub const HALT: u32 = 1 << 4;
    pub const GET: u32 = 1 << 5;
    pub const PUT: u32 = 1 << 6;
    pub const LOG: u32 = 1 << 7;
    pub const CREATE: u32 = 1 << 8;
    pub const DELETE: u32 = 1 << 9;
    pub const MOVE: u32 = 1 << 10;
    pub const FEE: u32 = 1 << 11;
    pub const HASH: u32 = 1 << 12;
    pub const CODE: u32 = 1 << 13;
    pub const ALL: u32 = u32::MAX;
    pub const TOP: u32 = ALL ^ STEP;
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Target {
    Nonce { acc: Acc, val: Int },
    Value { acc: Acc, val: Int },
    Store { acc: Acc, key: Int, val: Int },
    Temp { acc: Acc, key: Int, val: Int },
    Code { acc: Acc, hash: Int },
    Auth { acc: Acc },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Event {
    WarmKey(Acc, Int),
    WarmAcc(Acc),
    Move(Acc, Acc, Int),
    Get(Target),
    Put(Target, Int),
    Hash(Buf, Int),
    Code(Buf, Int),
    Log(Vec<Int>, Buf),
    Call(Call, CallMode),
    Return(Buf, u64),      // (data, gas_used)
    Revert(Buf, u64),      // (data, gas_used)
    Halt(HaltReason, u64), // (reason, gas_used)
    Undo(usize, usize),    // seq range [from, to) that was reverted
    Create(Acc),
    Delete(Acc),
    Fee(Acc, Acc, Int, Int, u64), // (sender, coinbase, base_burn, tip, gas_used)
    Blob(u64, Int),               // EIP-4844 BLOB carrying txs

    Step(Step),
    // Full(Step, Vec<Int>, Buf),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Step {
    pub pc: usize,
    pub op: u8,
    pub name: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Buf>,
    pub gas: u64,
    pub stack: usize,
    pub memory: usize,
    pub debug: Vec<String>,
}

impl Event {
    pub fn filter_bit(&self) -> u32 {
        match self {
            Event::Step(_) => filter::STEP,
            Event::Call(..) => filter::CALL,
            Event::Return(..) => filter::RETURN,
            Event::Revert(..) => filter::REVERT,
            Event::Undo(..) => filter::REVERT,
            Event::Halt(..) => filter::HALT,
            Event::Get(_) => filter::GET,
            Event::Put(..) => filter::PUT,
            Event::Log(..) => filter::LOG,
            Event::Create(_) => filter::CREATE,
            Event::Delete(_) => filter::DELETE,
            Event::Move(..) => filter::MOVE,
            Event::Fee(..) => filter::FEE,
            Event::Hash(..) => filter::HASH,
            Event::Code(..) => filter::CODE,
            _ => filter::NONE,
        }
    }
}

impl PartialEq for Step {
    fn eq(&self, other: &Self) -> bool {
        self.pc == other.pc
            && self.op == other.op
            && self.name == other.name
            && self.data == other.data
            && self.gas == other.gas
            // revm leaves/pops stack values on halt depending on opcode
            // && self.stack == other.stack
            && self.memory == other.memory
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Trace {
    pub seq: usize,
    pub event: Event,
    pub depth: usize,
    pub reverted: bool,
}
