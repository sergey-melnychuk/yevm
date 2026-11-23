//! Gas costs that differ between the forks yevm implements.
//!
//! Amsterdam (EIP-8037) adds a second dimension, state gas, for what grows the state: new
//! accounts, new storage slots, deployed code and delegations. It is charged from a
//! reservoir and spills into execution gas when the reservoir runs out, see [`crate::evm::Gas`].

use crate::Spec;

/// EIP-8037: state gas per byte of state.
pub const COST_PER_STATE_BYTE: i64 = 1530;
/// EIP-8037: state gas for a new account (120 bytes).
pub const NEW_ACCOUNT: i64 = 120 * COST_PER_STATE_BYTE;
/// EIP-8037: state gas for a storage slot set from zero (64 bytes).
pub const STORAGE_SET: i64 = 64 * COST_PER_STATE_BYTE;
/// EIP-8037: state gas for a delegation (23 bytes).
pub const AUTH_BASE: i64 = 23 * COST_PER_STATE_BYTE;
/// Amsterdam: the most execution gas a transaction gets (EIP-7825); gas beyond it goes to the
/// state gas reservoir.
pub const TX_MAX_GAS_LIMIT: i64 = 1 << 24;
/// Amsterdam: execution gas for writing an account (`ACCOUNT_WRITE`).
pub const ACCOUNT_WRITE: i64 = 9000;
/// The gas a call with value adds to the callee's gas.
pub const CALL_STIPEND: i64 = 2300;
/// EIP-7708: the address that ETH transfer logs come from.
pub const SYSTEM_ADDRESS: crate::Acc =
    yevm_base::acc::acc("0xfffffffffffffffffffffffffffffffffffffffe");

impl Spec {
    /// Accessing a cold account (EIP-2929; 3000 from Amsterdam, EIP-8038).
    pub fn cold_account(self) -> i64 {
        if self.is(Spec::Amsterdam) { 3000 } else { 2600 }
    }

    /// The cold account surcharge over a warm access.
    pub fn cold_account_extra(self) -> i64 {
        self.cold_account() - 100
    }

    /// CREATE and CREATE2 base cost: 32000, `ACCOUNT_WRITE + COLD_ACCOUNT_ACCESS` from Amsterdam.
    pub fn create(self) -> i64 {
        if self.is(Spec::Amsterdam) {
            ACCOUNT_WRITE + self.cold_account()
        } else {
            32_000
        }
    }

    /// A call that sends value: 9000, plus the stipend from Amsterdam (EIP-8038).
    pub fn call_value(self) -> i64 {
        if self.is(Spec::Amsterdam) {
            ACCOUNT_WRITE + CALL_STIPEND
        } else {
            9000
        }
    }

    /// The largest deployed code (EIP-170; 64 KiB from Amsterdam, EIP-7954).
    pub fn max_code_size(self) -> usize {
        if self.is(Spec::Amsterdam) {
            0x10000
        } else {
            24_576
        }
    }

    /// The largest initcode (EIP-3860): twice the largest code.
    pub fn max_initcode_size(self) -> usize {
        2 * self.max_code_size()
    }

    /// Whether gas has a state dimension (EIP-8037).
    pub fn state_gas(self) -> bool {
        self.is(Spec::Amsterdam)
    }
}
