use ripemd::{Digest, Ripemd160};

/// RIPEMD-160 (precompile 0x03)
/// Output: zero-padded to 32 bytes (20-byte hash right-aligned)
/// Gas: 600 + 120 * ceil(len / 32)
pub fn ripemd160(input: &[u8], gas_limit: i64) -> (bool, Vec<u8>, i64) {
    let gas = 600 + 120 * ((input.len() as i64 + 31) / 32);
    if gas > gas_limit {
        return (false, vec![], gas_limit);
    }
    let hash = Ripemd160::digest(input);
    let mut out = vec![0u8; 32];
    out[12..].copy_from_slice(&hash);
    (true, out, gas)
}
