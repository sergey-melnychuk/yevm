use eyre::{Result, eyre};
use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use yevm_base::{Acc, Int};
use yevm_core::call::{AccessListItem, AuthorizationListItem, Call, Tx};
use yevm_misc::{buf::Buf, hex::Hex, keccak256};

pub struct DecodedTx {
    pub call: Call,
    pub tx: Tx,
}

pub fn decode_raw(raw: &str) -> Result<DecodedTx> {
    let bytes = hex_decode(raw)?;
    if bytes.is_empty() {
        return Err(eyre!("empty tx"));
    }
    let hash: Int = keccak256(&bytes);
    match bytes[0] {
        b if b >= 0xc0 => decode_legacy(&bytes, hash),
        0x01 => decode_type1(&bytes[1..], hash),
        0x02 => decode_type2(&bytes[1..], hash),
        0x03 => decode_type3(&bytes[1..], hash),
        0x04 => decode_type4(&bytes[1..], hash),
        t => Err(eyre!("unsupported tx type: 0x{t:02x}")),
    }
}

// Legacy (type 0): RLP([nonce, gasPrice, gasLimit, to, value, data, v, r, s])
fn decode_legacy(bytes: &[u8], hash: Int) -> Result<DecodedTx> {
    let items = rlp_list(bytes)?;
    if items.len() < 9 {
        return Err(eyre!("legacy tx: expected 9 fields, got {}", items.len()));
    }
    let nonce = be_u64(&items[0])?;
    let gas_price = items[1].clone();
    let gas_limit = be_u64(&items[2])?;
    let to = to_addr(&items[3])?;
    let value = items[4].clone();
    let data = items[5].clone();
    let v = be_u64(&items[6])?;
    let r = items[7].clone();
    let s = items[8].clone();

    let (chain_id, recovery_id) = if v >= 35 {
        let chain_id = (v - 35) / 2;
        (chain_id, (v - 35 - chain_id * 2) as u8)
    } else {
        (0u64, (v - 27) as u8)
    };

    let signing_hash = if chain_id > 0 {
        let payload = rlp_encode_list(&[
            &items[0],
            &gas_price,
            &items[2],
            &items[3],
            &value,
            &data,
            &be_bytes(chain_id),
            &[],
            &[],
        ]);
        keccak256(&payload)
    } else {
        let payload =
            rlp_encode_list(&[&items[0], &gas_price, &items[2], &items[3], &value, &data]);
        keccak256(&payload)
    };

    let from = ecrecover(signing_hash.as_ref(), recovery_id, &r, &s)?;
    Ok(DecodedTx {
        call: Call {
            by: from,
            to,
            gas: gas_limit,
            eth: to_int(&value),
            data: Buf::from(data),
        },
        tx: Tx {
            chain_id: Hex::from(&be_bytes(chain_id)[..]),
            nonce: Int::from(nonce),
            gas_price: to_int(&gas_price),
            max_fee_per_gas: Int::ZERO,
            max_priority_fee_per_gas: Int::ZERO,
            access_list: vec![],
            authorization_list: vec![],
            blob_versioned_hashes: vec![],
            max_fee_per_blob_gas: None,
            hash,
            index: Int::ZERO,
        },
    })
}

// EIP-2930 (type 1): RLP([chainId, nonce, gasPrice, gasLimit, to, value, data, accessList, v, r, s])
fn decode_type1(bytes: &[u8], hash: Int) -> Result<DecodedTx> {
    let items = rlp_list(bytes)?;
    let raw = rlp_raw_items(bytes)?;
    if items.len() < 11 {
        return Err(eyre!("type1 tx: expected 11 fields, got {}", items.len()));
    }
    let chain_id = be_u64(&items[0])?;
    let nonce = be_u64(&items[1])?;
    let gas_price = items[2].clone();
    let gas_limit = be_u64(&items[3])?;
    let to = to_addr(&items[4])?;
    let value = items[5].clone();
    let data = items[6].clone();
    let v = be_u64(&items[8])? as u8;
    let r = items[9].clone();
    let s = items[10].clone();

    let mut sig_payload = vec![0x01u8];
    sig_payload.extend(rlp_list_raw(&raw[..8])); // fields 0-7 including access_list
    let signing_hash = keccak256(&sig_payload);

    let from = ecrecover(signing_hash.as_ref(), v, &r, &s)?;
    let access_list = decode_access_list(&raw[7])?;
    Ok(DecodedTx {
        call: Call {
            by: from,
            to,
            gas: gas_limit,
            eth: to_int(&value),
            data: Buf::from(data),
        },
        tx: Tx {
            chain_id: Hex::from(&be_bytes(chain_id)[..]),
            nonce: Int::from(nonce),
            gas_price: to_int(&gas_price),
            max_fee_per_gas: Int::ZERO,
            max_priority_fee_per_gas: Int::ZERO,
            access_list,
            authorization_list: vec![],
            blob_versioned_hashes: vec![],
            max_fee_per_blob_gas: None,
            hash,
            index: Int::ZERO,
        },
    })
}

// EIP-1559 (type 2): RLP([chainId, nonce, maxPriority, maxFee, gasLimit, to, value, data, accessList, v, r, s])
fn decode_type2(bytes: &[u8], hash: Int) -> Result<DecodedTx> {
    let items = rlp_list(bytes)?;
    let raw = rlp_raw_items(bytes)?;
    if items.len() < 12 {
        return Err(eyre!("type2 tx: expected 12 fields, got {}", items.len()));
    }
    let chain_id = be_u64(&items[0])?;
    let nonce = be_u64(&items[1])?;
    let max_priority = items[2].clone();
    let max_fee = items[3].clone();
    let gas_limit = be_u64(&items[4])?;
    let to = to_addr(&items[5])?;
    let value = items[6].clone();
    let data = items[7].clone();
    let v = be_u64(&items[9])? as u8;
    let r = items[10].clone();
    let s = items[11].clone();

    let mut sig_payload = vec![0x02u8];
    sig_payload.extend(rlp_list_raw(&raw[..9])); // fields 0-8 including access_list
    let signing_hash = keccak256(&sig_payload);

    let from = ecrecover(signing_hash.as_ref(), v, &r, &s)?;
    let access_list = decode_access_list(&raw[8])?;
    Ok(DecodedTx {
        call: Call {
            by: from,
            to,
            gas: gas_limit,
            eth: to_int(&value),
            data: Buf::from(data),
        },
        tx: Tx {
            chain_id: Hex::from(&be_bytes(chain_id)[..]),
            nonce: Int::from(nonce),
            gas_price: to_int(&max_fee),
            max_fee_per_gas: to_int(&max_fee),
            max_priority_fee_per_gas: to_int(&max_priority),
            access_list,
            authorization_list: vec![],
            blob_versioned_hashes: vec![],
            max_fee_per_blob_gas: None,
            hash,
            index: Int::ZERO,
        },
    })
}

// EIP-4844 (type 3): chain_id, nonce, maxPriority, maxFee, gasLimit, to, value, data, accessList, maxFeePerBlobGas, blobVersionedHashes, v, r, s
fn decode_type3(bytes: &[u8], hash: Int) -> Result<DecodedTx> {
    let items = rlp_list(bytes)?;
    let raw = rlp_raw_items(bytes)?;
    if items.len() < 14 {
        return Err(eyre!("type3 tx: expected 14 fields, got {}", items.len()));
    }
    let chain_id = be_u64(&items[0])?;
    let nonce = be_u64(&items[1])?;
    let max_priority = items[2].clone();
    let max_fee = items[3].clone();
    let gas_limit = be_u64(&items[4])?;
    let to = to_addr(&items[5])?;
    let value = items[6].clone();
    let data = items[7].clone();
    // items[8]  = access_list
    // items[9]  = max_fee_per_blob_gas
    // items[10] = blob_versioned_hashes
    let v = be_u64(&items[11])? as u8;
    let r = items[12].clone();
    let s = items[13].clone();

    let mut sig_payload = vec![0x03u8];
    sig_payload.extend(rlp_list_raw(&raw[..11]));
    let signing_hash = keccak256(&sig_payload);

    let from = ecrecover(signing_hash.as_ref(), v, &r, &s)?;
    let access_list = decode_access_list(&raw[8])?;
    let blob_versioned_hashes = decode_hash_list(&raw[10])?;

    Ok(DecodedTx {
        call: Call {
            by: from,
            to,
            gas: gas_limit,
            eth: to_int(&value),
            data: Buf::from(data),
        },
        tx: Tx {
            chain_id: Hex::from(&be_bytes(chain_id)[..]),
            nonce: Int::from(nonce),
            gas_price: to_int(&max_fee),
            max_fee_per_gas: to_int(&max_fee),
            max_priority_fee_per_gas: to_int(&max_priority),
            access_list,
            authorization_list: vec![],
            blob_versioned_hashes,
            max_fee_per_blob_gas: Some(to_int(&items[9])),
            hash,
            index: Int::ZERO,
        },
    })
}

// EIP-7702 (type 4): chain_id, nonce, maxPriority, maxFee, gasLimit, to, value, data, accessList, authorizationList, v, r, s
fn decode_type4(bytes: &[u8], hash: Int) -> Result<DecodedTx> {
    let items = rlp_list(bytes)?;
    let raw = rlp_raw_items(bytes)?;
    if items.len() < 13 {
        return Err(eyre!("type4 tx: expected 13 fields, got {}", items.len()));
    }
    let chain_id = be_u64(&items[0])?;
    let nonce = be_u64(&items[1])?;
    let max_priority = items[2].clone();
    let max_fee = items[3].clone();
    let gas_limit = be_u64(&items[4])?;
    let to = to_addr(&items[5])?;
    let value = items[6].clone();
    let data = items[7].clone();
    // items[8] = access_list
    // items[9] = authorization_list
    let v = be_u64(&items[10])? as u8;
    let r = items[11].clone();
    let s = items[12].clone();

    let mut sig_payload = vec![0x04u8];
    sig_payload.extend(rlp_list_raw(&raw[..10]));
    let signing_hash = keccak256(&sig_payload);

    let from = ecrecover(signing_hash.as_ref(), v, &r, &s)?;
    let access_list = decode_access_list(&raw[8])?;
    let authorization_list = decode_authorization_list(&raw[9])?;

    Ok(DecodedTx {
        call: Call {
            by: from,
            to,
            gas: gas_limit,
            eth: to_int(&value),
            data: Buf::from(data),
        },
        tx: Tx {
            chain_id: Hex::from(&be_bytes(chain_id)[..]),
            nonce: Int::from(nonce),
            gas_price: to_int(&max_fee),
            max_fee_per_gas: to_int(&max_fee),
            max_priority_fee_per_gas: to_int(&max_priority),
            access_list,
            authorization_list,
            blob_versioned_hashes: vec![],
            max_fee_per_blob_gas: None,
            hash,
            index: Int::ZERO,
        },
    })
}

fn decode_access_list(raw_list: &[u8]) -> Result<Vec<AccessListItem>> {
    let entry_raws = rlp_raw_items(raw_list)?;
    entry_raws
        .iter()
        .map(|entry_raw| {
            let field_raws = rlp_raw_items(entry_raw)?;
            if field_raws.len() < 2 {
                return Err(eyre!("access list entry: expected 2 fields"));
            }
            let (addr_bytes, _) = rlp_item(&field_raws[0])?;
            let address =
                to_addr(&addr_bytes)?.ok_or_else(|| eyre!("access list: missing address"))?;
            let key_raws = rlp_raw_items(&field_raws[1])?;
            let storage_keys = key_raws
                .iter()
                .map(|kr| {
                    let (key_bytes, _) = rlp_item(kr)?;
                    Ok(to_int(&key_bytes))
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(AccessListItem {
                address,
                storage_keys,
            })
        })
        .collect()
}

fn decode_hash_list(raw_list: &[u8]) -> Result<Vec<Int>> {
    let items = rlp_list(raw_list)?;
    Ok(items.iter().map(|b| to_int(b)).collect())
}

// Authorization item: [chain_id, address, nonce, y_parity, r, s]
fn decode_authorization_list(raw_list: &[u8]) -> Result<Vec<AuthorizationListItem>> {
    let entry_raws = rlp_raw_items(raw_list)?;
    entry_raws
        .iter()
        .map(|entry_raw| {
            let f = rlp_list(entry_raw)?;
            if f.len() < 6 {
                return Err(eyre!(
                    "authorization entry: expected 6 fields, got {}",
                    f.len()
                ));
            }
            Ok(AuthorizationListItem {
                chain_id: to_int(&f[0]),
                address: to_addr(&f[1])?.ok_or_else(|| eyre!("authorization: missing address"))?,
                nonce: to_int(&f[2]),
                y_parity: to_int(&f[3]),
                r: to_int(&f[4]),
                s: to_int(&f[5]),
            })
        })
        .collect()
}

fn ecrecover(hash: &[u8], recovery_id: u8, r: &[u8], s: &[u8]) -> Result<Acc> {
    let rid = RecoveryId::try_from(recovery_id).map_err(|e| eyre!("invalid recovery id: {e}"))?;
    let mut r32 = [0u8; 32];
    let mut s32 = [0u8; 32];
    r32[32 - r.len()..].copy_from_slice(r);
    s32[32 - s.len()..].copy_from_slice(s);
    let sig = Signature::from_scalars(r32, s32).map_err(|e| eyre!("invalid sig: {e}"))?;
    let (sig, rid) = if let Some(norm) = sig.normalize_s() {
        (norm, RecoveryId::new(!rid.is_y_odd(), rid.is_x_reduced()))
    } else {
        (sig, rid)
    };
    let key =
        VerifyingKey::recover_from_prehash(hash, &sig, rid).map_err(|e| eyre!("ecrecover: {e}"))?;
    let point = key.to_encoded_point(false);
    let h = keccak256(&point.as_bytes()[1..]);
    Ok(Acc::from(&h.as_ref()[12..]))
}

/// Decoded payload bytes for each item in an RLP list (without item prefixes).
fn rlp_list(data: &[u8]) -> Result<Vec<Vec<u8>>> {
    let (payload, _) = rlp_payload(data)?;
    let mut items = vec![];
    let mut i = 0;
    while i < payload.len() {
        let (item, consumed) = rlp_item(&payload[i..])?;
        items.push(item);
        i += consumed;
    }
    Ok(items)
}

/// Full raw RLP bytes for each item in an RLP list (including item prefixes).
fn rlp_raw_items(data: &[u8]) -> Result<Vec<Vec<u8>>> {
    let (payload, _) = rlp_payload(data)?;
    let mut items = vec![];
    let mut i = 0;
    while i < payload.len() {
        let (_, consumed) = rlp_item(&payload[i..])?;
        items.push(payload[i..i + consumed].to_vec());
        i += consumed;
    }
    Ok(items)
}

/// Wrap pre-encoded RLP items into a list.
fn rlp_list_raw(items: &[Vec<u8>]) -> Vec<u8> {
    let payload: Vec<u8> = items.iter().flatten().copied().collect();
    let mut out = encode_length(payload.len(), 0xc0);
    out.extend(payload);
    out
}

/// Returns (raw_bytes_of_item, bytes_consumed) for one RLP item.
fn rlp_item(data: &[u8]) -> Result<(Vec<u8>, usize)> {
    let b = *data.first().ok_or_else(|| eyre!("rlp: unexpected end"))?;
    match b {
        0x00..=0x7f => Ok((vec![b], 1)),
        0x80 => Ok((vec![], 1)),
        0x81..=0xb7 => {
            let len = (b - 0x80) as usize;
            Ok((data[1..1 + len].to_vec(), 1 + len))
        }
        0xb8..=0xbf => {
            let len_len = (b - 0xb7) as usize;
            let len = be_usize(&data[1..1 + len_len]);
            Ok((
                data[1 + len_len..1 + len_len + len].to_vec(),
                1 + len_len + len,
            ))
        }
        // List: return raw bytes of the whole list item (for nested lists like access_list)
        0xc0..=0xf7 => {
            let len = (b - 0xc0) as usize;
            Ok((data[1..1 + len].to_vec(), 1 + len))
        }
        0xf8..=0xff => {
            let len_len = (b - 0xf7) as usize;
            let len = be_usize(&data[1..1 + len_len]);
            Ok((
                data[1 + len_len..1 + len_len + len].to_vec(),
                1 + len_len + len,
            ))
        }
    }
}

/// Returns the payload slice of the outermost list (skips the list prefix).
fn rlp_payload(data: &[u8]) -> Result<(&[u8], usize)> {
    let b = *data.first().ok_or_else(|| eyre!("rlp: empty"))?;
    match b {
        0xc0..=0xf7 => {
            let len = (b - 0xc0) as usize;
            Ok((&data[1..1 + len], 1 + len))
        }
        0xf8..=0xff => {
            let len_len = (b - 0xf7) as usize;
            let len = be_usize(&data[1..1 + len_len]);
            Ok((&data[1 + len_len..1 + len_len + len], 1 + len_len + len))
        }
        _ => Err(eyre!("rlp: expected list prefix, got 0x{b:02x}")),
    }
}

fn rlp_encode_bytes(b: &[u8]) -> Vec<u8> {
    if b.len() == 1 && b[0] < 0x80 {
        return b.to_vec();
    }
    if b.is_empty() {
        return vec![0x80];
    }
    let mut out = encode_length(b.len(), 0x80);
    out.extend_from_slice(b);
    out
}

fn rlp_encode_list(items: &[&[u8]]) -> Vec<u8> {
    let payload: Vec<u8> = items.iter().flat_map(|i| rlp_encode_bytes(i)).collect();
    let mut out = encode_length(payload.len(), 0xc0);
    out.extend(payload);
    out
}

fn encode_length(len: usize, offset: u8) -> Vec<u8> {
    if len <= 55 {
        vec![offset + len as u8]
    } else {
        let lb = be_bytes_minimal(len);
        let mut out = vec![offset + 55 + lb.len() as u8];
        out.extend(lb);
        out
    }
}

fn be_u64(b: &[u8]) -> Result<u64> {
    if b.len() > 8 {
        return Err(eyre!("value too large for u64: {} bytes", b.len()));
    }
    let mut arr = [0u8; 8];
    arr[8 - b.len()..].copy_from_slice(b);
    Ok(u64::from_be_bytes(arr))
}

fn be_usize(b: &[u8]) -> usize {
    let mut v = 0usize;
    for &x in b {
        v = (v << 8) | x as usize;
    }
    v
}

fn be_bytes(v: u64) -> Vec<u8> {
    let b = v.to_be_bytes();
    let skip = b.iter().take_while(|&&x| x == 0).count();
    b[skip..].to_vec()
}

fn be_bytes_minimal(v: usize) -> Vec<u8> {
    let b = v.to_be_bytes();
    let skip = b.iter().take_while(|&&x| x == 0).count();
    b[skip..].to_vec()
}

fn to_addr(b: &[u8]) -> Result<Option<Acc>> {
    match b.len() {
        0 => Ok(None),
        20 => Ok(Some(Acc::from(b))),
        n => Err(eyre!("invalid address length: {n}")),
    }
}

fn to_int(b: &[u8]) -> Int {
    if b.len() > 32 {
        return Int::ZERO;
    }
    Int::from(b)
}

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    hex::decode(s).map_err(|e| eyre!("hex decode: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn roundtrip_type3_known_key() {
        use k256::ecdsa::SigningKey;
        use yevm_misc::keccak256;

        let privkey =
            hex::decode("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80")
                .unwrap();
        let signing_key = SigningKey::from_slice(&privkey).unwrap();

        let to_bytes = hex::decode("d3cda913deb6f4967b2ef3aa68f5a843aaba4cc3").unwrap();
        let chain_id_enc = rlp_encode_bytes(&be_bytes(1u64));
        let nonce_enc = rlp_encode_bytes(&be_bytes(5u64));
        let max_pri_enc = rlp_encode_bytes(&be_bytes(1_000_000_000u64));
        let max_fee_enc = rlp_encode_bytes(&be_bytes(20_000_000_000u64));
        let gas_limit_enc = rlp_encode_bytes(&be_bytes(21_000u64));
        let to_enc = rlp_encode_bytes(&to_bytes);
        let value_enc = rlp_encode_bytes(&be_bytes(0u64));
        let data_enc = rlp_encode_bytes(&[]);
        let access_list_enc = vec![0xc0u8];
        let max_fee_blob_enc = rlp_encode_bytes(&be_bytes(10u64));
        // two fake blob hashes
        let blob_hash1 = [0x01u8; 32];
        let blob_hash2 = [0x02u8; 32];
        let blob_hashes_enc = {
            let h1 = rlp_encode_bytes(&blob_hash1);
            let h2 = rlp_encode_bytes(&blob_hash2);
            let mut combined = h1;
            combined.extend(h2);
            let mut out = encode_length(combined.len(), 0xc0);
            out.extend(combined);
            out
        };

        let unsigned = vec![
            chain_id_enc.clone(),
            nonce_enc.clone(),
            max_pri_enc.clone(),
            max_fee_enc.clone(),
            gas_limit_enc.clone(),
            to_enc.clone(),
            value_enc.clone(),
            data_enc.clone(),
            access_list_enc.clone(),
            max_fee_blob_enc.clone(),
            blob_hashes_enc.clone(),
        ];
        let mut signing_payload = vec![0x03u8];
        signing_payload.extend(rlp_list_raw(&unsigned));
        let signing_hash = keccak256(&signing_payload);

        let (sig, rid) = signing_key
            .sign_prehash_recoverable(signing_hash.as_ref())
            .unwrap();
        let sig_bytes = sig.to_bytes();
        let v = rid.to_byte() as u64;

        let mut all_items = unsigned;
        all_items.push(rlp_encode_bytes(&be_bytes(v)));
        all_items.push(rlp_encode_bytes(&sig_bytes[..32]));
        all_items.push(rlp_encode_bytes(&sig_bytes[32..]));

        let mut raw_tx = vec![0x03u8];
        raw_tx.extend(rlp_list_raw(&all_items));
        let raw_hex = format!("0x{}", hex::encode(&raw_tx));

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
        use k256::ecdsa::SigningKey;
        use yevm_misc::keccak256;

        let privkey =
            hex::decode("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80")
                .unwrap();
        let signing_key = SigningKey::from_slice(&privkey).unwrap();

        let to_bytes = hex::decode("d3cda913deb6f4967b2ef3aa68f5a843aaba4cc3").unwrap();
        let auth_addr = hex::decode("1111111111111111111111111111111111111111").unwrap();
        let chain_id_enc = rlp_encode_bytes(&be_bytes(1u64));
        let nonce_enc = rlp_encode_bytes(&be_bytes(7u64));
        let max_pri_enc = rlp_encode_bytes(&be_bytes(1_000_000_000u64));
        let max_fee_enc = rlp_encode_bytes(&be_bytes(20_000_000_000u64));
        let gas_limit_enc = rlp_encode_bytes(&be_bytes(50_000u64));
        let to_enc = rlp_encode_bytes(&to_bytes);
        let value_enc = rlp_encode_bytes(&be_bytes(0u64));
        let data_enc = rlp_encode_bytes(&[]);
        let access_list_enc = vec![0xc0u8];
        // one authorization: [chain_id=1, address, nonce=0, y_parity=0, r=1, s=1]
        let auth_item = rlp_list_raw(&[
            rlp_encode_bytes(&be_bytes(1u64)),
            rlp_encode_bytes(&auth_addr),
            rlp_encode_bytes(&be_bytes(0u64)),
            rlp_encode_bytes(&be_bytes(0u64)),
            rlp_encode_bytes(&[0x01u8; 32]),
            rlp_encode_bytes(&[0x01u8; 32]),
        ]);
        let auth_list_enc = {
            let mut out = encode_length(auth_item.len(), 0xc0);
            out.extend(auth_item);
            out
        };

        let unsigned = vec![
            chain_id_enc.clone(),
            nonce_enc.clone(),
            max_pri_enc.clone(),
            max_fee_enc.clone(),
            gas_limit_enc.clone(),
            to_enc.clone(),
            value_enc.clone(),
            data_enc.clone(),
            access_list_enc.clone(),
            auth_list_enc.clone(),
        ];
        let mut signing_payload = vec![0x04u8];
        signing_payload.extend(rlp_list_raw(&unsigned));
        let signing_hash = keccak256(&signing_payload);

        let (sig, rid) = signing_key
            .sign_prehash_recoverable(signing_hash.as_ref())
            .unwrap();
        let sig_bytes = sig.to_bytes();
        let v = rid.to_byte() as u64;

        let mut all_items = unsigned;
        all_items.push(rlp_encode_bytes(&be_bytes(v)));
        all_items.push(rlp_encode_bytes(&sig_bytes[..32]));
        all_items.push(rlp_encode_bytes(&sig_bytes[32..]));

        let mut raw_tx = vec![0x04u8];
        raw_tx.extend(rlp_list_raw(&all_items));
        let raw_hex = format!("0x{}", hex::encode(&raw_tx));

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
    fn roundtrip_type2_known_key() {
        use k256::ecdsa::SigningKey;
        use yevm_misc::keccak256;

        // Hardhat/Anvil account 0: publicly known test key
        let privkey =
            hex::decode("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80")
                .unwrap();
        let signing_key = SigningKey::from_slice(&privkey).unwrap();

        // Verify address derivation
        let point = signing_key.verifying_key().to_encoded_point(false);
        let h = keccak256(&point.as_bytes()[1..]);
        assert_eq!(
            format!("0x{}", hex::encode(&h.as_ref()[12..])),
            "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266",
        );

        // Encode EIP-1559 tx fields
        let to_bytes = hex::decode("d3cda913deb6f4967b2ef3aa68f5a843aaba4cc3").unwrap();
        let chain_id_enc = rlp_encode_bytes(&be_bytes(1u64));
        let nonce_enc = rlp_encode_bytes(&be_bytes(0u64));
        let max_pri_enc = rlp_encode_bytes(&be_bytes(1_000_000_000u64));
        let max_fee_enc = rlp_encode_bytes(&be_bytes(20_000_000_000u64));
        let gas_limit_enc = rlp_encode_bytes(&be_bytes(21_000u64));
        let to_enc = rlp_encode_bytes(&to_bytes);
        let value_enc = rlp_encode_bytes(&be_bytes(0u64));
        let data_enc = rlp_encode_bytes(&[]);
        let access_list_enc = vec![0xc0u8]; // empty RLP list

        // signing hash: 0x02 || RLP([chain_id..access_list])
        let unsigned = vec![
            chain_id_enc.clone(),
            nonce_enc.clone(),
            max_pri_enc.clone(),
            max_fee_enc.clone(),
            gas_limit_enc.clone(),
            to_enc.clone(),
            value_enc.clone(),
            data_enc.clone(),
            access_list_enc.clone(),
        ];
        let mut signing_payload = vec![0x02u8];
        signing_payload.extend(rlp_list_raw(&unsigned));
        let signing_hash = keccak256(&signing_payload);

        let (sig, rid) = signing_key
            .sign_prehash_recoverable(signing_hash.as_ref())
            .unwrap();
        let sig_bytes = sig.to_bytes();
        let v = rid.to_byte() as u64;

        let v_enc = rlp_encode_bytes(&be_bytes(v));
        let r_enc = rlp_encode_bytes(&sig_bytes[..32]);
        let s_enc = rlp_encode_bytes(&sig_bytes[32..]);

        // full signed tx: 0x02 || RLP([chain_id..s])
        let all_items = vec![
            chain_id_enc,
            nonce_enc,
            max_pri_enc,
            max_fee_enc,
            gas_limit_enc,
            to_enc,
            value_enc,
            data_enc,
            access_list_enc,
            v_enc,
            r_enc,
            s_enc,
        ];
        let mut raw_tx = vec![0x02u8];
        raw_tx.extend(rlp_list_raw(&all_items));
        let raw_hex = format!("0x{}", hex::encode(&raw_tx));

        let decoded = decode_raw(&raw_hex).expect("decode_raw failed");
        assert_eq!(
            format!("{}", decoded.call.by),
            "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266",
        );
    }
}
