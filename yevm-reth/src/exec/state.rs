//! yevm state backed by reth's state overlay.
//!
//! yevm's executor reads state through [`State`] and, when an entry is missing, yields to fetch
//! it from a [`Chain`] and then re-executes the instruction. [`DbState`] instead loads every
//! missing account and storage slot synchronously from the overlay, so execution never yields,
//! and records what was read for the transaction's pending state.

use super::convert::{self, int, int_b256};
use alloy_primitives::{U256, map::AddressMap};
use core::cell::RefCell;
use evm2::{
    DatabaseError,
    evm::{AccountInfo, DynDatabase},
};
use std::sync::Arc;
use yevm_core::{
    Acc, Int,
    cache::{Cache, Slot},
    call::{Block, Head},
    chain::Chain,
    evm::JumpTable,
    state::{Account, State},
    trace::Event,
};
use yevm_misc::buf::Buf;

/// yevm's journal ([`Cache`]) over a database, loading misses on demand.
pub(crate) struct DbState<'s, 'a> {
    cache: &'s mut Cache,
    /// In a cell for [`State::head`], the only read yevm makes through `&self`.
    db: RefCell<&'s mut (dyn DynDatabase + 'a)>,
    /// Account info of every account loaded, as it was when loaded (the transaction start).
    originals: &'s mut AddressMap<Option<AccountInfo>>,
    /// The first database error. Execution continues on empty values; the caller discards its
    /// result when this is set.
    error: RefCell<Option<DatabaseError>>,
}

impl<'s, 'a> DbState<'s, 'a> {
    /// Creates a state over `cache` and `db` that records loaded accounts in `originals`, which
    /// must be empty.
    pub(crate) fn new(
        cache: &'s mut Cache,
        db: &'s mut (dyn DynDatabase + 'a),
        originals: &'s mut AddressMap<Option<AccountInfo>>,
    ) -> Self {
        Self {
            cache,
            db: RefCell::new(db),
            originals,
            error: RefCell::new(None),
        }
    }

    /// Returns the first database error.
    pub(crate) fn into_error(self) -> Option<DatabaseError> {
        self.error.into_inner()
    }

    fn fail(&self, error: DatabaseError) {
        self.error.borrow_mut().get_or_insert(error);
    }

    /// Reads a slot with `read`, which loads missing slots of loaded accounts and returns `None`
    /// if the account is not loaded; the account is then loaded and `read` retried.
    fn read_slot<T>(
        &mut self,
        acc: &Acc,
        key: &Int,
        read: impl Fn(&mut Cache, &mut dyn FnMut() -> Int) -> Option<T>,
    ) -> Option<T> {
        for _ in 0..2 {
            let Self {
                cache, db, error, ..
            } = self;
            let mut load = || load_storage(*db.get_mut(), error, acc, key);
            if let Some(value) = read(cache, &mut load) {
                return Some(value);
            }
            self.ensure_account(acc);
        }
        None
    }

    /// Loads `acc` into the journal unless it is already there.
    fn ensure_account(&mut self, acc: &Acc) {
        if self.cache.account(acc).is_some() {
            return;
        }
        let account = load(*self.db.get_mut(), self.originals, &self.error, acc);
        self.cache.merge(acc, account);
    }

    /// Loads a storage slot that is not in the journal.
    fn load_slot(&mut self, acc: &Acc, key: &Int) -> Slot {
        self.ensure_account(acc);
        let value = load_storage(*self.db.get_mut(), &self.error, acc, key);
        self.cache.insert_slot(acc, key, value)
    }
}

/// Reads an account for the journal, recording its info in `originals` and a failure in `error`
/// (reading the account as empty).
fn load(
    db: &mut (dyn DynDatabase + '_),
    originals: &mut AddressMap<Option<AccountInfo>>,
    error: &RefCell<Option<DatabaseError>>,
    acc: &Acc,
) -> Account {
    let address = convert::address(acc);
    match load_account(db, &address) {
        Ok(info) => {
            let account = convert::account(
                info.as_ref(),
                info.as_ref().and_then(|info| info.code.as_ref()),
            );
            originals.entry(address).or_insert(info);
            account
        }
        Err(failure) => {
            error.borrow_mut().get_or_insert(failure);
            Account::default()
        }
    }
}

/// Reads a storage slot, recording a failure in `error` and reading it as zero.
fn load_storage(
    db: &mut (dyn DynDatabase + '_),
    error: &RefCell<Option<DatabaseError>>,
    acc: &Acc,
    key: &Int,
) -> Int {
    match db.get_storage(&convert::address(acc), &convert::u256(key)) {
        Ok(value) => int(value),
        Err(failure) => {
            error.borrow_mut().get_or_insert(failure);
            Int::ZERO
        }
    }
}

/// Reads account info with its code loaded.
pub(crate) fn load_account(
    db: &mut (dyn DynDatabase + '_),
    address: &alloy_primitives::Address,
) -> Result<Option<AccountInfo>, DatabaseError> {
    let Some(mut info) = db.get_account(address)? else {
        return Ok(None);
    };
    if info.code.is_none() && !info.is_empty_code_hash() {
        info.code = Some(db.get_code_by_hash(&info.code_hash)?);
    }
    Ok(Some(info))
}

// Reads try the journal first and load the account only when it misses.
impl State for DbState<'_, '_> {
    fn get(&mut self, acc: &Acc, key: &Int) -> Option<(Int, Int)> {
        if let Some(slot) = self.cache.get(acc, key) {
            return Some(slot);
        }
        let slot = self.load_slot(acc, key);
        Some((slot.current, slot.original))
    }

    fn get_with_warmth(&mut self, acc: &Acc, key: &Int) -> Option<(Int, Int, bool)> {
        self.read_slot(acc, key, |cache, load| {
            cache.get_with_warmth_or_load(acc, key, load)
        })
    }

    fn get_warm(&mut self, acc: &Acc, key: &Int) -> Option<(Int, bool)> {
        self.read_slot(acc, key, |cache, load| {
            cache.get_warm_or_load(acc, key, load)
        })
    }

    fn put_warm(&mut self, acc: &Acc, key: &Int, val: Int) -> Option<Int> {
        self.cache.put_warm(acc, key, val)
    }

    fn put(&mut self, acc: &Acc, key: &Int, val: Int) -> Option<Int> {
        self.cache.put(acc, key, val)
    }

    fn init(&mut self, acc: &Acc, key: &Int, val: Int) {
        self.cache.init(acc, key, val);
    }

    fn tget(&mut self, acc: &Acc, key: &Int) -> Option<Int> {
        self.cache.tget(acc, key)
    }

    fn tput(&mut self, acc: Acc, key: Int, val: Int) -> Option<Int> {
        self.cache.tput(acc, key, val)
    }

    fn inc_nonce(&mut self, acc: &Acc, nonce: Int) -> Int {
        let Self {
            cache,
            db,
            originals,
            error,
        } = self;
        cache.inc_nonce_or_load(acc, nonce, || load(*db.get_mut(), originals, error, acc))
    }

    fn set_value(&mut self, acc: &Acc, value: Int) -> Int {
        let Self {
            cache,
            db,
            originals,
            error,
        } = self;
        cache.set_value_or_load(acc, value, || load(*db.get_mut(), originals, error, acc))
    }

    fn set_code(&mut self, acc: &Acc, code: Buf, hash: Int) -> Int {
        let Self {
            cache,
            db,
            originals,
            error,
        } = self;
        cache.set_code_or_load(acc, code, hash, || {
            load(*db.get_mut(), originals, error, acc)
        })
    }

    fn set_auth(&mut self, src: &Acc, dst: &Acc) {
        self.ensure_account(src);
        self.cache.set_auth(src, dst);
    }

    fn merge(&mut self, acc: &Acc, chain: Account) {
        self.cache.merge(acc, chain);
    }

    fn balance(&mut self, acc: &Acc) -> Option<Int> {
        if let Some(value) = self.cache.balance(acc) {
            return Some(value);
        }
        self.ensure_account(acc);
        self.cache.balance(acc)
    }

    fn nonce(&mut self, acc: &Acc) -> Option<Int> {
        if let Some(value) = self.cache.nonce(acc) {
            return Some(value);
        }
        self.ensure_account(acc);
        self.cache.nonce(acc)
    }

    fn code(&mut self, acc: &Acc) -> Option<(Buf, Int)> {
        if let Some(value) = self.cache.code(acc) {
            return Some(value);
        }
        self.ensure_account(acc);
        self.cache.code(acc)
    }

    fn acc(&mut self, acc: &Acc) -> Option<Account> {
        if let Some(value) = self.cache.acc(acc) {
            return Some(value);
        }
        self.ensure_account(acc);
        self.cache.acc(acc)
    }

    fn is_cold_acc(&self, acc: &Acc) -> bool {
        self.cache.is_cold_acc(acc)
    }

    fn is_cold_key(&self, acc: &Acc, key: &Int) -> bool {
        self.cache.is_cold_key(acc, key)
    }

    fn warm_acc(&mut self, acc: &Acc) -> bool {
        self.cache.warm_acc(acc)
    }

    fn warm_key(&mut self, acc: &Acc, key: &Int) -> bool {
        self.cache.warm_key(acc, key)
    }

    fn create(&mut self, acc: Acc, info: Account) {
        self.cache.create(acc, info);
    }

    fn destroy(&mut self, acc: &Acc) {
        self.cache.destroy(acc);
    }

    fn hash(&mut self, number: u64, hash: Int) {
        self.cache.hash(number, hash);
    }

    fn log(&mut self, acc: &Acc, data: Buf, topics: Vec<Int>) {
        self.cache.log(acc, data, topics);
    }

    fn head(&self, number: u64) -> Option<Head> {
        let hash = self
            .db
            .borrow_mut()
            .get_block_hash(&U256::from(number))
            .unwrap_or_else(|error| {
                self.fail(error);
                Default::default()
            });
        Some(Head {
            number: number.into(),
            hash: int_b256(hash),
            ..Head::default()
        })
    }

    fn auth(&self, acc: &Acc) -> Option<Acc> {
        self.cache.auth(acc)
    }

    fn created(&self) -> Vec<Acc> {
        self.cache.created()
    }

    fn destroyed(&self) -> Vec<Acc> {
        self.cache.destroyed()
    }

    fn apply(&mut self) {
        self.cache.apply();
    }

    fn get_depth(&mut self) -> usize {
        self.cache.get_depth()
    }

    fn set_depth(&mut self, depth: usize) {
        self.cache.set_depth(depth);
    }

    fn emit(&mut self, event: Event) -> usize {
        self.cache.emit(event)
    }

    fn checkpoint(&mut self) -> usize {
        self.cache.checkpoint()
    }

    fn revert_to(&mut self, checkpoint: usize) {
        self.cache.revert_to(checkpoint);
    }

    fn set_chain_id(&mut self, id: u64) {
        self.cache.set_chain_id(id);
    }

    fn get_chain_id(&self) -> u64 {
        self.cache.get_chain_id()
    }

    fn is_tracing(&self) -> bool {
        self.cache.is_tracing()
    }

    fn records_trace(&self) -> bool {
        self.cache.records_trace()
    }

    fn has_acc(&mut self, acc: &Acc) -> bool {
        self.ensure_account(acc);
        true
    }

    fn code_size(&mut self, acc: &Acc) -> Option<usize> {
        if let Some(value) = self.cache.code_size(acc) {
            return Some(value);
        }
        self.ensure_account(acc);
        self.cache.code_size(acc)
    }

    fn code_hash(&mut self, acc: &Acc) -> Option<Int> {
        if let Some(value) = self.cache.code_hash(acc) {
            return Some(value);
        }
        self.ensure_account(acc);
        self.cache.code_hash(acc)
    }

    fn frame_code(&mut self, acc: &Acc) -> Option<(Arc<[u8]>, Int)> {
        if let Some(value) = self.cache.frame_code(acc) {
            return Some(value);
        }
        self.ensure_account(acc);
        self.cache.frame_code(acc)
    }

    fn jump_table(&mut self, hash: &Int, code: &[u8]) -> Arc<JumpTable> {
        self.cache.jump_table(hash, code)
    }
}

/// [`Chain`] for an executor whose state never misses ([`DbState`]): any call is a bug and fails
/// the transaction.
#[derive(Debug)]
pub(crate) struct NoChain;

#[async_trait::async_trait]
impl Chain for NoChain {
    async fn get(&self, _: &Acc, _: &Int) -> eyre::Result<Int> {
        Err(eyre::eyre!(
            "yevm fetched storage although its state loads it"
        ))
    }

    async fn acc(&self, _: &Acc) -> eyre::Result<Account> {
        Err(eyre::eyre!(
            "yevm fetched an account although its state loads it"
        ))
    }

    async fn code(&self, _: &Acc) -> eyre::Result<(Buf, Int)> {
        Err(eyre::eyre!("yevm fetched code although its state loads it"))
    }

    async fn nonce(&self, _: &Acc) -> eyre::Result<u64> {
        Err(eyre::eyre!(
            "yevm fetched a nonce although its state loads it"
        ))
    }

    async fn balance(&self, _: &Acc) -> eyre::Result<Int> {
        Err(eyre::eyre!(
            "yevm fetched a balance although its state loads it"
        ))
    }

    async fn head(&self, number: u64) -> eyre::Result<Head> {
        Err(eyre::eyre!(
            "yevm fetched block {number} although its state loads it"
        ))
    }

    async fn block(&self, number: u64) -> eyre::Result<Block> {
        Err(eyre::eyre!(
            "block {number} requested, but yevm only reads state during execution"
        ))
    }

    async fn chain_id(&self) -> eyre::Result<u64> {
        Err(eyre::eyre!(
            "yevm fetched the chain id although its state knows it"
        ))
    }
}
