/// Identity (precompile 0x04)
/// Returns the input data unchanged.
/// Gas: 15 + 3 * ceil(len / 32)
pub fn identity(input: &[u8], gas_limit: i64) -> (bool, Vec<u8>, i64) {
    let gas = 15 + 3 * ((input.len() as i64 + 31) / 32);
    if gas > gas_limit {
        return (false, vec![], gas_limit);
    }
    (true, input.to_vec(), gas)
}
