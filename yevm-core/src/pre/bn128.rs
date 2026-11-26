//! bn254 (alt_bn128) precompiles 0x06-0x08 (EIP-196, EIP-197) on arkworks, the library evm2 uses.

use ark_bn254::{Bn254, Fq, Fq2, Fr, G1Affine, G1Projective, G2Affine};
use ark_ec_06::{AffineRepr, CurveGroup, pairing::Pairing};
use ark_ff_06::{BigInt, One, PrimeField, Zero};

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

    let (Some(p1), Some(p2)) = (read_g1(&buf[0..64]), read_g1(&buf[64..128])) else {
        return (false, vec![], gas_limit);
    };
    let sum = (G1Projective::from(p1) + p2).into_affine();
    (true, encode_g1(sum), GAS)
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

    let Some(p) = read_g1(&buf[0..64]) else {
        return (false, vec![], gas_limit);
    };
    // The scalar is any 256-bit number: multiplying by it is multiplying by it modulo the
    // group order, as in geth.
    let scalar = Fr::from_be_bytes_mod_order(&buf[64..96]);
    (true, encode_g1((p * scalar).into_affine()), GAS)
}

/// ecPairing (precompile 0x08) - BN128 pairing check
/// Gas: 45000 + 34000 * num_pairs
pub fn ec_pairing(input: &[u8], gas_limit: i64) -> (bool, Vec<u8>, i64) {
    if !input.len().is_multiple_of(192) {
        return (false, vec![], gas_limit);
    }
    let num_pairs = input.len() / 192;
    let gas = 45_000 + 34_000 * num_pairs as i64;
    if gas > gas_limit {
        return (false, vec![], gas_limit);
    }

    let (mut g1s, mut g2s) = (Vec::with_capacity(num_pairs), Vec::with_capacity(num_pairs));
    for pair in input.as_chunks::<192>().0 {
        let (Some(g1), Some(g2)) = (read_g1(&pair[..64]), read_g2(&pair[64..])) else {
            return (false, vec![], gas_limit);
        };
        // A pair with the point at infinity contributes the identity.
        if !g1.is_zero() && !g2.is_zero() {
            g1s.push(g1);
            g2s.push(g2);
        }
    }
    // One Miller loop over all pairs and a single final exponentiation.
    let holds = g1s.is_empty() || Bn254::multi_pairing(g1s, g2s).0.is_one();

    let mut out = vec![0u8; 32];
    out[31] = u8::from(holds);
    (true, out, gas)
}

/// A field element from 32 big-endian bytes, `None` unless it is below the modulus.
fn read_fq(bytes: &[u8]) -> Option<Fq> {
    let mut limbs = [0u64; 4];
    for (i, limb) in limbs.iter_mut().enumerate() {
        let end = 32 - 8 * i;
        *limb = u64::from_be_bytes(bytes[end - 8..end].try_into().unwrap());
    }
    Fq::from_bigint(BigInt::new(limbs))
}

fn write_fq(value: Fq, out: &mut [u8]) {
    for (i, limb) in value.into_bigint().0.iter().enumerate() {
        let end = 32 - 8 * i;
        out[end - 8..end].copy_from_slice(&limb.to_be_bytes());
    }
}

/// A G1 point from x ++ y (64 bytes), (0, 0) being the point at infinity. G1's cofactor is 1,
/// so every point on the curve is in the group.
fn read_g1(bytes: &[u8]) -> Option<G1Affine> {
    let (x, y) = (read_fq(&bytes[..32])?, read_fq(&bytes[32..64])?);
    if x.is_zero() && y.is_zero() {
        return Some(G1Affine::zero());
    }
    let point = G1Affine::new_unchecked(x, y);
    point.is_on_curve().then_some(point)
}

/// A G2 point from x_im ++ x_re ++ y_im ++ y_re (128 bytes), (0, 0) being the point at infinity;
/// it must be on the curve and in the order-r subgroup.
fn read_g2(bytes: &[u8]) -> Option<G2Affine> {
    let x = Fq2::new(read_fq(&bytes[32..64])?, read_fq(&bytes[0..32])?);
    let y = Fq2::new(read_fq(&bytes[96..128])?, read_fq(&bytes[64..96])?);
    if x.is_zero() && y.is_zero() {
        return Some(G2Affine::zero());
    }
    let point = G2Affine::new_unchecked(x, y);
    (point.is_on_curve() && point.is_in_correct_subgroup_assuming_on_curve()).then_some(point)
}

/// x ++ y, all zero for the point at infinity.
fn encode_g1(point: G1Affine) -> Vec<u8> {
    let mut out = vec![0u8; 64];
    if let Some((x, y)) = point.xy() {
        write_fq(x, &mut out[..32]);
        write_fq(y, &mut out[32..]);
    }
    out
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
}
