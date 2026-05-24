use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};

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

    // v is the last byte of the 32-byte word at offset 32
    let v = buf[63];
    if v != 27 && v != 28 {
        return (true, vec![], GAS); // invalid v: success but empty output
    }
    let rid = match RecoveryId::try_from(v - 27) {
        Ok(r) => r,
        Err(_) => return (true, vec![], GAS),
    };

    let r: [u8; 32] = buf[64..96].try_into().unwrap();
    let s: [u8; 32] = buf[96..128].try_into().unwrap();
    let sig = match Signature::from_scalars(r, s) {
        Ok(s) => s,
        Err(_) => return (true, vec![], GAS),
    };

    // EVM ecrecover accepts any s in [1, n-1], but k256 recovery requires low-s.
    // Normalize s and flip recovery id if needed.
    let (sig, rid) = if let Some(normalized) = sig.normalize_s() {
        let new_rid = RecoveryId::new(!rid.is_y_odd(), rid.is_x_reduced());
        (normalized, new_rid)
    } else {
        (sig, rid)
    };

    let key = match VerifyingKey::recover_from_prehash(&buf[0..32], &sig, rid) {
        Ok(k) => k,
        Err(_) => return (true, vec![], GAS),
    };

    let point = key.to_encoded_point(false);
    let hash = yevm_misc::keccak256(&point.as_bytes()[1..]);
    let mut out = [0u8; 32];
    out[12..].copy_from_slice(&hash.as_ref()[12..]);
    (true, out.to_vec(), GAS)
}
