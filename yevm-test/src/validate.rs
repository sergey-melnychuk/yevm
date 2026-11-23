//! Transaction validity checks for the execution-spec state tests, ported from the Python spec
//! (`check_transaction`, `validate_transaction` and `recover_sender` under
//! `src/ethereum/forks/{osaka,amsterdam}`, v21.0.3).
//!
//! yevm executes whatever it is given and leaves deciding whether a transaction may run
//! (signature, nonce, balance, gas limits, blob rules) to its host. The state tests expect that
//! decision, so the runner makes it here, before handing the transaction to yevm. Nothing
//! outside the tests uses this.
//!
//! Each check returns the fixture's name for the exception (`TransactionException.<NAME>`), so
//! the runner can compare it with the one the fixture expects.

use alloy_consensus::{TxEnvelope, transaction::SignerRecoverable};
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::U256;
use yevm_base::Int;
use yevm_core::{Head, Spec};

use crate::eest::{Case, Post};

/// The state tests' chain.
const CHAIN_ID: u64 = 1;
/// EIP-7594: the most blobs one transaction may carry.
const BLOB_COUNT_LIMIT: usize = 6;
/// EIP-4844: blob gas per blob.
const GAS_PER_BLOB: u64 = 1 << 17;
/// EIP-2681.
const NONCE_MAX: u64 = u64::MAX;
/// EIP-7825: the gas limit cap; from Amsterdam it caps execution gas only.
const TX_MAX_GAS_LIMIT: u64 = 1 << 24;
/// EIP-8037: the gas limit cap from Amsterdam.
const TX_MAX_TOTAL_GAS_LIMIT: u64 = u32::MAX as u64;

fn u256(x: Int) -> U256 {
    U256::from_be_bytes(x.to_bytes())
}

fn u64_of(x: Int) -> Option<u64> {
    u64::try_from(u256(x)).ok()
}

/// The intrinsic gas and the calldata floor (`calculate_intrinsic_cost`).
fn intrinsic(spec: Spec, case: &Case, post: &Post) -> (u64, u64) {
    let tx = &case.transaction;
    let data = tx.data[post.indexes.data].as_slice();
    let value = tx.value[post.indexes.value];
    let zeros = data.iter().filter(|b| **b == 0).count() as u64;
    let len = data.len() as u64;
    let tokens = zeros + (len - zeros) * 4;
    let is_create = tx.to.is_none();
    let init_code = if is_create { 2 * len.div_ceil(32) } else { 0 };
    let access_list = tx
        .access_lists
        .as_ref()
        .and_then(|lists| lists.get(post.indexes.data))
        .and_then(Option::as_ref);
    let (addresses, keys) = access_list.map_or((0, 0), |list| {
        let keys = list.iter().map(|a| a.storage_keys.len() as u64).sum();
        (list.len() as u64, keys)
    });
    let auths = tx.authorization_list.as_ref().map_or(0, |a| a.len() as u64);

    match spec {
        Spec::Osaka => {
            let create = if is_create { 32_000 + init_code } else { 0 };
            let execution =
                21_000 + 4 * tokens + create + 2_400 * addresses + 1_900 * keys + 25_000 * auths;
            (execution, 21_000 + 10 * tokens)
        }
        Spec::Amsterdam => {
            const COLD_ACCOUNT_ACCESS: u64 = 3_000;
            let recipient = if is_create {
                9_000 + COLD_ACCOUNT_ACCESS
            } else if tx.to == Some(tx.sender) {
                0
            } else if value.is_zero() {
                COLD_ACCOUNT_ACCESS
            } else {
                COLD_ACCOUNT_ACCESS + 6_000
            };
            let base = 12_000 + recipient;
            let access_list_data = 16 * (80 * addresses + 128 * keys);
            let per_auth = 101 * 16 + 3_000 + COLD_ACCOUNT_ACCESS + 200;
            let execution = base
                + init_code
                + 4 * tokens
                + 2_900 * addresses
                + 2_000 * keys
                + access_list_data
                + per_auth * auths;
            (execution, base + 16 * 4 * len + access_list_data)
        }
    }
}

/// One RLP item: whether it is a list, its payload, and its encoded length.
fn rlp_item(b: &[u8]) -> Option<(bool, &[u8], usize)> {
    let be = |len: &[u8]| len.iter().fold(0usize, |n, b| (n << 8) | *b as usize);
    let (list, start, len) = match *b.first()? {
        0x00..=0x7f => return Some((false, &b[..1], 1)),
        x @ 0x80..=0xb7 => (false, 1, (x - 0x80) as usize),
        x @ 0xb8..=0xbf => {
            let n = (x - 0xb7) as usize;
            (false, 1 + n, be(b.get(1..1 + n)?))
        }
        x @ 0xc0..=0xf7 => (true, 1, (x - 0xc0) as usize),
        x @ 0xf8..=0xff => {
            let n = (x - 0xf7) as usize;
            (true, 1 + n, be(b.get(1..1 + n)?))
        }
    };
    Some((list, b.get(start..start + len)?, start + len))
}

/// The top-level fields of a signed transaction: its type (none for legacy) and each field's
/// payload.
fn rlp_fields(bytes: &[u8]) -> Option<(Option<u8>, Vec<&[u8]>)> {
    let (ty, body) = match bytes.first()? {
        ty @ 0x00..=0x7f => (Some(*ty), &bytes[1..]),
        _ => (None, bytes),
    };
    let (true, mut list, _) = rlp_item(body)? else {
        return None;
    };
    let mut fields = Vec::new();
    while !list.is_empty() {
        let (_, payload, len) = rlp_item(list)?;
        fields.push(payload);
        list = &list[len..];
    }
    Some((ty, fields))
}

/// Checks the signature values and the chain ID (`chain_id`, `recover_sender`), then that the
/// signature recovers the fixture's sender.
fn signature(post: &Post, sender: &yevm_base::Acc) -> Result<(), &'static str> {
    const BAD: &str = "INVALID_SIGNATURE_VRS";
    // secp256k1's group order.
    const N: U256 = alloy_primitives::uint!(
        0xfffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141_U256
    );
    let bytes = post.txbytes.as_slice();
    let (ty, fields) = rlp_fields(bytes).ok_or(BAD)?;
    let [.., v, r, s] = fields[..] else {
        return Err(BAD);
    };
    let int = |b: &[u8]| U256::try_from_be_slice(b).ok_or(BAD);
    let (v, r, s) = (int(v)?, int(r)?, int(s)?);

    let chain_id = match ty {
        Some(_) => Some(int(fields.first().ok_or(BAD)?)?),
        // Legacy: none before EIP-155 (v is 27 or 28), else v = 35 + 2 * chain ID + parity.
        None if v == U256::from(27) || v == U256::from(28) => None,
        None if v < U256::from(35) => return Err(BAD),
        None => Some((v - U256::from(35)) >> 1),
    };
    if chain_id.is_some_and(|id| id != U256::from(CHAIN_ID)) {
        return Err("INVALID_CHAINID");
    }
    if r.is_zero() || r >= N || s.is_zero() || s > N >> 1 {
        return Err(BAD);
    }
    if ty.is_some() && v > U256::ONE {
        return Err(BAD);
    }

    // Some fixtures' transactions are invalid in ways the decoder can't represent (a blob or
    // set-code transaction that creates a contract, a gas price above 2^128); the checks above
    // cover their signature values, and the transaction is rejected for its other fault.
    let Ok(tx) = TxEnvelope::decode_2718(&mut &bytes[..]) else {
        return Ok(());
    };
    let signer = tx.recover_signer().map_err(|_| BAD)?;
    if signer.as_slice() != sender.as_ref() {
        return Err(BAD);
    }
    Ok(())
}

/// The first reason the spec gives to reject the transaction, if any.
pub fn validate(case: &Case, post: &Post, head: &Head) -> Result<(), &'static str> {
    let spec = head.spec;
    let tx = &case.transaction;
    let gas = u64_of(tx.gas_limit[post.indexes.gas]).unwrap_or(u64::MAX);
    let value = u256(tx.value[post.indexes.value]);
    let data_len = tx.data[post.indexes.data].as_slice().len();
    let is_create = tx.to.is_none();
    let blobs = tx.blob_versioned_hashes.as_ref();
    let auths = tx.authorization_list.as_ref();
    let fee_market = tx.max_fee_per_gas.is_some();
    let block_gas_limit = u64_of(case.env.current_gas_limit).unwrap_or(u64::MAX);
    let max_blob_gas_per_block = GAS_PER_BLOB
        * match spec {
            Spec::Osaka => 9,
            Spec::Amsterdam => 21,
        };
    let blob_gas = blobs.map_or(0, |b| b.len() as u64) * GAS_PER_BLOB;
    let max_init_code_size = spec.max_initcode_size();

    let (execution, floor) = intrinsic(spec, case, post);
    // Amsterdam checks the signature first; Osaka after the block's gas and blob capacity.
    if spec == Spec::Amsterdam {
        signature(post, &tx.sender)?;
    }

    // `validate_transaction`
    let nonce_max = u256(tx.nonce) >= U256::from(NONCE_MAX);
    match spec {
        Spec::Osaka => {
            if execution.max(floor) > gas {
                return Err(if execution > gas {
                    "INTRINSIC_GAS_TOO_LOW"
                } else {
                    "INTRINSIC_GAS_BELOW_FLOOR_GAS_COST"
                });
            }
            if is_create && data_len > max_init_code_size {
                return Err("INITCODE_SIZE_EXCEEDED");
            }
            if gas > TX_MAX_GAS_LIMIT {
                return Err("GAS_LIMIT_EXCEEDS_MAXIMUM");
            }
            if nonce_max {
                return Err("NONCE_IS_MAX");
            }
        }
        Spec::Amsterdam => {
            if nonce_max {
                return Err("NONCE_IS_MAX");
            }
            if is_create && data_len > max_init_code_size {
                return Err("INITCODE_SIZE_EXCEEDED");
            }
            if gas > TX_MAX_TOTAL_GAS_LIMIT {
                return Err("GAS_LIMIT_EXCEEDS_MAXIMUM");
            }
        }
    }
    if let (Some(max_fee), Some(priority)) = (tx.max_fee_per_gas, tx.max_priority_fee_per_gas)
        && u256(max_fee) < u256(priority)
    {
        return Err("PRIORITY_GREATER_THAN_MAX_FEE_PER_GAS");
    }
    let blob_shape = || -> Result<(), &'static str> {
        if let Some(blobs) = blobs {
            if blobs.is_empty() {
                return Err("TYPE_3_TX_ZERO_BLOBS");
            }
            if blobs.len() > BLOB_COUNT_LIMIT {
                return Err("TYPE_3_TX_BLOB_COUNT_EXCEEDED");
            }
            if blobs.iter().any(|h| h.as_ref()[0] != 0x01) {
                return Err("TYPE_3_TX_INVALID_BLOB_VERSIONED_HASH");
            }
        }
        Ok(())
    };
    let tx_shape = || -> Result<(), &'static str> {
        if is_create && blobs.is_some() {
            return Err("TYPE_3_TX_CONTRACT_CREATION");
        }
        if is_create && auths.is_some() {
            return Err("TYPE_4_TX_CONTRACT_CREATION");
        }
        if auths.is_some_and(|a| a.is_empty()) {
            return Err("TYPE_4_EMPTY_AUTHORIZATION_LIST");
        }
        Ok(())
    };
    if spec == Spec::Amsterdam {
        blob_shape()?;
        tx_shape()?;
        if execution > gas {
            return Err("INTRINSIC_GAS_TOO_LOW");
        }
        if floor > gas {
            return Err("INTRINSIC_GAS_BELOW_FLOOR_GAS_COST");
        }
        if execution > TX_MAX_GAS_LIMIT || floor > TX_MAX_GAS_LIMIT {
            return Err("INTRINSIC_GAS_TOO_LOW");
        }
    }

    // `check_transaction`
    match spec {
        Spec::Osaka => {
            if gas > block_gas_limit {
                return Err("GAS_ALLOWANCE_EXCEEDED");
            }
            if blob_gas > max_blob_gas_per_block {
                return Err("TYPE_3_TX_MAX_BLOB_GAS_ALLOWANCE_EXCEEDED");
            }
            signature(post, &tx.sender)?;
        }
        Spec::Amsterdam => {
            if gas.min(TX_MAX_GAS_LIMIT) > block_gas_limit || gas > block_gas_limit {
                return Err("GAS_ALLOWANCE_EXCEEDED");
            }
            if blob_gas > max_blob_gas_per_block {
                return Err("TYPE_3_TX_MAX_BLOB_GAS_ALLOWANCE_EXCEEDED");
            }
        }
    }
    let base_fee = u256(case.env.current_base_fee.unwrap_or_default());
    let price = match (tx.max_fee_per_gas, tx.gas_price) {
        (Some(max_fee), _) if fee_market => u256(max_fee),
        (_, price) => u256(price.unwrap_or_default()),
    };
    if price < base_fee {
        return Err("INSUFFICIENT_MAX_FEE_PER_GAS");
    }
    if spec == Spec::Osaka {
        blob_shape()?;
    }
    // The spec's integers are unbounded: a fee that overflows 256 bits is unaffordable.
    let mut max_gas_fee = U256::from(gas).checked_mul(price);
    if blobs.is_some() {
        let blob_fee = u256(head.blob_base_fee.unwrap_or_default());
        let max_fee_per_blob_gas = u256(tx.max_fee_per_blob_gas.unwrap_or_default());
        if max_fee_per_blob_gas < blob_fee {
            return Err("INSUFFICIENT_MAX_FEE_PER_BLOB_GAS");
        }
        let blob_cost = U256::from(blob_gas).checked_mul(max_fee_per_blob_gas);
        max_gas_fee = max_gas_fee
            .zip(blob_cost)
            .and_then(|(a, b)| a.checked_add(b));
    }
    if spec == Spec::Osaka {
        tx_shape()?;
    }

    let account = case.pre.get(&tx.sender);
    let sender_nonce = account.map_or(U256::ZERO, |a| u256(a.nonce));
    let tx_nonce = u256(tx.nonce);
    if sender_nonce > tx_nonce {
        return Err("NONCE_MISMATCH_TOO_LOW");
    }
    if sender_nonce < tx_nonce {
        return Err("NONCE_MISMATCH_TOO_HIGH");
    }
    let balance = account.map_or(U256::ZERO, |a| u256(a.balance));
    let cost = max_gas_fee.and_then(|fee| fee.checked_add(value));
    if cost.is_none_or(|cost| balance < cost) {
        return Err("INSUFFICIENT_ACCOUNT_FUNDS");
    }
    let code = account.map_or(&[][..], |a| a.code.as_slice());
    let delegation = code.len() == 23 && code.starts_with(&[0xef, 0x01, 0x00]);
    if !code.is_empty() && !delegation {
        return Err("SENDER_NOT_EOA");
    }
    Ok(())
}
