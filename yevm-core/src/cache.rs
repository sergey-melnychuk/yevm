use foldhash::{HashMap, HashMapExt, HashSet};

use std::sync::Arc;

use crate::{
    call::Head,
    chain::Fetched,
    evm::JumpTable,
    misc::is_precompile,
    state::{Account, State},
    trace::{Event, Target, Trace, filter},
};
use futures::channel::mpsc;
use yevm_base::{Acc, Int, math::lift};
use yevm_misc::buf::Buf;

/// A storage slot touched by execution.
#[derive(Clone, Copy, Debug, Default)]
pub struct Slot {
    /// Value at the start of the transaction (as fetched from the chain).
    pub original: Int,
    /// Value after the mutations applied so far.
    pub current: Int,
    /// Whether the slot was accessed in the current transaction (EIP-2929).
    pub warm: bool,
}

struct AccountEntry {
    account: Account,
    storage: HashMap<Int, Slot>,
    /// Whether the account's storage started empty here, because the account was created (or
    /// destroyed) in this cache: a slot missing from `storage` is then zero and is never loaded.
    /// The chain may still hold storage at a created address (the collisions of EIP-7610), but a
    /// creation discards it, as it does in the spec's `process_create_message`.
    fresh: bool,
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
            fresh: false,
        }
    }
}

/// An undo record: what a journaled change replaced.
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
    Log,
}

impl Revert {
    /// Returns the undo record of a state-changing event, or `None` for other events.
    fn of(event: &Event) -> Option<Self> {
        Some(match event {
            Event::Put(Target::Store { acc, key, val }, _) => Self::Store(*acc, *key, *val),
            Event::Put(Target::Nonce { acc, val }, _) => Self::Nonce(*acc, *val),
            Event::Put(Target::Value { acc, val }, _) => Self::Value(*acc, *val),
            Event::Put(Target::Temp { acc, key, val }, _) => Self::Temp(*acc, *key, *val),
            Event::Put(Target::Code { acc, hash }, _) => Self::Code(*acc, *hash),
            Event::WarmAcc(acc) => Self::WarmAcc(*acc),
            Event::WarmKey(acc, key) => Self::WarmKey(*acc, *key),
            Event::Create(acc) => Self::Create(*acc),
            Event::Delete(acc) => Self::Delete(*acc),
            Event::Log(..) => Self::Log,
            _ => return None,
        })
    }
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
    /// Logs emitted so far, as `(address, data, topics)`; reverted frames' logs are removed.
    pub logs: Vec<(Acc, Buf, Vec<Int>)>,
    pub events: Vec<Trace>,
    pub sender: Option<mpsc::Sender<Trace>>,
    pub filter: u32,
    pub fetched: Vec<Fetched>,
    pub index: usize,
    offline: bool,
    chain_id: u64,
    total_fetch_calls: u64,
    total_fetch_millis: f64,
    /// See [`Cache::set_journal_only`].
    journal_only: bool,
    /// Jump tables by code hash. Code is content-addressed, so [`Cache::clear`] keeps them.
    jump_tables: HashMap<Int, Arc<JumpTable>>,
    /// Shared code by code hash, kept by [`Cache::clear`] like `jump_tables`.
    codes: HashMap<Int, Arc<[u8]>>,
    /// Empty storage maps of cleared accounts, reused to avoid regrowing them per transaction.
    spare_storage: Vec<HashMap<Int, Slot>>,
    /// The journal when no trace is recorded (see [`Self::set_journal_only`]): undo records
    /// only, while `events` stays empty.
    undo: Vec<Revert>,
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

    /// Iterates over every account loaded or modified so far, with its current state and the
    /// storage slots it touched.
    ///
    /// Accounts destroyed by `State::apply` are reported as empty accounts. Embedders use this
    /// to extract the state diff of an executed transaction.
    pub fn entries(&self) -> impl Iterator<Item = (&Acc, &Account, &HashMap<Int, Slot>)> {
        self.accounts
            .iter()
            .map(|(acc, entry)| (acc, &entry.account, &entry.storage))
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
        self.undo.clear();

        // NOTE: profiling shows 0.7%, not a bottleneck, leaving as is
        for account in self.accounts.values_mut() {
            for slot in account.storage.values_mut() {
                slot.original = slot.current;
                slot.warm = false;
            }
        }

        self.total_fetch_calls = 0;
        self.total_fetch_millis = 0.0;
    }

    /// Without a trace sender, journals only what undo needs (state writes, warm-ups, creations,
    /// deletions and log markers) as compact undo records, and records no events: reads, calls,
    /// returns and log contents are not copied into [`Self::events`]. This makes execution
    /// considerably cheaper for embedders that only need the resulting state.
    pub fn set_journal_only(&mut self, journal_only: bool) {
        self.journal_only = journal_only;
    }

    /// Forgets all state and journal entries, like [`Self::purge`], but keeps the allocated
    /// capacity and the settings (sender, filter, chain id, journal mode) for reuse.
    pub fn clear(&mut self) {
        for entry in self.accounts.values_mut() {
            recycle(&mut self.spare_storage, &mut entry.storage);
        }
        self.accounts.clear();
        self.transient.clear();
        self.warm_accs.clear();
        self.warm_keys.clear();
        self.created.clear();
        self.destroyed.clear();
        self.hash.clear();
        self.depth = 0;
        self.logs.clear();
        self.events.clear();
        self.undo.clear();
        self.fetched.clear();
        self.index = 0;
        self.total_fetch_calls = 0;
        self.total_fetch_millis = 0.0;
    }

    /// Journals a state change: as `undo` when no trace is recorded, otherwise as `event`.
    fn journal(&mut self, undo: Revert, event: impl FnOnce() -> Event) {
        if self.records_trace() {
            self.emit(event());
        } else {
            self.undo.push(undo);
        }
    }

    /// Records a trace-only event, built only when traces are recorded.
    fn trace(&mut self, event: impl FnOnce() -> Event) {
        if self.records_trace() {
            self.emit(event());
        }
    }

    /// Undoes one journaled change.
    fn revert(&mut self, undo: Revert) {
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
                if let Some(slot) = self
                    .accounts
                    .get_mut(&acc)
                    .and_then(|e| e.storage.get_mut(&key))
                {
                    slot.warm = false;
                }
                self.warm_keys.remove(&(acc, key));
            }
            Revert::Log => {
                self.logs.pop();
            }
        }
    }

    /// Loads a storage slot fetched from the chain, keeping its warmth if it was loaded before,
    /// and returns it.
    pub fn insert_slot(&mut self, acc: &Acc, key: &Int, val: Int) -> Slot {
        let entry = entry(
            &mut self.accounts,
            &mut self.spare_storage,
            *acc,
            Account::default,
        );
        let warm = match entry.storage.get(key) {
            Some(slot) => slot.warm,
            None => preloaded_warm(&self.warm_keys, acc, key),
        };
        let slot = Slot {
            original: val,
            current: val,
            warm,
        };
        entry.storage.insert(*key, slot);
        slot
    }

    /// Like [`State::get_warm`], but a missing slot of a loaded account is first loaded with the
    /// value `load` returns, like [`Self::insert_slot`]. Returns `None` only if `acc` is not
    /// loaded.
    pub fn get_warm_or_load(
        &mut self,
        acc: &Acc,
        key: &Int,
        load: impl FnOnce() -> Int,
    ) -> Option<(Int, bool)> {
        self.get_warm_with(acc, key, Some(load))
    }

    /// Like [`State::get_with_warmth`], but loads a missing slot like
    /// [`Self::get_warm_or_load`]. Returns `None` only if `acc` is not loaded.
    pub fn get_with_warmth_or_load(
        &mut self,
        acc: &Acc,
        key: &Int,
        load: impl FnOnce() -> Int,
    ) -> Option<(Int, Int, bool)> {
        self.get_with_warmth_with(acc, key, Some(load))
    }

    fn get_warm_with(
        &mut self,
        acc: &Acc,
        key: &Int,
        load: Option<impl FnOnce() -> Int>,
    ) -> Option<(Int, bool)> {
        let slot = slot(&mut self.accounts, &self.warm_keys, acc, key, load)?;
        let (current, cold) = (slot.current, !std::mem::replace(&mut slot.warm, true));
        self.trace(|| {
            Event::Get(Target::Store {
                acc: *acc,
                key: *key,
                val: current,
            })
        });
        if cold {
            self.journal(Revert::WarmKey(*acc, *key), || Event::WarmKey(*acc, *key));
        }
        Some((current, cold))
    }

    fn get_with_warmth_with(
        &mut self,
        acc: &Acc,
        key: &Int,
        load: Option<impl FnOnce() -> Int>,
    ) -> Option<(Int, Int, bool)> {
        let slot = slot(&mut self.accounts, &self.warm_keys, acc, key, load)?;
        let (current, original, cold) = (slot.current, slot.original, !slot.warm);
        self.trace(|| {
            Event::Get(Target::Store {
                acc: *acc,
                key: *key,
                val: current,
            })
        });
        Some((current, original, cold))
    }

    /// Like [`State::inc_nonce`], but an account that is not loaded is first loaded as the
    /// account `load` returns.
    pub fn inc_nonce_or_load(&mut self, acc: &Acc, by: Int, load: impl FnOnce() -> Account) -> Int {
        let (old, new) = {
            let entry = entry(&mut self.accounts, &mut self.spare_storage, *acc, load);
            let old = entry.account.nonce;
            let f = lift(|[a, b]| a + b);
            let new = f([old, by]);
            entry.account.nonce = new;
            (old, new)
        };
        self.journal(Revert::Nonce(*acc, old), || {
            Event::Put(
                Target::Nonce {
                    acc: *acc,
                    val: old,
                },
                new,
            )
        });
        new
    }

    /// Like [`State::set_value`], loading the account like [`Self::inc_nonce_or_load`].
    pub fn set_value_or_load(
        &mut self,
        acc: &Acc,
        value: Int,
        load: impl FnOnce() -> Account,
    ) -> Int {
        let prev = {
            let entry = entry(&mut self.accounts, &mut self.spare_storage, *acc, load);
            let prev = entry.account.value;
            entry.account.value = value;
            prev
        };
        self.journal(Revert::Value(*acc, prev), || {
            Event::Put(
                Target::Value {
                    acc: *acc,
                    val: prev,
                },
                value,
            )
        });
        prev
    }

    /// Like [`State::set_code`], loading the account like [`Self::inc_nonce_or_load`].
    pub fn set_code_or_load(
        &mut self,
        acc: &Acc,
        code: Buf,
        hash: Int,
        load: impl FnOnce() -> Account,
    ) -> Int {
        let prev = {
            let entry = entry(&mut self.accounts, &mut self.spare_storage, *acc, load);
            let (_, prev) = entry.account.code;
            entry.account.code = (code, hash);
            prev
        };
        self.journal(Revert::Code(*acc, prev), || {
            Event::Put(
                Target::Code {
                    acc: *acc,
                    hash: prev,
                },
                hash,
            )
        });
        // TODO: use code cache for by-hash lookups
        prev
    }

    pub fn purge(&mut self) {
        *self = Self {
            sender: self.sender.take(),
            filter: self.filter,
            chain_id: self.chain_id,
            offline: self.offline,
            journal_only: self.journal_only,
            ..Default::default()
        };
    }
}

/// Bound on [`Cache`]'s pool of spare storage maps.
const SPARE_STORAGE: usize = 256;

/// Returns `acc`'s entry, creating it with the account `load` returns and a storage map from
/// `spare` if needed.
fn entry<'a>(
    accounts: &'a mut HashMap<Acc, AccountEntry>,
    spare: &mut Vec<HashMap<Int, Slot>>,
    acc: Acc,
    load: impl FnOnce() -> Account,
) -> &'a mut AccountEntry {
    accounts.entry(acc).or_insert_with(|| AccountEntry {
        account: load(),
        storage: spare.pop().unwrap_or_default(),
        fresh: false,
    })
}

/// Empties `storage` and keeps it in `spare` for reuse, unless the pool is full.
fn recycle(spare: &mut Vec<HashMap<Int, Slot>>, storage: &mut HashMap<Int, Slot>) {
    if spare.len() < SPARE_STORAGE {
        storage.clear();
        spare.push(std::mem::take(storage));
    }
}

/// Returns the slot of `key` in `acc`'s storage. A missing slot is loaded with the value `load`
/// returns, or `None` is returned without `load`; `None` is also returned if `acc` is not loaded.
/// A missing slot of a fresh account (see [`AccountEntry::fresh`]) is zero, with or without
/// `load`.
fn slot<'a>(
    accounts: &'a mut HashMap<Acc, AccountEntry>,
    warm_keys: &HashSet<(Acc, Int)>,
    acc: &Acc,
    key: &Int,
    load: Option<impl FnOnce() -> Int>,
) -> Option<&'a mut Slot> {
    let AccountEntry { storage, fresh, .. } = accounts.get_mut(acc)?;
    let fresh = *fresh;
    if load.is_none() && !fresh {
        return storage.get_mut(key);
    }
    Some(storage.entry(*key).or_insert_with(|| {
        let value = match load {
            Some(load) if !fresh => load(),
            _ => Int::ZERO,
        };
        Slot {
            original: value,
            current: value,
            warm: preloaded_warm(warm_keys, acc, key),
        }
    }))
}

/// Whether `key` of `acc` was warmed before its slot was loaded (by an access list).
fn preloaded_warm(warm_keys: &HashSet<(Acc, Int)>, acc: &Acc, key: &Int) -> bool {
    !warm_keys.is_empty() && warm_keys.contains(&(*acc, *key))
}

impl State for Cache {
    // Storage: returns (current, original)
    fn get(&mut self, acc: &Acc, key: &Int) -> Option<(Int, Int)> {
        let entry = self.accounts.get(acc)?;
        let (cur, org) = match entry.storage.get(key) {
            Some(slot) => (slot.current, slot.original),
            None if entry.fresh => (Int::ZERO, Int::ZERO),
            None => return None,
        };
        self.trace(|| {
            Event::Get(Target::Store {
                acc: *acc,
                key: *key,
                val: cur,
            })
        });
        Some((cur, org))
    }

    fn put(&mut self, acc: &Acc, key: &Int, val: Int) -> Option<Int> {
        let entry = entry(
            &mut self.accounts,
            &mut self.spare_storage,
            *acc,
            Account::default,
        );
        let warm_keys = &self.warm_keys;
        let slot = entry.storage.entry(*key).or_insert_with(|| Slot {
            warm: preloaded_warm(warm_keys, acc, key),
            ..Slot::default()
        });
        let prev = slot.current;
        slot.current = val;
        self.journal(Revert::Store(*acc, *key, prev), || {
            Event::Put(
                Target::Store {
                    acc: *acc,
                    key: *key,
                    val: prev,
                },
                val,
            )
        });
        Some(prev)
    }

    fn init(&mut self, acc: &Acc, key: &Int, val: Int) {
        self.insert_slot(acc, key, val);
    }

    fn tget(&mut self, acc: &Acc, key: &Int) -> Option<Int> {
        let val = self.transient.get(&(*acc, *key)).copied();
        self.trace(|| {
            Event::Get(Target::Temp {
                acc: *acc,
                key: *key,
                val: val.unwrap_or_default(),
            })
        });
        val
    }

    fn tput(&mut self, acc: Acc, key: Int, val: Int) -> Option<Int> {
        let prev = self.transient.insert((acc, key), val);
        let old = prev.unwrap_or_default();
        self.journal(Revert::Temp(acc, key, old), || {
            Event::Put(Target::Temp { acc, key, val: old }, val)
        });
        prev
    }

    // Account mutations
    fn inc_nonce(&mut self, acc: &Acc, by: Int) -> Int {
        self.inc_nonce_or_load(acc, by, Account::default)
    }

    fn set_value(&mut self, acc: &Acc, value: Int) -> Int {
        self.set_value_or_load(acc, value, Account::default)
    }

    fn set_code(&mut self, acc: &Acc, code: Buf, hash: Int) -> Int {
        self.set_code_or_load(acc, code, hash, Account::default)
    }

    fn set_auth(&mut self, src: &Acc, dst: &Acc) {
        let mut code = vec![0; 23];
        // EIP-7702 delegation designator: 0xef0100 || address (20 bytes)
        code[..3].copy_from_slice(&[0xEF, 0x01, 0x00]);
        code[3..].copy_from_slice(dst.as_ref());
        let hash = Int::from(yevm_misc::keccak256(&code).as_ref());
        self.set_code(src, code.into(), hash);
    }

    fn merge(&mut self, acc: &Acc, chain: Account) {
        entry(
            &mut self.accounts,
            &mut self.spare_storage,
            *acc,
            Account::default,
        )
        .account = chain;
    }

    fn balance(&mut self, acc: &Acc) -> Option<Int> {
        let val = self.accounts.get(acc).map(|e| e.account.value);
        self.trace(|| {
            Event::Get(Target::Value {
                acc: *acc,
                val: val.unwrap_or_default(),
            })
        });
        val
    }

    fn nonce(&mut self, acc: &Acc) -> Option<Int> {
        let val = self.accounts.get(acc).map(|e| e.account.nonce);
        self.trace(|| {
            Event::Get(Target::Nonce {
                acc: *acc,
                val: val.unwrap_or_default(),
            })
        });
        val
    }

    fn code(&mut self, acc: &Acc) -> Option<(Buf, Int)> {
        let (code, hash) = self.accounts.get(acc).map(|e| e.account.code.clone())?;
        self.trace(|| Event::Get(Target::Code { acc: *acc, hash }));
        if !code.is_empty() {
            self.trace(|| Event::Code(code.clone(), hash));
        }
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
        // Precompiles are warm from the start of every transaction (EIP-2929).
        !is_precompile(acc) && !self.warm_accs.contains(acc)
    }

    fn is_cold_key(&self, acc: &Acc, key: &Int) -> bool {
        match self.accounts.get(acc).and_then(|e| e.storage.get(key)) {
            Some(slot) => !slot.warm,
            None => !self.warm_keys.contains(&(*acc, *key)),
        }
    }

    fn warm_acc(&mut self, acc: &Acc) -> bool {
        let cold = self.warm_accs.insert(*acc);
        if cold {
            self.journal(Revert::WarmAcc(*acc), || Event::WarmAcc(*acc));
        }
        cold
    }

    fn warm_key(&mut self, acc: &Acc, key: &Int) -> bool {
        // Loaded slots carry their warmth; the set holds keys warmed before being loaded.
        let cold = match self
            .accounts
            .get_mut(acc)
            .and_then(|e| e.storage.get_mut(key))
        {
            Some(slot) => !std::mem::replace(&mut slot.warm, true),
            None => self.warm_keys.insert((*acc, *key)),
        };
        if cold {
            self.journal(Revert::WarmKey(*acc, *key), || Event::WarmKey(*acc, *key));
        }
        cold
    }

    fn create(&mut self, acc: Acc, info: Account) {
        if !info.nonce.is_zero() {
            self.journal(Revert::Nonce(acc, Int::ZERO), || {
                Event::Put(
                    Target::Nonce {
                        acc,
                        val: Int::ZERO,
                    },
                    info.nonce,
                )
            });
        }
        if !info.value.is_zero() {
            self.journal(Revert::Value(acc, Int::ZERO), || {
                Event::Put(
                    Target::Value {
                        acc,
                        val: Int::ZERO,
                    },
                    info.value,
                )
            });
        }
        self.journal(Revert::Create(acc), || Event::Create(acc));
        let storage = self.spare_storage.pop().unwrap_or_default();
        if let Some(mut old) = self.accounts.insert(
            acc,
            AccountEntry {
                account: info,
                storage,
                fresh: true,
            },
        ) {
            recycle(&mut self.spare_storage, &mut old.storage);
        }
        self.created.insert(acc);
    }

    fn destroy(&mut self, acc: &Acc) {
        // Journal only the first destruction: reverting a repeated one must not undo an
        // earlier destruction that is not being reverted.
        if self.destroyed.insert(*acc) {
            self.journal(Revert::Delete(*acc), || Event::Delete(*acc));
        }
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

    fn log(&mut self, acc: &Acc, data: Buf, topics: Vec<Int>) {
        self.journal(Revert::Log, || Event::Log(topics.clone(), data.clone()));
        self.logs.push((*acc, data, topics));
    }

    fn get_depth(&mut self) -> usize {
        self.depth
    }

    fn set_depth(&mut self, depth: usize) {
        self.depth = depth;
    }

    fn emit(&mut self, mut event: Event) -> usize {
        if !self.records_trace() {
            if let Some(undo) = Revert::of(&event) {
                self.undo.push(undo);
            }
            return self.undo.len();
        }
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
        if let Some(sender) = self.sender.as_mut()
            && self.filter & trace.event.filter_bit() != 0
        {
            // TODO: FIXME: can drop traces on full channel
            let _ = sender.try_send(trace.clone());
        }
        self.events.push(trace);
        id
    }

    fn save_fetched(&mut self, fetched: Fetched, millis: f64) {
        self.fetched.push(fetched);
        self.total_fetch_calls += 1;
        self.total_fetch_millis += millis;
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

    fn fetch_stats(&self) -> (u64, f64) {
        (self.total_fetch_calls, self.total_fetch_millis)
    }

    fn checkpoint(&mut self) -> usize {
        if self.records_trace() {
            self.events.len()
        } else {
            self.undo.len()
        }
    }

    fn revert_to(&mut self, cp: usize) {
        if !self.records_trace() {
            while self.undo.len() > cp {
                let Some(undo) = self.undo.pop() else { break };
                self.revert(undo);
            }
            return;
        }
        if cp >= self.events.len() {
            return;
        }
        let to = self.events.len();

        // Collect undo operations (immutable borrow of events ends here)
        let undos: Vec<Revert> = self.events[cp..]
            .iter()
            .filter(|t| !t.reverted)
            .filter_map(|trace| Revert::of(&trace.event))
            .collect();

        // Mark all reverted traces
        for t in &mut self.events[cp..] {
            t.reverted = true;
        }

        // Apply undos in reverse order
        for undo in undos.into_iter().rev() {
            self.revert(undo);
        }

        if let Some(sender) = self.sender.as_mut()
            && self.filter & filter::REVERT != 0
        {
            // TODO: FIXME: can drop traces on full channel
            let _ = sender.try_send(Trace {
                seq: cp,
                event: Event::Undo(cp, to),
                depth: self.depth,
                reverted: true,
            });
        }
    }

    fn apply(&mut self) {
        let destroyed = std::mem::take(&mut self.destroyed);
        for acc in destroyed {
            // EIP-6780 (Cancun): only contracts created in the same tx reach
            // `destroyed` (the SELFDESTRUCT op gates on created-in-same-tx),
            // and for those the account is deleted entirely -- balance, code,
            // storage, AND nonce. Keeping the nonce made a later CREATE2 at
            // the same address (same salt, next tx/block) hit the collision
            // check and drain all gas. Replace the entry with an explicitly
            // empty account rather than removing it, so follow-up reads see
            // the post-destruction state instead of refetching stale pre-tx
            // state from the chain backend.
            let storage = self.spare_storage.pop().unwrap_or_default();
            let empty = AccountEntry {
                account: AccountEntry::default().account,
                storage,
                fresh: true,
            };
            if let Some(mut old) = self.accounts.insert(acc, empty) {
                recycle(&mut self.spare_storage, &mut old.storage);
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

    fn records_trace(&self) -> bool {
        !self.journal_only || self.sender.is_some()
    }

    fn has_acc(&mut self, acc: &Acc) -> bool {
        self.accounts.contains_key(acc)
    }

    fn get_warm(&mut self, acc: &Acc, key: &Int) -> Option<(Int, bool)> {
        self.get_warm_with(acc, key, None::<fn() -> Int>)
    }

    fn put_warm(&mut self, acc: &Acc, key: &Int, val: Int) -> Option<Int> {
        let entry = entry(
            &mut self.accounts,
            &mut self.spare_storage,
            *acc,
            Account::default,
        );
        let warm_keys = &self.warm_keys;
        let slot = entry.storage.entry(*key).or_insert_with(|| Slot {
            warm: preloaded_warm(warm_keys, acc, key),
            ..Slot::default()
        });
        let cold = !std::mem::replace(&mut slot.warm, true);
        let prev = std::mem::replace(&mut slot.current, val);
        if cold {
            self.journal(Revert::WarmKey(*acc, *key), || Event::WarmKey(*acc, *key));
        }
        self.journal(Revert::Store(*acc, *key, prev), || {
            Event::Put(
                Target::Store {
                    acc: *acc,
                    key: *key,
                    val: prev,
                },
                val,
            )
        });
        Some(prev)
    }

    fn get_with_warmth(&mut self, acc: &Acc, key: &Int) -> Option<(Int, Int, bool)> {
        self.get_with_warmth_with(acc, key, None::<fn() -> Int>)
    }

    fn code_size(&mut self, acc: &Acc) -> Option<usize> {
        if self.records_trace() {
            return self.code(acc).map(|(code, _)| code.len());
        }
        self.accounts.get(acc).map(|e| e.account.code.0.len())
    }

    fn code_hash(&mut self, acc: &Acc) -> Option<Int> {
        if self.records_trace() {
            return self.code(acc).map(|(_, hash)| hash);
        }
        self.accounts.get(acc).map(|e| e.account.code.1)
    }

    fn frame_code(&mut self, acc: &Acc) -> Option<(Arc<[u8]>, Int)> {
        if self.records_trace() {
            return self
                .code(acc)
                .map(|(code, hash)| (code.into_vec().into(), hash));
        }
        let (code, hash) = &self.accounts.get(acc)?.account.code;
        // A zero hash marks code whose hash is unknown, which must not be shared by hash.
        if hash.is_zero() {
            return Some((Arc::from(code.as_slice()), *hash));
        }
        let code = self
            .codes
            .entry(*hash)
            .or_insert_with(|| Arc::from(code.as_slice()))
            .clone();
        Some((code, *hash))
    }

    fn jump_table(&mut self, hash: &Int, code: &[u8]) -> Arc<JumpTable> {
        self.jump_tables
            .entry(*hash)
            .or_insert_with(|| Arc::new(JumpTable::new(code)))
            .clone()
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

#[cfg(test)]
mod tests {
    use super::*;
    use yevm_base::acc::acc;

    /// EIP-6780 regression: a contract created and self-destructed in the
    /// same tx is deleted entirely at `apply()` — balance, code, storage,
    /// AND nonce. A retained nonce makes a later CREATE2 at the same address
    /// (same salt, next tx/block) fail the collision check and drain all gas
    /// (seen on mainnet replay: block 26135267, same-salt redeployer bot).
    #[test]
    fn apply_deletes_same_tx_destroyed_account_entirely() {
        let target = acc("0x00000000000000000000000000000000000000d9");
        let mut cache = Cache::new();

        // What CREATE does: fresh account with nonce 1 (EIP-161), then the
        // deployed contract writes code and storage and receives value.
        cache.create(
            target,
            Account {
                value: Int::from(5u32),
                nonce: Int::ONE,
                code: (Buf::default(), Int::ZERO),
            },
        );
        cache.set_code(&target, vec![0x60, 0x00].into(), Int::from(7u32));
        cache.init(&target, &Int::from(1u32), Int::ZERO);
        cache.put(&target, &Int::from(1u32), Int::from(42u32));
        assert_eq!(cache.nonce(&target), Some(Int::ONE));

        // Same-tx SELFDESTRUCT, then tx-end apply.
        cache.destroy(&target);
        cache.apply();

        let account = cache.acc(&target).expect("entry stays, explicitly empty");
        assert!(account.nonce.is_zero(), "nonce must be cleared (EIP-6780)");
        assert!(account.value.is_zero(), "balance must be cleared");
        assert!(account.code.0.is_empty(), "code must be cleared");
        assert_eq!(
            cache.get(&target, &Int::from(1u32)),
            Some((Int::ZERO, Int::ZERO)),
            "storage must be cleared, not refetched"
        );
        assert!(
            cache.created().is_empty() && cache.destroyed().is_empty(),
            "create/destroy bookkeeping must be drained"
        );
    }

    #[test]
    fn logs_keep_their_address_and_revert_with_the_frame() {
        let first = acc("0x00000000000000000000000000000000000000a1");
        let second = acc("0x00000000000000000000000000000000000000a2");
        let mut cache = Cache::new();

        cache.log(&first, vec![1].into(), vec![Int::ONE]);
        let checkpoint = cache.checkpoint();
        cache.log(&second, vec![2].into(), vec![]);
        cache.revert_to(checkpoint);

        assert_eq!(
            cache.logs,
            vec![(first, Buf::from(vec![1]), vec![Int::ONE])]
        );
    }

    #[test]
    fn slot_warmth_survives_loading_and_reverts_with_the_frame() {
        let target = acc("0x00000000000000000000000000000000000000f1");
        let (listed, accessed) = (Int::from(1u32), Int::from(2u32));
        let mut cache = Cache::new();

        // Warmed by an access list before being loaded.
        cache.warm_key(&target, &listed);
        cache.init(&target, &listed, Int::ZERO);
        cache.init(&target, &accessed, Int::ZERO);
        assert_eq!(
            cache.get_with_warmth(&target, &listed),
            Some((Int::ZERO, Int::ZERO, false))
        );

        let checkpoint = cache.checkpoint();
        assert!(cache.warm_key(&target, &accessed));
        assert!(!cache.is_cold_key(&target, &accessed));
        cache.revert_to(checkpoint);

        assert!(cache.is_cold_key(&target, &accessed));
        assert!(!cache.is_cold_key(&target, &listed));
    }

    #[test]
    fn reverting_a_repeated_destroy_keeps_the_first() {
        let target = acc("0x00000000000000000000000000000000000000e1");
        let mut cache = Cache::new();

        cache.destroy(&target);
        let checkpoint = cache.checkpoint();
        cache.destroy(&target);
        cache.revert_to(checkpoint);

        assert_eq!(cache.destroyed(), vec![target]);
    }

    /// Applies changes to an account, reverts the later ones and returns what is left: the
    /// state, the logs, the warmth of `[b, c, (a, k2), (a, k4)]`, a transient slot, and the
    /// created and destroyed accounts.
    #[allow(clippy::type_complexity)]
    fn revert_later_changes(
        mut cache: Cache,
    ) -> (
        Env,
        Vec<(Acc, Buf, Vec<Int>)>,
        [bool; 4],
        Option<Int>,
        Vec<Acc>,
        Vec<Acc>,
    ) {
        let [a, b, c, d] = [
            acc("0x00000000000000000000000000000000000000a1"),
            acc("0x00000000000000000000000000000000000000b1"),
            acc("0x00000000000000000000000000000000000000c1"),
            acc("0x00000000000000000000000000000000000000d1"),
        ];
        let [k1, k2, k3, k4] = [1u32, 2, 3, 4].map(Int::from);
        cache.merge(
            &a,
            Account {
                value: Int::from(10u32),
                nonce: Int::ONE,
                code: (Buf::default(), Int::ZERO),
            },
        );
        cache.init(&a, &k1, Int::from(5u32));
        cache.set_value(&a, Int::from(11u32));
        cache.put(&a, &k1, Int::from(6u32));
        cache.warm_acc(&b);
        cache.log(&a, vec![1].into(), vec![]);

        let checkpoint = cache.checkpoint();
        cache.set_value(&a, Int::from(12u32));
        cache.inc_nonce(&a, Int::ONE);
        cache.put(&a, &k1, Int::from(7u32));
        cache.put_warm(&a, &k2, Int::from(8u32));
        cache.tput(a, k3, Int::from(9u32));
        cache.warm_acc(&c);
        cache.warm_key(&a, &k4);
        cache.create(
            d,
            Account {
                value: Int::from(3u32),
                nonce: Int::ONE,
                code: (Buf::default(), Int::ZERO),
            },
        );
        cache.destroy(&a);
        cache.set_code(&a, vec![0x00].into(), Int::from(13u32));
        cache.log(&a, vec![2].into(), vec![Int::ONE]);
        cache.revert_to(checkpoint);

        let warmth = [
            cache.is_cold_acc(&b),
            cache.is_cold_acc(&c),
            cache.is_cold_key(&a, &k2),
            cache.is_cold_key(&a, &k4),
        ];
        let transient = cache.tget(&a, &k3);
        (
            cache.snapshot(),
            cache.logs.clone(),
            warmth,
            transient,
            cache.created(),
            cache.destroyed(),
        )
    }

    #[test]
    fn journal_only_mode_reverts_like_the_trace() {
        let mut journal_only = Cache::new();
        journal_only.set_journal_only(true);
        let reverted = revert_later_changes(journal_only);
        assert_eq!(reverted, revert_later_changes(Cache::new()));

        let (state, logs, warmth, transient, created, destroyed) = reverted;
        let a = acc("0x00000000000000000000000000000000000000a1");
        assert_eq!(
            state,
            vec![(
                a,
                Account {
                    value: Int::from(11u32),
                    nonce: Int::ONE,
                    code: (Buf::default(), Int::ZERO),
                },
                vec![
                    (Int::from(1u32), Int::from(6u32)),
                    (Int::from(2u32), Int::ZERO)
                ],
            )]
        );
        assert_eq!(logs, vec![(a, Buf::from(vec![1]), vec![])]);
        assert_eq!(warmth, [false, true, true, true]);
        assert_eq!(transient, None);
        assert!(created.is_empty() && destroyed.is_empty());
    }

    #[test]
    fn journal_only_mode_records_no_events() {
        let target = acc("0x00000000000000000000000000000000000000a1");
        let mut cache = Cache::new();
        cache.set_journal_only(true);

        cache.set_value(&target, Int::ONE);
        cache.log(&target, vec![1].into(), vec![]);

        assert!(cache.events.is_empty());
        assert_eq!(cache.checkpoint(), 2);
    }

    #[test]
    fn slots_load_once_into_loaded_accounts() {
        let target = acc("0x00000000000000000000000000000000000000a1");
        let (key, listed) = (Int::from(1u32), Int::from(2u32));
        let mut cache = Cache::new();

        assert_eq!(
            cache.get_warm_or_load(&target, &key, || unreachable!()),
            None
        );
        cache.merge(&target, Account::default());
        cache.warm_key(&target, &listed);

        let seven = Int::from(7u32);
        assert_eq!(
            cache.get_with_warmth_or_load(&target, &key, || seven),
            Some((seven, seven, true))
        );
        assert_eq!(
            cache.get_warm_or_load(&target, &key, || unreachable!()),
            Some((seven, true))
        );
        assert_eq!(
            cache.get_warm_or_load(&target, &key, || unreachable!()),
            Some((seven, false))
        );
        // Warmed by an access list before it was loaded.
        assert_eq!(
            cache.get_with_warmth_or_load(&target, &listed, || Int::ZERO),
            Some((Int::ZERO, Int::ZERO, false))
        );
    }

    /// A created account's storage starts empty even where the chain still has storage at its
    /// address (an EIP-7610 collision): its slots read as zero, both original and current,
    /// without being loaded, until the creation is reverted.
    #[test]
    fn created_accounts_start_with_empty_storage() {
        let target = acc("0x00000000000000000000000000000000000000c1");
        let (key, other) = (Int::from(1u32), Int::from(2u32));
        let stale = Int::from(7u32);
        let mut cache = Cache::new();
        cache.merge(&target, Account::default());

        let checkpoint = cache.checkpoint();
        cache.create(
            target,
            Account {
                value: Int::ZERO,
                nonce: Int::ONE,
                code: (Buf::default(), Int::ZERO),
            },
        );
        assert_eq!(cache.get(&target, &key), Some((Int::ZERO, Int::ZERO)));
        assert_eq!(
            cache.get_with_warmth_or_load(&target, &key, || stale),
            Some((Int::ZERO, Int::ZERO, true))
        );
        // No yield to fetch the slot either.
        assert_eq!(cache.get_warm(&target, &other), Some((Int::ZERO, true)));

        cache.revert_to(checkpoint);
        cache.merge(&target, Account::default());
        assert_eq!(
            cache.get_with_warmth_or_load(&target, &key, || stale),
            Some((stale, stale, true))
        );
    }

    #[test]
    fn accounts_load_once_on_first_write() {
        let target = acc("0x00000000000000000000000000000000000000a1");
        let mut cache = Cache::new();
        let loaded = || Account {
            value: Int::from(3u32),
            nonce: Int::from(4u32),
            code: (Buf::default(), Int::ZERO),
        };

        assert_eq!(
            cache.set_value_or_load(&target, Int::from(5u32), loaded),
            Int::from(3u32)
        );
        assert_eq!(
            cache.inc_nonce_or_load(&target, Int::ONE, || unreachable!()),
            Int::from(5u32)
        );
        assert_eq!(
            cache.set_code_or_load(&target, vec![0x00].into(), Int::ONE, || unreachable!()),
            Int::ZERO
        );
        assert_eq!(cache.balance(&target), Some(Int::from(5u32)));
    }

    #[test]
    fn entries_report_original_and_current_storage() {
        let target = acc("0x00000000000000000000000000000000000000b1");
        let key = Int::from(3u32);
        let mut cache = Cache::new();

        cache.merge(&target, Account::default());
        cache.init(&target, &key, Int::from(7u32));
        cache.put(&target, &key, Int::from(9u32));

        let entries: Vec<_> = cache
            .entries()
            .map(|(acc, _, storage)| (*acc, storage[&key].original, storage[&key].current))
            .collect();
        assert_eq!(entries, vec![(target, Int::from(7u32), Int::from(9u32))]);
    }
}
