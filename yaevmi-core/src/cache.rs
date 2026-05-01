use ahash::{AHashMap as HashMap, AHashSet as HashSet};

use crate::{
    call::Head,
    chain::Fetched,
    state::{Account, State},
    trace::{Event, Target, Trace, filter},
};
use futures::channel::mpsc;
use yaevmi_base::{Acc, Int, math::lift};
use yaevmi_misc::buf::Buf;

#[derive(Default)]
struct Slot {
    original: Int,
    current: Int,
}

struct AccountEntry {
    account: Account,
    storage: HashMap<Int, Slot>,
}

impl AccountEntry {
    fn new(account: Account) -> Self {
        Self {
            account,
            storage: HashMap::new(),
        }
    }
}

impl Default for AccountEntry {
    fn default() -> Self {
        Self {
            account: Account {
                value: Int::ZERO,
                nonce: Int::ZERO,
                code: (vec![].into(), Int::ZERO),
            },
            storage: HashMap::new(),
        }
    }
}

enum Revert {
    WarmAcc(Acc),
    WarmKey(Acc, Int),
    Store(Acc, Int, Int),
    Nonce(Acc, Int),
    Value(Acc, Int),
    Temp(Acc, Int, Int),
    Code(Acc, Int),
    Create(Acc),
    Delete(Acc),
}

#[derive(Default)]
pub struct Cache {
    accounts: HashMap<Acc, AccountEntry>,
    transient: HashMap<(Acc, Int), Int>,
    warm_accs: HashSet<Acc>,
    warm_keys: HashSet<(Acc, Int)>,
    created: HashSet<Acc>,
    destroyed: HashSet<Acc>,
    hash: HashMap<u64, Int>,
    depth: usize,
    pub logs: Vec<(Buf, Vec<Int>)>,
    pub events: Vec<Trace>,
    pub sender: Option<mpsc::Sender<Trace>>,
    pub filter: u32,
    pub fetched: Vec<Fetched>,
    pub index: usize,
    offline: bool,
    chain_id: u64,
}

impl Cache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_sender(sender: mpsc::Sender<Trace>, filter: u32) -> Self {
        Self {
            sender: Some(sender),
            filter,
            ..Self::default()
        }
    }

    pub fn insert_account(&mut self, addr: Acc, account: Account) {
        let entry = self.accounts.entry(addr).or_default();
        entry.account = account;
    }

    pub fn account(&self, addr: &Acc) -> Option<&Account> {
        self.accounts.get(addr).map(|e| &e.account)
    }

    pub fn storage(&self, addr: &Acc, key: &Int) -> Option<Int> {
        self.accounts.get(addr)?.storage.get(key).map(|s| s.current)
    }

    pub fn reset(&mut self) {
        self.transient.clear();
        self.warm_accs.clear();
        self.warm_keys.clear();
        self.created.clear();
        self.destroyed.clear();
        self.depth = 0;
        self.logs.clear();
        self.events.clear();

        // TODO: FIXME: re-work original slot value tracking to avoid this
        for (_, account) in self.accounts.iter_mut() {
            for (_, slot) in account.storage.iter_mut() {
                slot.original = slot.current;
            }
        }
    }
}

impl State for Cache {
    // Storage: returns (current, original)
    fn get(&mut self, acc: &Acc, key: &Int) -> Option<(Int, Int)> {
        let entry = self.accounts.get(acc)?;
        let (cur, org) = entry.storage.get(key).map(|s| (s.current, s.original))?;
        self.emit(Event::Get(Target::Store {
            acc: *acc,
            key: *key,
            val: cur,
        }));
        Some((cur, org))
    }

    fn put(&mut self, acc: &Acc, key: &Int, val: Int) -> Option<Int> {
        let entry = self.accounts.entry(*acc).or_default();
        let slot = entry.storage.entry(*key).or_default();
        let prev = slot.current;
        slot.current = val;
        self.emit(Event::Put(
            Target::Store {
                acc: *acc,
                key: *key,
                val: prev,
            },
            val,
        ));
        Some(prev)
    }

    fn init(&mut self, acc: &Acc, key: &Int, val: Int) {
        let entry = self.accounts.entry(*acc).or_default();
        entry.storage.insert(
            *key,
            Slot {
                original: val,
                current: val,
            },
        );
    }

    fn tget(&mut self, acc: &Acc, key: &Int) -> Option<Int> {
        let val = self.transient.get(&(*acc, *key)).copied();
        self.emit(Event::Get(Target::Temp {
            acc: *acc,
            key: *key,
            val: val.unwrap_or_default(),
        }));
        val
    }

    fn tput(&mut self, acc: Acc, key: Int, val: Int) -> Option<Int> {
        let prev = self.transient.insert((acc, key), val);
        self.emit(Event::Put(
            Target::Temp {
                acc,
                key,
                val: prev.unwrap_or_default(),
            },
            val,
        ));
        prev
    }

    // Account mutations
    fn inc_nonce(&mut self, acc: &Acc, by: Int) -> Int {
        let (old, new) = {
            let entry = self.accounts.entry(*acc).or_default();
            let old = entry.account.nonce;
            let f = lift(|[a, b]| a + b);
            let new = f([old, by]);
            entry.account.nonce = new;
            (old, new)
        };
        self.emit(Event::Put(
            Target::Nonce {
                acc: *acc,
                val: old,
            },
            new,
        ));
        new
    }

    fn set_value(&mut self, acc: &Acc, value: Int) -> Int {
        let prev = {
            let entry = self.accounts.entry(*acc).or_default();
            let prev = entry.account.value;
            entry.account.value = value;
            prev
        };
        self.emit(Event::Put(
            Target::Value {
                acc: *acc,
                val: prev,
            },
            value,
        ));
        prev
    }

    fn set_code(&mut self, acc: &Acc, code: Buf, hash: Int) -> Int {
        let prev = {
            let entry = self.accounts.entry(*acc).or_default();
            let (_, prev) = entry.account.code;
            entry.account.code = (code, hash);
            prev
        };
        self.emit(Event::Put(
            Target::Code {
                acc: *acc,
                hash: prev,
            },
            hash,
        ));
        // TODO: use code cache for by-hash lookups
        prev
    }

    fn set_auth(&mut self, src: &Acc, dst: &Acc) {
        let mut code = vec![0; 23];
        // EIP-7702 delegation designator: 0xef0100 || address (20 bytes)
        code[..3].copy_from_slice(&[0xEF, 0x01, 0x00]);
        code[3..].copy_from_slice(dst.as_ref());
        self.set_code(src, code.into(), Int::ZERO);
    }

    fn merge(&mut self, acc: &Acc, chain: Account) {
        self.accounts.entry(*acc).or_default().account = chain;
    }

    fn balance(&mut self, acc: &Acc) -> Option<Int> {
        let val = self.accounts.get(acc).map(|e| e.account.value);
        self.emit(Event::Get(Target::Value {
            acc: *acc,
            val: val.unwrap_or_default(),
        }));
        val
    }

    fn nonce(&mut self, acc: &Acc) -> Option<Int> {
        let val = self.accounts.get(acc).map(|e| e.account.nonce);
        self.emit(Event::Get(Target::Nonce {
            acc: *acc,
            val: val.unwrap_or_default(),
        }));
        val
    }

    fn code(&mut self, acc: &Acc) -> Option<(Buf, Int)> {
        let (code, hash) = self.accounts.get(acc).map(|e| e.account.code.clone())?;
        self.emit(Event::Get(Target::Code { acc: *acc, hash }));
        Some((code, hash))
    }

    fn acc(&mut self, acc: &Acc) -> Option<Account> {
        self.accounts.get(acc).map(|e| Account {
            value: e.account.value,
            nonce: e.account.nonce,
            code: e.account.code.clone(),
        })
    }

    fn is_cold_acc(&self, acc: &Acc) -> bool {
        !self.warm_accs.contains(acc)
    }

    fn is_cold_key(&self, acc: &Acc, key: &Int) -> bool {
        !self.warm_keys.contains(&(*acc, *key))
    }

    fn warm_acc(&mut self, acc: &Acc) -> bool {
        let cold = self.warm_accs.insert(*acc);
        if cold {
            self.emit(Event::WarmAcc(*acc));
        }
        cold
    }

    fn warm_key(&mut self, acc: &Acc, key: &Int) -> bool {
        let cold = self.warm_keys.insert((*acc, *key));
        if cold {
            self.emit(Event::WarmKey(*acc, *key));
        }
        cold
    }

    fn create(&mut self, acc: Acc, info: Account) {
        if !info.nonce.is_zero() {
            self.emit(Event::Put(Target::Nonce { acc, val: Int::ZERO }, info.nonce));
        }
        if !info.value.is_zero() {
            self.emit(Event::Put(Target::Value { acc, val: Int::ZERO }, info.value));
        }
        self.emit(Event::Create(acc));
        self.accounts.insert(acc, AccountEntry::new(info));
        self.created.insert(acc);
    }

    fn destroy(&mut self, acc: &Acc) {
        self.destroyed.insert(*acc);
        self.emit(Event::Delete(*acc));
    }

    fn created(&self) -> Vec<Acc> {
        self.created.iter().cloned().collect()
    }

    fn destroyed(&self) -> Vec<Acc> {
        self.destroyed.iter().cloned().collect()
    }

    fn head(&self, number: u64) -> Option<Head> {
        self.hash.get(&number).map(|&hash| Head {
            number: number.into(),
            hash,
            ..Head::default()
        })
    }

    fn hash(&mut self, number: u64, hash: Int) {
        self.hash.insert(number, hash);
    }

    fn auth(&self, acc: &Acc) -> Option<Acc> {
        let entry = self.accounts.get(acc)?;
        let (code, _) = &entry.account.code;
        if code.0.len() == 23 && code.0.starts_with(&[0xEF, 0x01, 0x00]) {
            let acc = Acc::from(&code.0[3..]);
            if acc.is_zero() { None } else { Some(acc) }
        } else {
            None
        }
    }

    fn log(&mut self, data: Buf, topics: Vec<Int>) {
        self.emit(Event::Log(topics.clone(), data.clone()));
        self.logs.push((data, topics));
    }

    fn set_depth(&mut self, depth: usize) {
        self.depth = depth;
    }

    fn emit(&mut self, mut event: Event) -> usize {
        let id = self.events.len();
        if let Event::Step(step) = &mut event {
            step.debug.push(format!("depth={}", self.depth));
        }
        let trace = Trace {
            seq: id,
            event,
            depth: self.depth,
            reverted: false,
        };
        if self.filter & trace.event.filter_bit() != 0 {
            if let Some(sender) = self.sender.as_mut() {
                let _ = sender.try_send(trace.clone());
            }
        }
        self.events.push(trace);
        id
    }

    fn save_fetched(&mut self, fetched: Fetched) {
        self.fetched.push(fetched);
    }

    fn next_fetched(&mut self) -> Option<Fetched> {
        self.index += 1;
        self.fetched.get(self.index).cloned()
    }

    fn prefetched(&mut self, fetched: Vec<Fetched>) {
        self.fetched = fetched;
        self.offline = true;
        self.index = 1; // skip chain id & block
    }

    fn is_offline(&self) -> bool {
        self.offline
    }

    fn checkpoint(&mut self) -> usize {
        self.events.len()
    }

    fn revert_to(&mut self, cp: usize) {
        if cp >= self.events.len() {
            return;
        }
        let to = self.events.len();

        // Count logs added since checkpoint (to truncate self.logs)
        let logs_to_remove = self.events[cp..]
            .iter()
            .filter(|t| !t.reverted)
            .filter(|t| matches!(t.event, Event::Log(..)))
            .count();
        self.logs
            .truncate(self.logs.len().saturating_sub(logs_to_remove));

        // Collect undo operations (immutable borrow of events ends here)
        let undos: Vec<Revert> = self.events[cp..]
            .iter()
            .filter(|t| !t.reverted)
            .filter_map(|trace| match &trace.event {
                Event::Put(Target::Store { acc, key, val }, _) => {
                    Some(Revert::Store(*acc, *key, *val))
                }
                Event::Put(Target::Nonce { acc, val }, _) => Some(Revert::Nonce(*acc, *val)),
                Event::Put(Target::Value { acc, val }, _) => Some(Revert::Value(*acc, *val)),
                Event::Put(Target::Temp { acc, key, val }, _) => {
                    Some(Revert::Temp(*acc, *key, *val))
                }
                Event::Put(Target::Code { acc, hash }, _) => Some(Revert::Code(*acc, *hash)),
                Event::WarmAcc(acc) => Some(Revert::WarmAcc(*acc)),
                Event::WarmKey(acc, key) => Some(Revert::WarmKey(*acc, *key)),
                Event::Create(acc) => Some(Revert::Create(*acc)),
                Event::Delete(acc) => Some(Revert::Delete(*acc)),
                _ => None,
            })
            .collect();

        // Mark all reverted traces
        for t in &mut self.events[cp..] {
            t.reverted = true;
        }

        // Apply undos in reverse order
        for undo in undos.into_iter().rev() {
            match undo {
                Revert::Store(acc, key, val) => {
                    if let Some(entry) = self.accounts.get_mut(&acc)
                        && let Some(slot) = entry.storage.get_mut(&key)
                    {
                        slot.current = val;
                    }
                }
                Revert::Nonce(acc, val) => {
                    if let Some(entry) = self.accounts.get_mut(&acc) {
                        entry.account.nonce = val;
                    }
                }
                Revert::Value(acc, val) => {
                    if let Some(entry) = self.accounts.get_mut(&acc) {
                        entry.account.value = val;
                    }
                }
                Revert::Temp(acc, key, val) => {
                    if val.is_zero() {
                        self.transient.remove(&(acc, key));
                    } else {
                        self.transient.insert((acc, key), val);
                    }
                }
                Revert::Code(acc, _prev_hash) => {
                    if let Some(entry) = self.accounts.get_mut(&acc) {
                        entry.account.code = (Buf::default(), Int::ZERO);
                    }
                }
                Revert::Create(acc) => {
                    self.created.remove(&acc);
                    self.accounts.remove(&acc);
                }
                Revert::Delete(acc) => {
                    self.destroyed.remove(&acc);
                }
                Revert::WarmAcc(acc) => {
                    self.warm_accs.remove(&acc);
                }
                Revert::WarmKey(acc, key) => {
                    self.warm_keys.remove(&(acc, key));
                }
            }
        }

        if self.filter & filter::REVERT != 0 {
            if let Some(sender) = self.sender.as_mut() {
                let _ = sender.try_send(Trace {
                    seq: cp,
                    event: Event::Undo(cp, to),
                    depth: self.depth,
                    reverted: true,
                });
            }
        }
    }

    fn apply(&mut self) {
        let destroyed = std::mem::take(&mut self.destroyed);
        for acc in destroyed {
            if let Some(entry) = self.accounts.get_mut(&acc) {
                // do not reset nonce for self-destructed contracts
                entry.account.value = Int::ZERO;
                entry.account.code = (Buf::default(), Int::ZERO);
                entry.storage.clear();
            }
            self.created.remove(&acc);
        }
    }

    fn set_chain_id(&mut self, id: u64) {
        self.chain_id = id;
    }

    fn get_chain_id(&self) -> u64 {
        self.chain_id
    }

    fn is_tracing(&self) -> bool {
        self.sender.is_some()
    }
}

pub type Env = Vec<(Acc, Account, Vec<(Int, Int)>)>;

impl Cache {
    pub fn snapshot(&self) -> Env {
        let mut ret = Vec::with_capacity(self.accounts.len());
        for (acc, entry) in &self.accounts {
            let mut kv = Vec::with_capacity(entry.storage.len());
            for (key, slot) in &entry.storage {
                kv.push((*key, slot.current));
            }
            kv.sort_by_key(|(k, _)| *k);
            ret.push((*acc, entry.account.clone(), kv));
        }
        ret.sort_by_key(|(acc, _, _)| *acc);
        ret
    }
}
