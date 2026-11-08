use serde::{Deserialize, Serialize};
use yevm_misc::buf::Buf;

use std::sync::Arc;

use crate::{Acc, Head, Int, chain::Fetched, evm::JumpTable, trace::Event};

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
    /// Records a log emitted by `acc` (the executing contract, `ADDRESS` of the frame).
    fn log(&mut self, acc: &Acc, data: Buf, topics: Vec<Int>);

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

    fn save_fetched(&mut self, _fetched: Fetched, _millis: f64) {}
    fn next_fetched(&mut self) -> Option<Fetched> {
        None
    }
    fn prefetched(&mut self, _: Vec<Fetched>) {}
    fn is_offline(&self) -> bool {
        false
    }
    fn fetch_stats(&self) -> (u64, f64) {
        (0, 0.0)
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

    /// Whether trace-only events (reads, calls, returns, fees) are recorded; executors skip
    /// building them, and the copies they need, when this is `false`.
    fn records_trace(&self) -> bool {
        true
    }

    /// Returns a loaded slot's current and original values and whether it is cold, like
    /// [`Self::get`] followed by [`Self::is_cold_key`].
    fn get_with_warmth(&mut self, acc: &Acc, key: &Int) -> Option<(Int, Int, bool)> {
        let (current, original) = self.get(acc, key)?;
        Some((current, original, self.is_cold_key(acc, key)))
    }

    /// Returns a loaded slot's current value and warms it, returning whether it was cold, like
    /// [`Self::get`] followed by [`Self::warm_key`].
    fn get_warm(&mut self, acc: &Acc, key: &Int) -> Option<(Int, bool)> {
        let (current, _) = self.get(acc, key)?;
        Some((current, self.warm_key(acc, key)))
    }

    /// Writes a slot and warms it, like [`Self::warm_key`] followed by [`Self::put`].
    fn put_warm(&mut self, acc: &Acc, key: &Int, val: Int) -> Option<Int> {
        self.warm_key(acc, key);
        self.put(acc, key, val)
    }

    /// Whether `acc` is loaded. Unlike [`Self::acc`], copies nothing.
    fn has_acc(&mut self, acc: &Acc) -> bool {
        self.acc(acc).is_some()
    }

    /// The size of `acc`'s code, if loaded. Unlike [`Self::code`], copies nothing.
    fn code_size(&mut self, acc: &Acc) -> Option<usize> {
        self.code(acc).map(|(code, _)| code.len())
    }

    /// The hash of `acc`'s code, if loaded. Unlike [`Self::code`], copies nothing.
    fn code_hash(&mut self, acc: &Acc) -> Option<Int> {
        self.code(acc).map(|(_, hash)| hash)
    }

    /// `acc`'s code as bytes shared with every frame that runs it, with its hash. States may
    /// cache it by hash.
    fn frame_code(&mut self, acc: &Acc) -> Option<(Arc<[u8]>, Int)> {
        self.code(acc)
            .map(|(code, hash)| (code.into_vec().into(), hash))
    }

    /// Returns the jump table of `code`, whose hash is `hash`. States may cache it by hash.
    fn jump_table(&mut self, _hash: &Int, code: &[u8]) -> Arc<JumpTable> {
        Arc::new(JumpTable::new(code))
    }
}
