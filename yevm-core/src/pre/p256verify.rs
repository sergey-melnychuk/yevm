use p256::ecdsa::{Signature, VerifyingKey, signature::hazmat::PrehashVerifier};

/// P256VERIFY (precompile 0x100) - EIP-7212 / Osaka gas increase
/// Input: hash (32) || r (32) || s (32) || x (32) || y (32) = 160 bytes
/// Output: 1 (32 bytes, big-endian) on valid signature, empty on invalid
/// Gas: 6900 (post-Osaka, was 3450)
pub fn p256verify(input: &[u8], gas_limit: i64) -> (bool, Vec<u8>, i64) {
    const GAS: i64 = 6_900;
    if GAS > gas_limit {
        return (false, vec![], gas_limit);
    }
    if input.len() != 160 {
        return (true, vec![], GAS);
    }

    let hash = &input[0..32];
    let r: &[u8; 32] = input[32..64].try_into().unwrap();
    let s: &[u8; 32] = input[64..96].try_into().unwrap();
    let x = &input[96..128];
    let y = &input[128..160];

    // Build the uncompressed public key: 0x04 || x || y
    let mut pubkey_bytes = [0u8; 65];
    pubkey_bytes[0] = 0x04;
    pubkey_bytes[1..33].copy_from_slice(x);
    pubkey_bytes[33..65].copy_from_slice(y);

    let key = match VerifyingKey::from_sec1_bytes(&pubkey_bytes) {
        Ok(k) => k,
        Err(_) => return (true, vec![], GAS),
    };

    // Build the signature from r and s
    let sig = match Signature::from_scalars(*r, *s) {
        Ok(s) => s,
        Err(_) => return (true, vec![], GAS),
    };

    match key.verify_prehash(hash, &sig) {
        Ok(_) => {
            let mut out = vec![0u8; 32];
            out[31] = 1;
            (true, out, GAS)
        }
        Err(_) => (true, vec![], GAS),
    }
}
