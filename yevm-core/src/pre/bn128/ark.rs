//! bn254 on arkworks, for the browser and ESP32 builds (and, in tests, as the reference for mcl).

use ark_bn254::{Bn254, Fq, Fq2, Fr, G1Affine, G1Projective, G2Affine};
use ark_ec_06::{AffineRepr, CurveGroup, pairing::Pairing};
use ark_ff_06::{BigInt, One, PrimeField, Zero};

pub(super) fn add(a: &[u8], b: &[u8]) -> Option<[u8; 64]> {
    let (a, b) = (read_g1(a)?, read_g1(b)?);
    Some(encode_g1((G1Projective::from(a) + b).into_affine()))
}

pub(super) fn mul(point: &[u8], scalar: &[u8]) -> Option<[u8; 64]> {
    let point = read_g1(point)?;
    Some(encode_g1(
        (point * Fr::from_be_bytes_mod_order(scalar)).into_affine(),
    ))
}

pub(super) fn pairing(input: &[u8]) -> Option<bool> {
    let pairs = input.as_chunks::<192>().0;
    let (mut g1s, mut g2s) = (
        Vec::with_capacity(pairs.len()),
        Vec::with_capacity(pairs.len()),
    );
    for pair in pairs {
        let (g1, g2) = (read_g1(&pair[..64])?, read_g2(&pair[64..])?);
        // A pair with the point at infinity contributes the identity.
        if !g1.is_zero() && !g2.is_zero() {
            g1s.push(g1);
            g2s.push(g2);
        }
    }
    // One Miller loop over all pairs and a single final exponentiation.
    Some(g1s.is_empty() || Bn254::multi_pairing(g1s, g2s).0.is_one())
}

/// A field element from 32 big-endian bytes, `None` unless it is below the modulus.
pub(super) fn read_fq(bytes: &[u8]) -> Option<Fq> {
    let mut limbs = [0u64; 4];
    for (i, limb) in limbs.iter_mut().enumerate() {
        let end = 32 - 8 * i;
        *limb = u64::from_be_bytes(bytes[end - 8..end].try_into().unwrap());
    }
    Fq::from_bigint(BigInt::new(limbs))
}

pub(super) fn write_fq(value: Fq, out: &mut [u8]) {
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
fn encode_g1(point: G1Affine) -> [u8; 64] {
    let mut out = [0u8; 64];
    if let Some((x, y)) = point.xy() {
        write_fq(x, &mut out[..32]);
        write_fq(y, &mut out[32..]);
    }
    out
}
