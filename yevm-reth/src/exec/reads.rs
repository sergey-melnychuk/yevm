//! silt's read touches (silt PLAN D13/D15/D16) on the yevm executor.
//!
//! The counterpart of silt's own `silt_node::evm::Recording`, for the object-safe database the
//! engine hands yevm's executor factory: it notes every account and slot the block loads from
//! below its overlay, with its value, and files them under the block's ticket when the EVM drops
//! at the end of the block. Every value loaded from below the block's own writes is the parent
//! state's; for a key the block doesn't write that is also its value at the block, and silt drops
//! the written ones.

use alloy_primitives::{Address, B256, U256};
use evm2::{
    DatabaseError,
    bytecode::Bytecode,
    evm::{AccountInfo, DynDatabase},
};
use silt_node::reads::{ReadTouchSink, ReadValues, Ticket};
use silt_wire::{AccountChange, KECCAK_EMPTY};
use std::{sync::Arc, time::Instant};

/// An account as read: (balance, nonce, code hash), `None` if it doesn't exist.
type ReadAccount = Option<(U256, u64, B256)>;

/// Records the reads through `db` and files them under `ticket` when dropped. A failed read means
/// a failed execution: nothing is filed then.
pub(crate) struct Recording<D> {
    db: D,
    sink: Arc<ReadTouchSink>,
    ticket: Ticket,
    accounts: Vec<(Address, ReadAccount)>,
    slots: Vec<(Address, U256, U256)>,
    failed: bool,
}

impl<D> Recording<D> {
    pub(crate) fn new(db: D, sink: Arc<ReadTouchSink>, ticket: Ticket) -> Self {
        Self {
            db,
            sink,
            ticket,
            accounts: Vec::with_capacity(1024),
            slots: Vec::with_capacity(4096),
            failed: false,
        }
    }

    fn note<T>(&mut self, result: &Result<T, DatabaseError>) {
        self.failed |= result.is_err();
    }
}

impl<D: DynDatabase> DynDatabase for Recording<D> {
    fn get_account(&mut self, address: &Address) -> Result<Option<AccountInfo>, DatabaseError> {
        let result = self.db.get_account(address);
        self.note(&result);
        if let Ok(info) = &result {
            let read = info.as_ref().map(|i| (i.balance, i.nonce, i.code_hash));
            self.accounts.push((*address, read));
        }
        result
    }

    fn get_code_by_hash(&mut self, code_hash: &B256) -> Result<Bytecode, DatabaseError> {
        let result = self.db.get_code_by_hash(code_hash);
        self.note(&result);
        result
    }

    fn get_storage(&mut self, address: &Address, key: &U256) -> Result<U256, DatabaseError> {
        let result = self.db.get_storage(address, key);
        self.note(&result);
        if let Ok(value) = &result {
            self.slots.push((*address, *key, *value));
        }
        result
    }

    fn get_block_hash(&mut self, number: &U256) -> Result<B256, DatabaseError> {
        let result = self.db.get_block_hash(number);
        self.note(&result);
        result
    }
}

impl<D> Drop for Recording<D> {
    fn drop(&mut self) {
        if self.failed || std::thread::panicking() {
            return;
        }
        // On the engine's thread: only move what was recorded; the ExEx converts it when it
        // takes the block's values.
        let start = Instant::now();
        let accounts = std::mem::take(&mut self.accounts);
        let slots = std::mem::take(&mut self.slots);
        self.sink.put_deferred(
            self.ticket,
            move || read_values(accounts, slots),
            start.elapsed().as_micros() as u64,
        );
    }
}

/// Recorded reads as silt-wire values. An account that doesn't exist reads as empty (EIP-161).
fn read_values(
    accounts: Vec<(Address, ReadAccount)>,
    slots: Vec<(Address, U256, U256)>,
) -> ReadValues {
    ReadValues {
        accounts: accounts
            .into_iter()
            .map(|(address, info)| {
                let change = match info {
                    Some((balance, nonce, code_hash)) => AccountChange {
                        balance: balance.to_be_bytes(),
                        nonce,
                        code_hash: code_hash.0,
                    },
                    None => AccountChange {
                        balance: [0; 32],
                        nonce: 0,
                        code_hash: KECCAK_EMPTY,
                    },
                };
                (address.0.0, change)
            })
            .collect(),
        slots: slots
            .into_iter()
            .map(|(address, key, value)| (address.0.0, key.to_be_bytes(), value.to_be_bytes()))
            .collect(),
    }
}
