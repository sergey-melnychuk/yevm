use alloy_consensus::{
    Transaction, TxEnvelope,
    transaction::{Recovered, SignerRecoverable},
};
use alloy_eips::eip2718::Decodable2718;
use eyre::{Result, eyre};
use yevm_base::{Acc, Int};
use yevm_core::call::{AccessListItem, AuthorizationListItem, Call, Tx};
use yevm_misc::buf::Buf;

pub struct DecodedTx {
    pub call: Call,
    pub tx: Tx,
}

pub fn decode_raw(raw: &str) -> Result<DecodedTx> {
    let bytes = hex_decode(raw)?;
    let envelope =
        TxEnvelope::decode_2718(&mut bytes.as_slice()).map_err(|e| eyre!("rlp: {e}"))?;
    let from = envelope
        .recover_signer()
        .map_err(|e| eyre!("ecrecover: {e}"))?;
    let recovered = Recovered::new_unchecked(envelope, from);

    let hash = Int::from(recovered.hash().as_slice());
    let by = Acc::from(recovered.signer().as_slice());
    let to = recovered.to().map(|a| Acc::from(a.as_slice()));

    let access_list = recovered
        .access_list()
        .map(|list| {
            list.iter()
                .map(|item| AccessListItem {
                    address: Acc::from(item.address.as_slice()),
                    storage_keys: item
                        .storage_keys
                        .iter()
                        .map(|k| Int::from(k.as_slice()))
                        .collect(),
                })
                .collect()
        })
        .unwrap_or_default();

    let authorization_list = match recovered.authorization_list() {
        Some(list) => list
            .iter()
            .map(|auth| {
                let sig = auth
                    .signature()
                    .map_err(|e| eyre!("authorization signature: {e}"))?;
                Ok(AuthorizationListItem {
                    chain_id: Int::from(&auth.chain_id.to_be_bytes::<32>()[..]),
                    address: Acc::from(auth.address.as_slice()),
                    nonce: Int::from(auth.nonce),
                    y_parity: Int::from(auth.y_parity()),
                    r: Int::from(&sig.r().to_be_bytes::<32>()[..]),
                    s: Int::from(&sig.s().to_be_bytes::<32>()[..]),
                })
            })
            .collect::<Result<Vec<_>>>()?,
        None => vec![],
    };

    let blob_versioned_hashes = recovered
        .blob_versioned_hashes()
        .map(|hashes| hashes.iter().map(|h| Int::from(h.as_slice())).collect())
        .unwrap_or_default();

    let (gas_price, max_fee_per_gas, max_priority_fee_per_gas) = match recovered.gas_price() {
        Some(gas_price) => (Int::from(gas_price), Int::ZERO, Int::ZERO),
        None => (
            Int::from(recovered.max_fee_per_gas()),
            Int::from(recovered.max_fee_per_gas()),
            Int::from(recovered.max_priority_fee_per_gas().unwrap_or_default()),
        ),
    };

    Ok(DecodedTx {
        call: Call {
            by,
            to,
            gas: recovered.gas_limit(),
            eth: Int::from(&recovered.value().to_be_bytes::<32>()[..]),
            data: Buf::from(recovered.input().to_vec()),
        },
        tx: Tx {
            chain_id: recovered.chain_id().unwrap_or_default().into(),
            nonce: Int::from(recovered.nonce()),
            gas_price,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            access_list,
            authorization_list,
            blob_versioned_hashes,
            max_fee_per_blob_gas: recovered.max_fee_per_blob_gas().map(Int::from),
            hash,
            index: Int::ZERO,
        },
    })
}

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    hex::decode(s).map_err(|e| eyre!("hex decode: {e}"))
}
