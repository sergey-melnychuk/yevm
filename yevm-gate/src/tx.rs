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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{SignableTransaction, TxEip1559, TxEip4844, TxEip7702, TxLegacy};
    use alloy_eips::{
        eip2718::Encodable2718,
        eip2930::AccessList,
        eip7702::{Authorization, SignedAuthorization},
    };
    use alloy_primitives::{Address, Signature, TxKind, U256};
    use k256::ecdsa::SigningKey;

    fn test_signing_key() -> SigningKey {
        let privkey =
            hex::decode("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80")
                .unwrap();
        SigningKey::from_slice(&privkey).unwrap()
    }

    fn sign<T: SignableTransaction<Signature>>(
        tx: T,
        key: &SigningKey,
    ) -> alloy_consensus::Signed<T> {
        let sig_hash = tx.signature_hash();
        let (sig, rid) = key.sign_prehash_recoverable(sig_hash.as_slice()).unwrap();
        let sig_bytes = sig.to_bytes();
        let signature = Signature::new(
            U256::from_be_slice(&sig_bytes[..32]),
            U256::from_be_slice(&sig_bytes[32..]),
            rid.is_y_odd(),
        );
        tx.into_signed(signature)
    }

    // Mainnet tx 0x1322a406eb0afdd59438c6876fa15e7ae73f834b311906ab8f0a2ac0a9838981
    // type 2 (EIP-1559), from 0x74577e960439402367eafe2de2ca1cae4ae3987c
    const RAW_TYPE2: &str = "0x02f89001827ec0841a492040842d8181b38288b894681e908b8ab57c49c74d770f369754ccc3e1ae0980a469fbadc9876991113b005ed8775e2b0005f5e8fe070000000000fafa00019dff1ed07500c080a00bb06ff370660cc9b5cf19fddc214ee503bb758c81c5b965e02b5ef5f4ebc9c6a073561d200f896987ac21d285b596be54076fb87e7ffc490e5564465646bfb7d4";
    #[test]
    fn type2_from_address() {
        let decoded = decode_raw(RAW_TYPE2).expect("decode failed");
        assert_eq!(
            format!("{}", decoded.call.by),
            "0x74577e960439402367eafe2de2ca1cae4ae3987c"
        );
    }

    #[test]
    fn type2_fields() {
        let decoded = decode_raw(RAW_TYPE2).expect("decode failed");
        let to = decoded.call.to.expect("to should be present");
        assert_eq!(
            format!("{to}"),
            "0x681e908b8ab57c49c74d770f369754ccc3e1ae09"
        );
        assert_eq!(decoded.call.gas, 0x88b8);
        assert_eq!(decoded.tx.nonce.as_u64(), 0x7ec0);
        assert_eq!(decoded.tx.chain_id.as_ref(), &[0, 0, 0, 0, 0, 0, 0, 1]);
    }

    #[test]
    fn type2_tx_hash() {
        let decoded = decode_raw(RAW_TYPE2).expect("decode failed");
        assert_eq!(
            format!("{}", decoded.tx.hash),
            "0x1322a406eb0afdd59438c6876fa15e7ae73f834b311906ab8f0a2ac0a9838981",
        );
    }

    #[test]
    fn roundtrip_type2_known_key() {
        let key = test_signing_key();
        let to =
            Address::from_slice(&hex::decode("d3cda913deb6f4967b2ef3aa68f5a843aaba4cc3").unwrap());
        let tx = TxEip1559 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 20_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: TxKind::Call(to),
            value: U256::ZERO,
            access_list: AccessList::default(),
            input: Default::default(),
        };
        let signed = sign(tx, &key);
        let envelope: TxEnvelope = signed.into();
        let raw_hex = format!("0x{}", hex::encode(envelope.encoded_2718()));
        let decoded = decode_raw(&raw_hex).expect("decode_raw failed");
        assert_eq!(
            format!("{}", decoded.call.by),
            "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266",
        );
    }

    #[test]
    fn roundtrip_type3_known_key() {
        let key = test_signing_key();
        let to =
            Address::from_slice(&hex::decode("d3cda913deb6f4967b2ef3aa68f5a843aaba4cc3").unwrap());
        let tx = TxEip4844 {
            chain_id: 1,
            nonce: 5,
            gas_limit: 21_000,
            max_fee_per_gas: 20_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to,
            value: U256::ZERO,
            access_list: AccessList::default(),
            blob_versioned_hashes: vec![
                alloy_primitives::B256::from([0x01u8; 32]),
                alloy_primitives::B256::from([0x02u8; 32]),
            ],
            max_fee_per_blob_gas: 10,
            input: Default::default(),
        };
        let signed = sign(tx, &key);
        let envelope: TxEnvelope = signed.into();
        let raw_hex = format!("0x{}", hex::encode(envelope.encoded_2718()));
        let decoded = decode_raw(&raw_hex).expect("decode_raw failed");
        assert_eq!(
            format!("{}", decoded.call.by),
            "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266",
        );
        assert_eq!(decoded.tx.blob_versioned_hashes.len(), 2);
        assert_eq!(decoded.tx.max_fee_per_blob_gas, Some(Int::from(10u64)));
    }

    #[test]
    fn roundtrip_type4_known_key() {
        let key = test_signing_key();
        let to =
            Address::from_slice(&hex::decode("d3cda913deb6f4967b2ef3aa68f5a843aaba4cc3").unwrap());
        let auth_addr =
            Address::from_slice(&hex::decode("1111111111111111111111111111111111111111").unwrap());
        let auth = Authorization {
            chain_id: U256::from(1u64),
            address: auth_addr,
            nonce: 0,
        };
        let auth_sig = Signature::new(U256::from(1u64), U256::from(1u64), false);
        let signed_auth = SignedAuthorization::new_unchecked(
            auth,
            auth_sig.v() as u8,
            auth_sig.r(),
            auth_sig.s(),
        );
        let tx = TxEip7702 {
            chain_id: 1,
            nonce: 7,
            gas_limit: 50_000,
            max_fee_per_gas: 20_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to,
            value: U256::ZERO,
            access_list: AccessList::default(),
            authorization_list: vec![signed_auth],
            input: Default::default(),
        };
        let signed = sign(tx, &key);
        let envelope: TxEnvelope = signed.into();
        let raw_hex = format!("0x{}", hex::encode(envelope.encoded_2718()));
        let decoded = decode_raw(&raw_hex).expect("decode_raw failed");
        assert_eq!(
            format!("{}", decoded.call.by),
            "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266",
        );
        assert_eq!(decoded.tx.authorization_list.len(), 1);
        assert_eq!(
            format!("{}", decoded.tx.authorization_list[0].address),
            "0x1111111111111111111111111111111111111111",
        );
    }

    #[test]
    fn roundtrip_legacy_known_key() {
        let key = test_signing_key();
        let to =
            Address::from_slice(&hex::decode("d3cda913deb6f4967b2ef3aa68f5a843aaba4cc3").unwrap());
        let tx = TxLegacy {
            chain_id: Some(1),
            nonce: 0,
            gas_price: 20_000_000_000,
            gas_limit: 21_000,
            to: TxKind::Call(to),
            value: U256::ZERO,
            input: Default::default(),
        };
        let signed = sign(tx, &key);
        let envelope: TxEnvelope = signed.into();
        let raw_hex = format!("0x{}", hex::encode(envelope.encoded_2718()));
        let decoded = decode_raw(&raw_hex).expect("decode_raw failed");
        assert_eq!(
            format!("{}", decoded.call.by),
            "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266",
        );
        assert!(decoded.tx.max_fee_per_gas.is_zero());
    }

    const RAW_BYBIT: &str = "0xf9032b2a8502540be40083030d40941db92e2eebc8e0c075a02bea49a2935bcd2dfcf480b902c46a76120200000000000000000000000096221423681a6d52e184d440a8efcebb105c7242000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001400000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000b2b2000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001c00000000000000000000000000000000000000000000000000000000000000044a9059cbb000000000000000000000000bdd077f651ebe7f7b3ce16fe5f2b025be296951600000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c3d0afef78a52fd504479dc2af3dc401334762cbd05609c7ac18db9ec5abf4a07a5cc09fc86efd3489707b89b0c729faed616459189cb50084f208d03b201b001f1f0f62ad358d6b319d3c1221d44456080068fe02ae5b1a39b4afb1e6721ca7f9903ac523a801533f265231cd35fc2dfddc3bd9a9563b51315cf9d5ff23dc6d2c221fdf9e4b878877a8dbeee951a4a31ddbf1d3b71e127d5eda44b4730030114baba52e06dd23da37cd2a07a6e84f9950db867374a0f77558f42adf4409bfd569673c1f000000000000000000000000000000000000000000000000000000000025a0c06f155e9045c02891297148228ed69cc7167a6f8606f66a942ef75624c5906da03e9f83eae889e79e3af315c7e9a5e14b12f2bed9e23d994f751562ec7a4426b3";

    #[test]
    fn bybit_exploit_decodes() {
        let d = decode_raw(RAW_BYBIT).expect("decode failed");
        assert_eq!(
            format!("{}", d.tx.hash),
            "0x46deef0f52e3a983b67abf4714448a41dd7ffd6d32d32da69d62081c68ad7882",
        );
        // Executor EOA that submitted the multisig tx.
        assert_eq!(
            format!("{}", d.call.by),
            "0x0fa09c3a328792253f8dee7116848723b72a6d2e",
        );
        // Bybit cold-wallet Safe (the proxy whose implementation was swapped).
        assert_eq!(
            format!("{}", d.call.to.expect("to should be present")),
            "0x1db92e2eebc8e0c075a02bea49a2935bcd2dfcf4",
        );
        assert_eq!(d.tx.nonce.as_u64(), 42);
        assert_eq!(d.call.gas, 200_000);
    }
}
