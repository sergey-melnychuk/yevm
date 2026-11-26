/// ecrecover (precompile 0x01)
/// Input:  hash[32] ++ v[32] ++ r[32] ++ s[32]  (padded to 128 bytes)
/// Output: zero-padded recovered address (32 bytes), or empty on invalid sig
/// Gas:    3000
pub fn ecrecover(input: &[u8], gas_limit: i64) -> (bool, Vec<u8>, i64) {
    const GAS: i64 = 3_000;
    if GAS > gas_limit {
        return (false, vec![], gas_limit);
    }

    let mut buf = [0u8; 128];
    let n = input.len().min(128);
    buf[..n].copy_from_slice(&input[..n]);

    // v is the whole 32-byte word at offset 32, which must be 27 or 28.
    let v = buf[63];
    if buf[32..63].iter().any(|&b| b != 0) || (v != 27 && v != 28) {
        return (true, vec![], GAS); // invalid v: success but empty output
    }

    let hash: &[u8; 32] = buf[0..32].try_into().unwrap();
    let sig: &[u8; 64] = buf[64..128].try_into().unwrap();
    let Some(key) = recover(hash, sig, v - 27) else {
        return (true, vec![], GAS);
    };

    let hash = yevm_misc::keccak256(&key);
    let mut out = [0u8; 32];
    out[12..].copy_from_slice(&hash.as_ref()[12..]);
    (true, out.to_vec(), GAS)
}

/// Recovers the public key (64 bytes, uncompressed without its prefix) that signed `hash` with
/// `sig` (r ++ s) and recovery id `rid` (0 or 1). Any s in [1, n-1] is accepted, high or low.
#[cfg(not(any(target_arch = "wasm32", target_os = "espidf")))]
fn recover(hash: &[u8; 32], sig: &[u8; 64], rid: u8) -> Option<[u8; 64]> {
    use secp256k1::{
        Message,
        ecdsa::{RecoverableSignature, RecoveryId},
    };
    let rid = RecoveryId::try_from(i32::from(rid)).ok()?;
    // Rejects r or s at or above the curve order; recovery rejects zero.
    let sig = RecoverableSignature::from_compact(sig, rid).ok()?;
    let key = sig.recover_ecdsa(Message::from_digest(*hash)).ok()?;
    key.serialize_uncompressed()[1..].try_into().ok()
}

/// See the native [`recover`]; k256, for the browser and ESP32 builds.
#[cfg(any(target_arch = "wasm32", target_os = "espidf"))]
fn recover(hash: &[u8; 32], sig: &[u8; 64], rid: u8) -> Option<[u8; 64]> {
    use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
    let rid = RecoveryId::try_from(rid).ok()?;
    let r: [u8; 32] = sig[..32].try_into().unwrap();
    let s: [u8; 32] = sig[32..].try_into().unwrap();
    let sig = Signature::from_scalars(r, s).ok()?;
    // k256 recovery requires low s: normalize it and flip the recovery id.
    let (sig, rid) = match sig.normalize_s() {
        Some(normalized) => (
            normalized,
            RecoveryId::new(!rid.is_y_odd(), rid.is_x_reduced()),
        ),
        None => (sig, rid),
    };
    let key = VerifyingKey::recover_from_prehash(hash, &sig, rid).ok()?;
    key.to_encoded_point(false).as_bytes()[1..].try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v_with_nonzero_high_bytes_is_invalid() {
        // go-ethereum's ecrecover precompile test vector, a valid signature with v = 28.
        let mut input = hex::decode(concat!(
            "456e9aea5e197a1f1af7a3e85a3212fa4049a3ba34c2289b4c860fc0b0c64ef3",
            "000000000000000000000000000000000000000000000000000000000000001c",
            "9242685bf161793cc25603c231bc2f568eb630ea16aa137d2664ac8038825608",
            "4f8ae3bd7535248d0bd448298cc2e2071e56992d0774dc340c368ae950852ada",
        ))
        .unwrap();
        let (ok, out, _) = ecrecover(&input, 3_000);
        assert!(ok);
        assert_eq!(
            hex::encode(&out[12..]),
            "7156526fbd7a3c72969b54f64e42c10fbb768c8a"
        );

        input[32] = 0xf0;
        assert_eq!(ecrecover(&input, 3_000), (true, vec![], 3_000));
    }

    #[test]
    fn high_s_recovers_like_its_low_s_twin() {
        // The same signature with s replaced by n - s and v flipped recovers the same key.
        let low = hex::decode(concat!(
            "456e9aea5e197a1f1af7a3e85a3212fa4049a3ba34c2289b4c860fc0b0c64ef3",
            "000000000000000000000000000000000000000000000000000000000000001c",
            "9242685bf161793cc25603c231bc2f568eb630ea16aa137d2664ac8038825608",
            "4f8ae3bd7535248d0bd448298cc2e2071e56992d0774dc340c368ae950852ada",
        ))
        .unwrap();
        let n = num_bigint::BigUint::parse_bytes(
            b"fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141",
            16,
        )
        .unwrap();
        let s = num_bigint::BigUint::from_bytes_be(&low[96..128]);
        let mut high = low.clone();
        high[63] = 27;
        let high_s = (n - s).to_bytes_be();
        high[96..128].fill(0);
        high[128 - high_s.len()..128].copy_from_slice(&high_s);
        assert_eq!(ecrecover(&high, 3_000), ecrecover(&low, 3_000));

        // r = 0 and s at the curve order are invalid.
        let mut zero_r = low.clone();
        zero_r[64..96].fill(0);
        assert_eq!(ecrecover(&zero_r, 3_000), (true, vec![], 3_000));
        let mut big_s = low;
        big_s[96..128].copy_from_slice(
            &hex::decode("fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141")
                .unwrap(),
        );
        assert_eq!(ecrecover(&big_s, 3_000), (true, vec![], 3_000));
    }
}
