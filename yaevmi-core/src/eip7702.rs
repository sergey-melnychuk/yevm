//! EIP-7702: apply authorization list before transaction execution (see revm `apply_auth_list`).

use alloy_eip7702::{Authorization, SignedAuthorization};
use alloy_primitives::{Address, U256};
use yaevmi_misc::buf::Buf;

use crate::Tx;
use crate::chain::{Chain, fetch};
use crate::state::{Account, State};
use crate::{Acc, Int, Result};

/// Per EIP-7702 / `alloy_eip7702::constants` (matches revm initial gas and refund schedule).
const PER_AUTH_BASE: i64 = 12_500;
const PER_EMPTY_ACCOUNT: i64 = 25_000;

fn int_to_u256(i: &Int) -> U256 {
    U256::from_be_bytes(<[u8; 32]>::try_from(i.as_ref()).unwrap())
}

fn acc_from_address(a: Address) -> Acc {
    Acc::from(a.as_slice())
}

fn code_is_empty_or_eip7702(code: &[u8]) -> bool {
    code.is_empty() || (code.len() == 23 && code.starts_with(&[0xEF, 0x01, 0x00]))
}

/// Returns gas refund to add to the transaction refund counter (see revm `apply_eip7702_auth_list`).
pub async fn apply_authorization_list(
    tx: &Tx,
    chain_id: u64,
    state: &mut impl State,
    chain: &impl Chain,
) -> Result<i64> {
    let mut refund = 0i64;
    for item in &tx.authorization_list {
        let signed = SignedAuthorization::new_unchecked(
            Authorization {
                chain_id: int_to_u256(&item.chain_id),
                address: Address::from_slice(item.address.as_ref()),
                nonce: item.nonce.as_u64(),
            },
            item.y_parity.as_u8(),
            int_to_u256(&item.r),
            int_to_u256(&item.s),
        );
        let authority = match signed.recover_authority() {
            Ok(a) => acc_from_address(a),
            Err(_) => continue,
        };

        let cid = int_to_u256(&item.chain_id);
        if !cid.is_zero() && cid != U256::from(chain_id) {
            continue;
        }
        if item.nonce.as_u64() == u64::MAX {
            continue;
        }

        if state.acc(&authority).is_none() {
            fetch(crate::Fetch::Account(authority), state, chain).await?;
        }
        let acc = state.acc(&authority).unwrap_or_default();
        state.warm_acc(&authority);

        let code = acc.code.0.as_slice();
        if !code_is_empty_or_eip7702(code) {
            continue;
        }

        if item.nonce.as_u64() != acc.nonce.as_u64() {
            continue;
        }

        // Revm: refund if authority already exists in the trie (not "empty + never existed").
        if !account_is_empty_for_eip7702_refund(&acc) {
            refund += PER_EMPTY_ACCOUNT - PER_AUTH_BASE;
        }

        if item.address.is_zero() {
            let hash = Int::from(yaevmi_misc::keccak256(&[]).as_ref());
            state.set_code(&authority, Buf::default(), hash);
        } else {
            state.set_auth(&authority, &item.address);
            if state.acc(&item.address).is_none() {
                fetch(crate::Fetch::Account(item.address), state, chain).await?;
            }
        }
        state.inc_nonce(&authority, Int::ONE);
    }
    Ok(refund)
}

fn account_is_empty_for_eip7702_refund(acc: &Account) -> bool {
    acc.value.is_zero() && acc.nonce.is_zero() && acc.code.0.0.is_empty()
}
