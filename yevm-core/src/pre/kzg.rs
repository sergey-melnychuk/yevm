//! KZG point evaluation (EIP-4844). Native builds verify with c-kzg (blst), as evm2 does; the
//! browser and ESP32 builds with kzg-rs.

/// KZG point evaluation (precompile 0x0A) - EIP-4844
/// Input: versioned_hash (32) || z (32) || y (32) || commitment (48) || proof (48)
/// = 192 bytes total
/// Gas: 50000
pub fn point_evaluation(input: &[u8], gas_limit: i64) -> (bool, Vec<u8>, i64) {
    const GAS: i64 = 50_000;
    if GAS > gas_limit {
        return (false, vec![], gas_limit);
    }
    if input.len() != 192 {
        return (false, vec![], gas_limit);
    }

    let versioned_hash = &input[0..32];
    let z = &input[32..64];
    let y = &input[64..96];
    let commitment = &input[96..144];
    let proof = &input[144..192];

    // Verify that versioned_hash == SHA256(commitment) with first byte replaced by 0x01
    let commitment_hash = sha2_hash(commitment);
    if versioned_hash[0] != 0x01 || versioned_hash[1..] != commitment_hash[1..] {
        return (false, vec![], gas_limit);
    }

    let proved = verify(
        commitment.try_into().unwrap(),
        z.try_into().unwrap(),
        y.try_into().unwrap(),
        proof.try_into().unwrap(),
    );
    if !proved {
        return (false, vec![], gas_limit);
    }
    // FIELD_ELEMENTS_PER_BLOB (4096) ++ BLS_MODULUS.
    let mut out = vec![0u8; 64];
    out[30] = 0x10;
    out[32..64].copy_from_slice(&BLS_MODULUS);
    (true, out, GAS)
}

fn sha2_hash(data: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(data).to_vec()
}

/// The BLS12-381 scalar field modulus, big-endian.
const BLS_MODULUS: [u8; 32] = [
    0x73, 0xed, 0xa7, 0x53, 0x29, 0x9d, 0x7d, 0x48, 0x33, 0x39, 0xd8, 0x08, 0x09, 0xa1, 0xd8, 0x05,
    0x53, 0xbd, 0xa4, 0x02, 0xff, 0xfe, 0x5b, 0xfe, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x01,
];

/// Loads the trusted setup unless it is loaded.
#[cfg(not(any(target_arch = "wasm32", target_os = "espidf")))]
pub(super) fn load_settings() {
    c_kzg::ethereum_kzg_settings(0);
}

/// Whether `proof` shows that the polynomial committed to by `commitment` is `y` at `z`.
#[cfg(not(any(target_arch = "wasm32", target_os = "espidf")))]
fn verify(commitment: &[u8; 48], z: &[u8; 32], y: &[u8; 32], proof: &[u8; 48]) -> bool {
    use c_kzg::{Bytes32, Bytes48};
    c_kzg::ethereum_kzg_settings(0)
        .verify_kzg_proof(
            &Bytes48::new(*commitment),
            &Bytes32::new(*z),
            &Bytes32::new(*y),
            &Bytes48::new(*proof),
        )
        .unwrap_or(false)
}

#[cfg(any(target_arch = "wasm32", target_os = "espidf"))]
static SETTINGS: std::sync::LazyLock<kzg_rs::KzgSettings> = std::sync::LazyLock::new(|| {
    kzg_rs::KzgSettings::load_trusted_setup_file().expect("failed to load KZG trusted setup")
});

/// Loads the trusted setup unless it is loaded.
#[cfg(any(target_arch = "wasm32", target_os = "espidf"))]
pub(super) fn load_settings() {
    std::sync::LazyLock::force(&SETTINGS);
}

#[cfg(any(target_arch = "wasm32", target_os = "espidf"))]
fn verify(commitment: &[u8; 48], z: &[u8; 32], y: &[u8; 32], proof: &[u8; 48]) -> bool {
    use kzg_rs::{Bytes32, Bytes48, KzgProof};
    let (Ok(commitment), Ok(z), Ok(y), Ok(proof)) = (
        Bytes48::from_slice(commitment),
        Bytes32::from_slice(z),
        Bytes32::from_slice(y),
        Bytes48::from_slice(proof),
    ) else {
        return false;
    };
    KzgProof::verify_kzg_proof(&commitment, &z, &y, &proof, &SETTINGS).unwrap_or(false)
}
