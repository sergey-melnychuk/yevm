use sha2::{Digest, Sha256};

/// SHA-256 (precompile 0x02)
/// Gas: 60 + 12 * ceil(len / 32)
pub fn sha256(input: &[u8], gas_limit: i64) -> (bool, Vec<u8>, i64) {
    let gas = 60 + 12 * ((input.len() as i64 + 31) / 32);
    if gas > gas_limit {
        return (false, vec![], gas_limit);
    }
    let hash = Sha256::digest(input);
    (true, hash.to_vec(), gas)
}
