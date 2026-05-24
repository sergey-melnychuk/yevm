mod blake2f;
mod bn128;
mod ecrecover;
mod identity;
mod kzg;
mod modexp;
mod p256verify;
mod ripemd160;
mod sha256;

/// Run a precompile. Returns `(success, output, gas_used)`.
/// `success=false` means out-of-gas or invalid input; output is empty and gas_used equals gas_limit.
pub fn run(id: u64, input: &[u8], gas_limit: i64) -> (bool, Vec<u8>, i64) {
    match id {
        1 => ecrecover::ecrecover(input, gas_limit),
        2 => sha256::sha256(input, gas_limit),
        3 => ripemd160::ripemd160(input, gas_limit),
        4 => identity::identity(input, gas_limit),
        5 => modexp::modexp(input, gas_limit),
        6 => bn128::ec_add(input, gas_limit),
        7 => bn128::ec_mul(input, gas_limit),
        8 => bn128::ec_pairing(input, gas_limit),
        9 => blake2f::blake2f(input, gas_limit),
        0xa => kzg::point_evaluation(input, gas_limit),
        0x100 => p256verify::p256verify(input, gas_limit),
        _ => (true, vec![], 0),
    }
}
