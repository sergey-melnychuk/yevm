//! Conversions between alloy, evm2 and yevm types.

use alloy_consensus::{Transaction, transaction::Recovered};
use alloy_primitives::{Address, B256, Bytes, KECCAK256_EMPTY, Log, LogData, U256};
use evm2::{
    bytecode::Bytecode, env::BlockEnv, ethereum::TxEnvelope, evm::AccountInfo,
    interpreter::InstrStop,
};
use yevm_core::{
    Acc, Call, Head, Int, Tx,
    call::{AccessListItem, AuthorizationListItem},
    evm::HaltReason,
    exe::Outcome,
    state::Account,
};
use yevm_misc::buf::Buf;

pub(crate) const fn acc(address: Address) -> Acc {
    Acc::new(address.0.0)
}

pub(crate) fn address(acc: &Acc) -> Address {
    Address::from_slice(acc.as_ref())
}

pub(crate) const fn int(value: U256) -> Int {
    Int::new(value.to_be_bytes::<32>())
}

pub(crate) const fn int_b256(value: B256) -> Int {
    Int::new(value.0)
}

pub(crate) fn u256(int: &Int) -> U256 {
    U256::from_be_slice(int.as_ref())
}

pub(crate) fn b256(int: &Int) -> B256 {
    B256::from_slice(int.as_ref())
}

/// Converts account info read from reth's state into a yevm account with its code loaded.
///
/// A missing account becomes yevm's default (empty) account: yevm does not distinguish missing
/// from empty accounts, which matches EIP-161 state clearing.
pub(crate) fn account(info: Option<&AccountInfo>, code: Option<&Bytecode>) -> Account {
    let Some(info) = info else {
        return Account::default();
    };
    let code = code
        .map(|code| code.original_byte_slice().to_vec())
        .unwrap_or_default();
    Account {
        value: int(info.balance),
        nonce: Int::from(info.nonce),
        code: (Buf(code), int_b256(info.code_hash)),
    }
}

/// Converts a yevm account back into reth account info, or `None` if it is empty (EIP-161).
///
/// yevm does not record which accounts a transaction touched, so an empty account that already
/// existed is removed even if the transaction only read it, where EIP-161 removes only touched
/// ones. `original` is the account before the transaction; its bytecode is reused when the code
/// did not change.
pub(crate) fn account_info(
    account: &Account,
    original: Option<&AccountInfo>,
) -> Option<AccountInfo> {
    let (code, code_hash) = &account.code;
    if account.value.is_zero() && account.nonce.is_zero() && code.is_empty() {
        return None;
    }
    let balance = u256(&account.value);
    let nonce = account.nonce.as_u64();
    let info = if code.is_empty() {
        AccountInfo::new(balance, nonce, KECCAK256_EMPTY, None)
    } else if let Some(original) = original.filter(|original| original.code_hash == b256(code_hash))
    {
        // yevm keeps the code hash it loaded or computed, so an equal hash is unchanged code.
        AccountInfo::new(balance, nonce, original.code_hash, original.code.clone())
    } else {
        let code = Bytecode::from_stored(Bytes::copy_from_slice(code.as_slice()));
        AccountInfo::new(balance, nonce, code.hash_slow(), Some(code))
    };
    Some(info)
}

/// Builds the yevm block header for a block environment.
pub(crate) fn head(block: &BlockEnv, base_fee: U256, blob_base_fee: U256) -> Head {
    Head {
        number: block.number.saturating_to::<u64>().into(),
        gas_limit: int(block.gas_limit),
        coinbase: acc(block.beneficiary),
        timestamp: int(block.timestamp),
        base_fee: int(base_fee),
        blob_base_fee: Some(int(blob_base_fee)),
        prevrandao: int(block.prevrandao),
        ..Head::default()
    }
}

/// Builds the yevm message and transaction for a recovered transaction.
///
/// With `charge_fees` unset, all fee fields are zero, so yevm neither deducts nor credits fees.
pub(crate) fn call_and_tx(
    tx: &Recovered<TxEnvelope>,
    chain_id: u64,
    charge_fees: bool,
) -> (Call, Tx) {
    let call = Call {
        by: acc(tx.signer()),
        to: tx.kind().to().copied().map(acc),
        gas: tx.gas_limit(),
        eth: int(tx.value()),
        data: Buf(tx.input().to_vec()),
    };
    let fee = |value: u128| {
        if charge_fees {
            Int::from(value)
        } else {
            Int::ZERO
        }
    };
    let (max_fee_per_gas, max_priority_fee_per_gas) = if tx.is_dynamic_fee() {
        (
            fee(tx.max_fee_per_gas()),
            fee(tx.max_priority_fee_per_gas().unwrap_or_default()),
        )
    } else {
        (Int::ZERO, Int::ZERO)
    };
    let tx = Tx {
        chain_id: chain_id.into(),
        nonce: Int::from(tx.nonce()),
        gas_price: fee(tx.max_fee_per_gas()),
        max_fee_per_gas,
        max_priority_fee_per_gas,
        access_list: tx
            .access_list()
            .map(|list| {
                list.iter()
                    .map(|item| AccessListItem {
                        address: acc(item.address),
                        storage_keys: item.storage_keys.iter().copied().map(int_b256).collect(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        authorization_list: tx
            .authorization_list()
            .map(|list| {
                list.iter()
                    .map(|auth| AuthorizationListItem {
                        address: acc(auth.address),
                        chain_id: int(auth.chain_id),
                        nonce: Int::from(auth.nonce),
                        r: int(auth.r()),
                        s: int(auth.s()),
                        y_parity: Int::from(auth.y_parity()),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        blob_versioned_hashes: tx
            .blob_versioned_hashes()
            .map(|hashes| hashes.iter().copied().map(int_b256).collect())
            .unwrap_or_default(),
        max_fee_per_blob_gas: tx.max_fee_per_blob_gas().map(fee),
        hash: Int::ZERO,
        index: Int::ZERO,
    };
    (call, tx)
}

/// Converts the logs yevm recorded into alloy logs.
pub(crate) fn logs(logs: &[(Acc, Buf, Vec<Int>)]) -> Vec<Log> {
    logs.iter()
        .map(|(acc, data, topics)| Log {
            address: address(acc),
            data: LogData::new_unchecked(
                topics.iter().map(b256).collect(),
                Bytes::copy_from_slice(data.as_slice()),
            ),
        })
        .collect()
}

/// Returns evm2's stop reason for a transaction that ended with `outcome`.
pub(crate) const fn stop_reason(
    success: bool,
    outcome: Option<Outcome>,
    empty_output: bool,
) -> InstrStop {
    match outcome {
        Some(Outcome::Return) if success && !empty_output => InstrStop::Return,
        Some(Outcome::Return) if success => InstrStop::Stop,
        Some(Outcome::Revert) => InstrStop::Revert,
        Some(Outcome::Halt(reason)) => halt_reason(reason),
        // The executor records an outcome for every transaction it runs to the end.
        Some(Outcome::Return) | None => {
            if success {
                InstrStop::Stop
            } else {
                InstrStop::OutOfGas
            }
        }
    }
}

const fn halt_reason(reason: HaltReason) -> InstrStop {
    match reason {
        HaltReason::OutOfGas | HaltReason::GasBelowStipend => InstrStop::OutOfGas,
        HaltReason::OutOfMemory => InstrStop::MemoryLimitOOG,
        HaltReason::BadCopyRange => InstrStop::OutOfOffset,
        HaltReason::BadJump(_) => InstrStop::InvalidJump,
        HaltReason::BadOpcode(0xfe) => InstrStop::InvalidFEOpcode,
        HaltReason::BadOpcode(_) => InstrStop::OpcodeNotFound,
        HaltReason::NonStatic => InstrStop::StateChangeDuringStaticCall,
        HaltReason::StackUnderflow => InstrStop::StackUnderflow,
        HaltReason::StackOverflow => InstrStop::StackOverflow,
        HaltReason::OutOfFunds => InstrStop::OutOfFunds,
        HaltReason::CreateCollision => InstrStop::CreateCollision,
        HaltReason::CodeSizeLimit => InstrStop::CreateContractSizeLimit,
        HaltReason::InvalidCodePrefix => InstrStop::CreateContractStartingWithEF,
        HaltReason::PrecompileFailure => InstrStop::PrecompileError,
    }
}
