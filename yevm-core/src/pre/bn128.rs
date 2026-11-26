//! bn254 (alt_bn128) precompiles 0x06-0x08 (EIP-196, EIP-197). Native builds run them on
//! herumi's mcl, as evm2 does; the browser and ESP32 builds on arkworks.

#[cfg(any(test, target_arch = "wasm32", target_os = "espidf"))]
mod ark;
#[cfg(not(any(target_arch = "wasm32", target_os = "espidf")))]
mod mcl;

#[cfg(any(target_arch = "wasm32", target_os = "espidf"))]
use ark as backend;
#[cfg(not(any(target_arch = "wasm32", target_os = "espidf")))]
use mcl as backend;

/// ecAdd (precompile 0x06) - BN128 point addition
/// Gas: 150
pub fn ec_add(input: &[u8], gas_limit: i64) -> (bool, Vec<u8>, i64) {
    const GAS: i64 = 150;
    if GAS > gas_limit {
        return (false, vec![], gas_limit);
    }
    let mut buf = [0u8; 128];
    let n = input.len().min(128);
    buf[..n].copy_from_slice(&input[..n]);
    match backend::add(&buf[..64], &buf[64..]) {
        Some(sum) => (true, sum.to_vec(), GAS),
        None => (false, vec![], gas_limit),
    }
}

/// ecMul (precompile 0x07) - BN128 scalar multiplication
/// Gas: 6000
pub fn ec_mul(input: &[u8], gas_limit: i64) -> (bool, Vec<u8>, i64) {
    const GAS: i64 = 6_000;
    if GAS > gas_limit {
        return (false, vec![], gas_limit);
    }
    let mut buf = [0u8; 96];
    let n = input.len().min(96);
    buf[..n].copy_from_slice(&input[..n]);
    // The scalar is any 256-bit number: multiplying by it is multiplying by it modulo the
    // group order, as in geth.
    match backend::mul(&buf[..64], &buf[64..]) {
        Some(product) => (true, product.to_vec(), GAS),
        None => (false, vec![], gas_limit),
    }
}

/// ecPairing (precompile 0x08) - BN128 pairing check
/// Gas: 45000 + 34000 * num_pairs
pub fn ec_pairing(input: &[u8], gas_limit: i64) -> (bool, Vec<u8>, i64) {
    if !input.len().is_multiple_of(192) {
        return (false, vec![], gas_limit);
    }
    let gas = 45_000 + 34_000 * (input.len() / 192) as i64;
    if gas > gas_limit {
        return (false, vec![], gas_limit);
    }
    match backend::pairing(input) {
        Some(holds) => {
            let mut out = vec![0u8; 32];
            out[31] = u8::from(holds);
            (true, out, gas)
        }
        None => (false, vec![], gas_limit),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        ::hex::decode(s).unwrap()
    }

    // G1 generator (1, 2).
    const G1: &str = concat!(
        "0000000000000000000000000000000000000000000000000000000000000001",
        "0000000000000000000000000000000000000000000000000000000000000002",
    );
    // 2 * G1.
    const G1X2: &str = concat!(
        "030644e72e131a029b85045b68181585d97816a916871ca8d3c208c16d87cfd3",
        "15ed738c0e0a7c92e7845f96b2ae9c0a68a6a449e3538fc7ff3ebf7a5a18a2c4",
    );
    // -G1 = (1, p - 2).
    const NEG_G1: &str = concat!(
        "0000000000000000000000000000000000000000000000000000000000000001",
        "30644e72e131a029b85045b68181585d97816a916871ca8d3c208c16d87cfd45",
    );
    // G2 generator.
    const G2: &str = concat!(
        "198e9393920d483a7260bfb731fb5d25f1aa493335a9e71297e485b7aef312c2",
        "1800deef121f1e76426a00665e5c4479674322d4f75edadd46debd5cd992f6ed",
        "090689d0585ff075ec9e99ad690c3395bc4b313370b38ef355acdadcd122975b",
        "12c85ea5db8c6deb4aab71808dcb408fe3d1e7690c43d37b4ce6cc0166fa7daa",
    );
    // The group order r.
    const R: &str = "30644e72e131a029b85045b68181585d2833e84879b9709143e1f593f0000001";

    #[test]
    fn add_and_mul_match_known_points() {
        let (ok, out, gas) = ec_add(&hex(&format!("{G1}{G1}")), 1_000);
        assert_eq!((ok, gas), (true, 150));
        assert_eq!(out, hex(G1X2));

        let two = format!("{:064x}", 2);
        assert_eq!(ec_mul(&hex(&format!("{G1}{two}")), 10_000).1, hex(G1X2));
        // r + 2 multiplies like 2 (geth reduces the scalar modulo r).
        let mut r_plus_2 = hex(R);
        r_plus_2[31] += 2;
        assert_eq!(ec_mul(&[hex(G1), r_plus_2].concat(), 10_000).1, hex(G1X2));
        // r * G1 is the point at infinity.
        assert_eq!(ec_mul(&[hex(G1), hex(R)].concat(), 10_000).1, vec![0; 64]);
    }

    #[test]
    fn invalid_points_fail() {
        // (1, 3) is not on the curve; a coordinate equal to the modulus is not a field element.
        let off_curve = format!("{:064x}{:064x}", 1, 3);
        let input = hex(&format!("{off_curve}{G1}"));
        assert_eq!(ec_add(&input, 1_000), (false, vec![], 1_000));
        let p = "30644e72e131a029b85045b68181585d97816a916871ca8d3c208c16d87cfd47";
        let input = hex(&format!("{p}{:064x}{G1}", 2));
        assert_eq!(ec_add(&input, 1_000), (false, vec![], 1_000));
    }

    #[test]
    fn pairing_checks() {
        // e(G1, G2) * e(-G1, G2) = 1.
        let (ok, out, gas) = ec_pairing(&hex(&format!("{G1}{G2}{NEG_G1}{G2}")), 1_000_000);
        assert!(ok);
        assert_eq!((out[31], gas), (1, 45_000 + 2 * 34_000));
        assert_eq!(
            ec_pairing(&hex(&format!("{G1}{G2}{G1}{G2}")), 1_000_000).1[31],
            0
        );
        // No pairs, and pairs with the point at infinity, hold.
        assert_eq!(ec_pairing(&[], 1_000_000).1[31], 1);
        let infinity = hex(&format!("{:0128}{G2}", 0));
        assert_eq!(ec_pairing(&infinity, 1_000_000).1[31], 1);
    }

    /// mcl against arkworks: sums, products (scalars at and above r too) and pairings, including
    /// G2 points on the curve but outside the subgroup, which both must reject.
    #[cfg(not(any(target_arch = "wasm32", target_os = "espidf")))]
    #[test]
    fn mcl_matches_arkworks() {
        use ark_bn254::{Fq, Fq2, Fr, G1Projective, G2Affine, G2Projective};
        use ark_ec_06::{AffineRepr, CurveGroup, PrimeGroup};
        use ark_ff_06::PrimeField;

        // xorshift64: deterministic inputs.
        fn word(seed: &mut u64) -> u64 {
            *seed ^= *seed << 13;
            *seed ^= *seed >> 7;
            *seed ^= *seed << 17;
            *seed
        }
        fn bytes32(seed: &mut u64) -> [u8; 32] {
            let mut out = [0u8; 32];
            for chunk in out.chunks_mut(8) {
                chunk.copy_from_slice(&word(seed).to_be_bytes());
            }
            out
        }
        let seed = &mut 0x9e37_79b9_7f4a_7c15u64;
        let g1 = |k: &[u8; 32]| {
            let point = (G1Projective::generator() * Fr::from_be_bytes_mod_order(k)).into_affine();
            let mut out = [0u8; 64];
            if let Some((x, y)) = point.xy() {
                ark::write_fq(x, &mut out[..32]);
                ark::write_fq(y, &mut out[32..]);
            }
            out
        };
        let encode_g2 = |point: G2Affine| {
            let mut out = [0u8; 128];
            if let Some((x, y)) = point.xy() {
                ark::write_fq(x.c1, &mut out[0..32]);
                ark::write_fq(x.c0, &mut out[32..64]);
                ark::write_fq(y.c1, &mut out[64..96]);
                ark::write_fq(y.c0, &mut out[96..128]);
            }
            out
        };
        let fr_bytes = |v: Fr| {
            let mut out = [0u8; 32];
            for (j, limb) in v.into_bigint().0.iter().enumerate() {
                out[24 - 8 * j..32 - 8 * j].copy_from_slice(&limb.to_be_bytes());
            }
            out
        };

        let r = hex(R);
        for i in 0..64 {
            let (a, b) = (g1(&bytes32(seed)), g1(&bytes32(seed)));
            assert_eq!(mcl::add(&a, &b), ark::add(&a, &b), "add #{i}");
            assert_eq!(mcl::add(&a, &a), ark::add(&a, &a), "double #{i}");
            let mut scalar = bytes32(seed);
            if i % 4 == 0 {
                scalar[..16].copy_from_slice(&r[..16]); // around r
            }
            assert_eq!(mcl::mul(&a, &scalar), ark::mul(&a, &scalar), "mul #{i}");
        }
        assert_eq!(mcl::mul(&g1(&[1; 32]), &r), Some([0; 64]));

        let (mut outside, mut inside) = (0, 0);
        for i in 0..64u64 {
            // On the curve, almost surely outside the subgroup (G2's cofactor is about 2^254).
            let x = Fq2::new(Fq::from(i + 1), Fq::from(word(seed)));
            if let Some(point) = G2Affine::get_point_from_x_unchecked(x, i % 2 == 0) {
                let pair = [g1(&bytes32(seed)).as_slice(), &encode_g2(point)].concat();
                let expected = ark::pairing(&pair);
                assert_eq!(
                    mcl::pairing(&pair),
                    expected,
                    "pairing with a curve point #{i}"
                );
                if expected.is_none() {
                    outside += 1;
                }
            }
            // In the subgroup: e(aG1, bG2) · e(-abG1, G2) = 1, and a random pair rarely holds.
            let (a, b) = (Fr::from(word(seed)), Fr::from(word(seed)));
            let q = (G2Projective::generator() * b).into_affine();
            let generator = encode_g2(G2Projective::generator().into_affine());
            let holds = [
                &g1(&fr_bytes(a))[..],
                &encode_g2(q),
                &g1(&fr_bytes(-(a * b))),
                &generator,
            ]
            .concat();
            assert_eq!(mcl::pairing(&holds), Some(true), "pairing that holds #{i}");
            assert_eq!(ark::pairing(&holds), Some(true));
            let random = [&g1(&bytes32(seed))[..], &encode_g2(q)].concat();
            assert_eq!(
                mcl::pairing(&random),
                ark::pairing(&random),
                "random pairing #{i}"
            );
            inside += 1;
        }
        assert!(
            outside > 16 && inside == 64,
            "{outside} outside, {inside} inside"
        );
    }
}
