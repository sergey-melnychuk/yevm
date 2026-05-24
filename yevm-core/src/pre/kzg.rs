use std::sync::LazyLock;

use kzg_rs::{Bytes32, Bytes48, KzgProof, KzgSettings};

static KZG_SETTINGS: LazyLock<KzgSettings> = LazyLock::new(|| {
    KzgSettings::load_trusted_setup_file().expect("failed to load KZG trusted setup")
});

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

    let Ok(z_bytes) = Bytes32::from_slice(z) else {
        return (false, vec![], gas_limit);
    };
    let Ok(y_bytes) = Bytes32::from_slice(y) else {
        return (false, vec![], gas_limit);
    };
    let Ok(commitment_bytes) = Bytes48::from_slice(commitment) else {
        return (false, vec![], gas_limit);
    };
    let Ok(proof_bytes) = Bytes48::from_slice(proof) else {
        return (false, vec![], gas_limit);
    };

    let result = KzgProof::verify_kzg_proof(
        &commitment_bytes,
        &z_bytes,
        &y_bytes,
        &proof_bytes,
        &KZG_SETTINGS,
    );

    match result {
        Ok(true) => {
            // Return FIELD_ELEMENTS_PER_BLOB and BLS_MODULUS
            let mut out = vec![0u8; 64];
            // FIELD_ELEMENTS_PER_BLOB = 4096 = 0x1000
            out[30] = 0x10;
            // BLS_MODULUS
            let bls_modulus =
                hex::decode("73eda753299d7d483339d80809a1d80553bda402fffe5bfeffffffff00000001")
                    .unwrap();
            out[32..64].copy_from_slice(&bls_modulus);
            (true, out, GAS)
        }
        _ => (false, vec![], gas_limit),
    }
}

fn sha2_hash(data: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(data).to_vec()
}
