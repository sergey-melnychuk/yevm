use serde::{Deserialize, Serialize};
use yaevmi_misc::buf::Buf;

use crate::{Acc, Head, Int, chain::Fetched, trace::Event};

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct Account {
    pub value: Int,
    pub nonce: Int,
    pub code: (Buf, Int),
}

pub trait State {
    fn get(&mut self, acc: &Acc, key: &Int) -> Option<(Int, Int)>;
    fn put(&mut self, acc: &Acc, key: &Int, val: Int) -> Option<Int>;
    fn init(&mut self, acc: &Acc, key: &Int, val: Int);

    fn tget(&mut self, acc: &Acc, key: &Int) -> Option<Int>;
    fn tput(&mut self, acc: Acc, key: Int, val: Int) -> Option<Int>;

    fn inc_nonce(&mut self, acc: &Acc, nonce: Int) -> Int;
    fn set_value(&mut self, acc: &Acc, value: Int) -> Int;
    fn set_code(&mut self, acc: &Acc, code: Buf, hash: Int) -> Int;
    fn set_auth(&mut self, src: &Acc, dst: &Acc);

    fn merge(&mut self, acc: &Acc, chain: Account);

    fn balance(&mut self, acc: &Acc) -> Option<Int>;
    fn nonce(&mut self, acc: &Acc) -> Option<Int>;
    fn code(&mut self, acc: &Acc) -> Option<(Buf, Int)>;
    fn acc(&mut self, acc: &Acc) -> Option<Account>;

    fn is_cold_acc(&self, acc: &Acc) -> bool;
    fn is_cold_key(&self, acc: &Acc, key: &Int) -> bool;

    fn warm_acc(&mut self, acc: &Acc) -> bool;
    fn warm_key(&mut self, acc: &Acc, key: &Int) -> bool;

    fn create(&mut self, acc: Acc, info: Account);
    fn destroy(&mut self, acc: &Acc);

    fn hash(&mut self, number: u64, hash: Int);
    fn log(&mut self, data: Buf, topics: Vec<Int>);

    fn head(&self, number: u64) -> Option<Head>;
    fn auth(&self, acc: &Acc) -> Option<Acc>;
    fn created(&self) -> Vec<Acc>;
    fn destroyed(&self) -> Vec<Acc>;
    fn apply(&mut self);

    fn get_depth(&mut self) -> usize {
        0
    }
    fn set_depth(&mut self, _depth: usize) {}
    fn emit(&mut self, event: Event) -> usize;

    fn save_fetched(&mut self, _fetched: Fetched) {}
    fn next_fetched(&mut self) -> Option<Fetched> {
        None
    }
    fn prefetched(&mut self, _: Vec<Fetched>) {}
    fn is_offline(&self) -> bool {
        false
    }

    fn reset(&mut self) {}

    /// Take a state checkpoint; returns an opaque ID that can be passed to `revert_to`.
    fn checkpoint(&mut self) -> usize {
        0
    }
    /// Revert all state mutations since the given checkpoint.
    fn revert_to(&mut self, _checkpoint: usize) {}

    fn set_chain_id(&mut self, _id: u64) {}
    fn get_chain_id(&self) -> u64 {
        0
    }

    fn is_tracing(&self) -> bool {
        true
    }
}
