use ark_bls12_381::{Bls12_381, Fq, Fq2, Fr, G1Affine, G1Projective, G2Affine, G2Projective};
use ark_ec::{
    AffineRepr, CurveGroup, VariableBaseMSM,
    hashing::{
        curve_maps::wb::{WBConfig, WBMap},
        map_to_curve_hasher::MapToCurve,
    },
    pairing::Pairing,
    short_weierstrass::{Affine, SWCurveConfig},
};
use ark_ff::{BigInt, BigInteger, One, PrimeField, Zero};

/// BLS12-381 precompiles (EIP-2537, Prague): 0x0b..=0x11.
///
/// Built on the pure-Rust arkworks curve arithmetic (WASM-friendly).
/// Encoding: Fp is 64 bytes big-endian (top 16 bytes must be zero, value < p);
/// Fp2 is c0 || c1; a point is x || y; all-zero encodes the point at infinity.
/// Any failure (bad length, non-canonical field element, point not on curve,
/// point not in subgroup where required, out of gas) consumes all gas.
pub fn run(id: u64, input: &[u8], gas_limit: i64) -> (bool, Vec<u8>, i64) {
    let Some(gas) = gas_cost(id, input.len()) else {
        return (false, vec![], gas_limit);
    };
    if gas > gas_limit {
        return (false, vec![], gas_limit);
    }
    let out = match id {
        0x0b => g1_add(input),
        0x0c => g1_msm(input),
        0x0d => g2_add(input),
        0x0e => g2_msm(input),
        0x0f => pairing(input),
        0x10 => map_fp_to_g1(input),
        0x11 => map_fp2_to_g2(input),
        _ => unreachable!("not a BLS12-381 precompile: {id:#x}"),
    };
    match out {
        Some(out) => (true, out, gas),
        None => (false, vec![], gas_limit),
    }
}

const FP_LEN: usize = 64;
const FP2_LEN: usize = 2 * FP_LEN;
const G1_LEN: usize = 2 * FP_LEN;
const G2_LEN: usize = 2 * FP2_LEN;
const SCALAR_LEN: usize = 32;
const G1_MSM_PAIR_LEN: usize = G1_LEN + SCALAR_LEN;
const G2_MSM_PAIR_LEN: usize = G2_LEN + SCALAR_LEN;
const PAIRING_PAIR_LEN: usize = G1_LEN + G2_LEN;

const G1_ADD_GAS: i64 = 375;
const G2_ADD_GAS: i64 = 600;
const G1_MUL_GAS: i64 = 12_000;
const G2_MUL_GAS: i64 = 22_500;
const PAIRING_BASE_GAS: i64 = 37_700;
const PAIRING_PER_PAIR_GAS: i64 = 32_600;
const MAP_FP_TO_G1_GAS: i64 = 5_500;
const MAP_FP2_TO_G2_GAS: i64 = 23_800;
const MSM_MULTIPLIER: i64 = 1_000;

/// EIP-2537 MSM discount for k = 1..=128 pairs; k > 128 uses the last entry.
#[rustfmt::skip]
const G1_MSM_DISCOUNT: [u16; 128] = [
    1000, 949, 848, 797, 764, 750, 738, 728, 719, 712, 705, 698, 692, 687, 682, 677,
    673, 669, 665, 661, 658, 654, 651, 648, 645, 642, 640, 637, 635, 632, 630, 627,
    625, 623, 621, 619, 617, 615, 613, 611, 609, 608, 606, 604, 603, 601, 599, 598,
    596, 595, 593, 592, 591, 589, 588, 586, 585, 584, 582, 581, 580, 579, 577, 576,
    575, 574, 573, 572, 570, 569, 568, 567, 566, 565, 564, 563, 562, 561, 560, 559,
    558, 557, 556, 555, 554, 553, 552, 551, 550, 549, 548, 547, 547, 546, 545, 544,
    543, 542, 541, 540, 540, 539, 538, 537, 536, 536, 535, 534, 533, 532, 532, 531,
    530, 529, 528, 528, 527, 526, 525, 525, 524, 523, 522, 522, 521, 520, 520, 519,
];

#[rustfmt::skip]
const G2_MSM_DISCOUNT: [u16; 128] = [
    1000, 1000, 923, 884, 855, 832, 812, 796, 782, 770, 759, 749, 740, 732, 724, 717,
    711, 704, 699, 693, 688, 683, 679, 674, 670, 666, 663, 659, 655, 652, 649, 646,
    643, 640, 637, 634, 632, 629, 627, 624, 622, 620, 618, 615, 613, 611, 609, 607,
    606, 604, 602, 600, 598, 597, 595, 593, 592, 590, 589, 587, 586, 584, 583, 582,
    580, 579, 578, 576, 575, 574, 573, 571, 570, 569, 568, 567, 566, 565, 563, 562,
    561, 560, 559, 558, 557, 556, 555, 554, 553, 552, 552, 551, 550, 549, 548, 547,
    546, 545, 545, 544, 543, 542, 541, 541, 540, 539, 538, 537, 537, 536, 535, 535,
    534, 533, 532, 532, 531, 530, 530, 529, 528, 528, 527, 526, 526, 525, 524, 524,
];

/// Gas cost for a well-formed input length, `None` if the length is invalid.
fn gas_cost(id: u64, len: usize) -> Option<i64> {
    let msm = |pair_len: usize, mul_gas: i64, table: &[u16; 128]| {
        if len == 0 || !len.is_multiple_of(pair_len) {
            return None;
        }
        let k = len / pair_len;
        let discount = table[k.min(table.len()) - 1] as i64;
        Some(k as i64 * mul_gas * discount / MSM_MULTIPLIER)
    };
    match id {
        0x0b => (len == 2 * G1_LEN).then_some(G1_ADD_GAS),
        0x0c => msm(G1_MSM_PAIR_LEN, G1_MUL_GAS, &G1_MSM_DISCOUNT),
        0x0d => (len == 2 * G2_LEN).then_some(G2_ADD_GAS),
        0x0e => msm(G2_MSM_PAIR_LEN, G2_MUL_GAS, &G2_MSM_DISCOUNT),
        0x0f => (len != 0 && len.is_multiple_of(PAIRING_PAIR_LEN))
            .then(|| PAIRING_BASE_GAS + PAIRING_PER_PAIR_GAS * (len / PAIRING_PAIR_LEN) as i64),
        0x10 => (len == FP_LEN).then_some(MAP_FP_TO_G1_GAS),
        0x11 => (len == FP2_LEN).then_some(MAP_FP2_TO_G2_GAS),
        _ => None,
    }
}

/// 0x0b: G1 point addition (no subgroup check).
fn g1_add(input: &[u8]) -> Option<Vec<u8>> {
    let a = read_g1(&input[..G1_LEN])?;
    let b = read_g1(&input[G1_LEN..])?;
    Some(write_g1(&(a + b).into_affine()))
}

/// 0x0d: G2 point addition (no subgroup check).
fn g2_add(input: &[u8]) -> Option<Vec<u8>> {
    let a = read_g2(&input[..G2_LEN])?;
    let b = read_g2(&input[G2_LEN..])?;
    Some(write_g2(&(a + b).into_affine()))
}

/// 0x0c: G1 multi-scalar multiplication (points must be in the subgroup).
fn g1_msm(input: &[u8]) -> Option<Vec<u8>> {
    let (points, scalars) = read_msm(input, G1_LEN, read_g1_subgroup)?;
    Some(write_g1(
        &G1Projective::msm_unchecked(&points, &scalars).into_affine(),
    ))
}

/// 0x0e: G2 multi-scalar multiplication (points must be in the subgroup).
fn g2_msm(input: &[u8]) -> Option<Vec<u8>> {
    let (points, scalars) = read_msm(input, G2_LEN, read_g2_subgroup)?;
    Some(write_g2(
        &G2Projective::msm_unchecked(&points, &scalars).into_affine(),
    ))
}

/// 0x0f: pairing check, returns 1 iff the product of pairings is one.
fn pairing(input: &[u8]) -> Option<Vec<u8>> {
    let mut g1 = Vec::with_capacity(input.len() / PAIRING_PAIR_LEN);
    let mut g2 = Vec::with_capacity(input.len() / PAIRING_PAIR_LEN);
    for chunk in input.as_chunks::<PAIRING_PAIR_LEN>().0 {
        g1.push(read_g1_subgroup(&chunk[..G1_LEN])?);
        g2.push(read_g2_subgroup(&chunk[G1_LEN..])?);
    }
    let mut out = vec![0u8; 32];
    if Bls12_381::multi_pairing(g1, g2).0.is_one() {
        out[31] = 1;
    }
    Some(out)
}

/// 0x10: map an Fp element to G1 (SSWU + isogeny, then cofactor clearing).
fn map_fp_to_g1(input: &[u8]) -> Option<Vec<u8>> {
    let u = read_fp(input)?;
    Some(write_g1(&map_to_curve(u)?))
}

/// 0x11: map an Fp2 element to G2 (SSWU + isogeny, then cofactor clearing).
fn map_fp2_to_g2(input: &[u8]) -> Option<Vec<u8>> {
    let u = read_fp2(input)?;
    Some(write_g2(&map_to_curve(u)?))
}

fn map_to_curve<P: WBConfig>(u: P::BaseField) -> Option<Affine<P>> {
    let p = WBMap::<P>::map_to_curve(u).ok()?;
    Some(p.clear_cofactor())
}

fn read_msm<A: AffineRepr>(
    input: &[u8],
    point_len: usize,
    read_point: fn(&[u8]) -> Option<A>,
) -> Option<(Vec<A>, Vec<Fr>)> {
    let pair_len = point_len + SCALAR_LEN;
    let mut points = Vec::with_capacity(input.len() / pair_len);
    let mut scalars = Vec::with_capacity(input.len() / pair_len);
    for chunk in input.chunks_exact(pair_len) {
        points.push(read_point(&chunk[..point_len])?);
        // Scalars may exceed the group order; reducing is sound since points
        // are verified to be in the prime-order subgroup.
        scalars.push(Fr::from_be_bytes_mod_order(&chunk[point_len..]));
    }
    Some((points, scalars))
}

/// Parse a 64-byte Fp element: 16 zero bytes of padding, then 48 bytes < p.
fn read_fp(bytes: &[u8]) -> Option<Fq> {
    let (pad, be) = bytes.split_at(FP_LEN - 48);
    if pad.iter().any(|&b| b != 0) {
        return None;
    }
    let mut limbs = [0u64; 6];
    for (i, chunk) in be.rchunks_exact(8).enumerate() {
        limbs[i] = u64::from_be_bytes(chunk.try_into().unwrap());
    }
    Fq::from_bigint(BigInt(limbs))
}

fn read_fp2(bytes: &[u8]) -> Option<Fq2> {
    Some(Fq2::new(
        read_fp(&bytes[..FP_LEN])?,
        read_fp(&bytes[FP_LEN..])?,
    ))
}

fn read_g1(bytes: &[u8]) -> Option<G1Affine> {
    let x = read_fp(&bytes[..FP_LEN])?;
    let y = read_fp(&bytes[FP_LEN..])?;
    on_curve(x, y)
}

fn read_g2(bytes: &[u8]) -> Option<G2Affine> {
    let x = read_fp2(&bytes[..FP2_LEN])?;
    let y = read_fp2(&bytes[FP2_LEN..])?;
    on_curve(x, y)
}

fn read_g1_subgroup(bytes: &[u8]) -> Option<G1Affine> {
    read_g1(bytes).filter(|p| p.is_in_correct_subgroup_assuming_on_curve())
}

fn read_g2_subgroup(bytes: &[u8]) -> Option<G2Affine> {
    read_g2(bytes).filter(|p| p.is_in_correct_subgroup_assuming_on_curve())
}

/// (0, 0) encodes infinity; any other point must satisfy the curve equation.
fn on_curve<P: SWCurveConfig>(x: P::BaseField, y: P::BaseField) -> Option<Affine<P>> {
    if x.is_zero() && y.is_zero() {
        return Some(Affine::identity());
    }
    let p = Affine::new_unchecked(x, y);
    p.is_on_curve().then_some(p)
}

fn write_fp(out: &mut Vec<u8>, f: &Fq) {
    out.extend_from_slice(&[0u8; FP_LEN - 48]);
    out.extend_from_slice(&f.into_bigint().to_bytes_be());
}

fn write_g1(p: &G1Affine) -> Vec<u8> {
    let mut out = Vec::with_capacity(G1_LEN);
    match p.xy() {
        Some((x, y)) => {
            write_fp(&mut out, &x);
            write_fp(&mut out, &y);
        }
        None => out.resize(G1_LEN, 0),
    }
    out
}

fn write_g2(p: &G2Affine) -> Vec<u8> {
    let mut out = Vec::with_capacity(G2_LEN);
    match p.xy() {
        Some((x, y)) => {
            for f in [x.c0, x.c1, y.c0, y.c1] {
                write_fp(&mut out, &f);
            }
        }
        None => out.resize(G2_LEN, 0),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g1() -> G1Affine {
        G1Affine::generator()
    }

    fn g2() -> G2Affine {
        G2Affine::generator()
    }

    fn scalar(n: u64) -> Vec<u8> {
        let mut s = vec![0u8; SCALAR_LEN];
        s[24..].copy_from_slice(&n.to_be_bytes());
        s
    }

    #[test]
    fn add_matches_msm() {
        let input = [write_g1(&g1()), write_g1(&g1())].concat();
        let (ok, sum, gas) = run(0x0b, &input, 1_000);
        assert!(ok);
        assert_eq!(gas, G1_ADD_GAS);
        let (ok, dbl, gas) = run(0x0c, &[write_g1(&g1()), scalar(2)].concat(), 100_000);
        assert!(ok);
        assert_eq!(gas, G1_MUL_GAS);
        assert_eq!(sum, dbl);

        let input = [write_g2(&g2()), write_g2(&g2())].concat();
        let (ok, sum, _) = run(0x0d, &input, 1_000);
        assert!(ok);
        let (ok, dbl, _) = run(0x0e, &[write_g2(&g2()), scalar(2)].concat(), 100_000);
        assert!(ok);
        assert_eq!(sum, dbl);
    }

    #[test]
    fn msm_scalar_above_order_and_gas() {
        // G * (q + 1) == G
        let mut s = Fr::MODULUS;
        s.add_with_carry(&BigInt::from(1u64));
        let input = [write_g1(&g1()), s.to_bytes_be()].concat();
        let (ok, out, _) = run(0x0c, &input, 100_000);
        assert!(ok);
        assert_eq!(out, write_g1(&g1()));

        let pair = [write_g1(&g1()), scalar(1)].concat();
        let input = pair.repeat(2);
        assert_eq!(run(0x0c, &input, 100_000).2, 2 * 12_000 * 949 / 1000);
        let input = pair.repeat(200);
        assert_eq!(run(0x0c, &input, 10_000_000).2, 200 * 12_000 * 519 / 1000);
    }

    #[test]
    fn pairing_identity() {
        let neg = -g1();
        let input = [
            write_g1(&g1()),
            write_g2(&g2()),
            write_g1(&neg),
            write_g2(&g2()),
        ]
        .concat();
        let (ok, out, gas) = run(0x0f, &input, 1_000_000);
        assert!(ok);
        assert_eq!(gas, PAIRING_BASE_GAS + 2 * PAIRING_PER_PAIR_GAS);
        assert_eq!(out[31], 1);

        let input = [write_g1(&g1()), write_g2(&g2())].concat();
        let (ok, out, _) = run(0x0f, &input, 1_000_000);
        assert!(ok);
        assert_eq!(out, vec![0u8; 32]);

        // Infinity pairs are fine and contribute one.
        let (ok, out, _) = run(0x0f, &vec![0u8; PAIRING_PAIR_LEN], 1_000_000);
        assert!(ok);
        assert_eq!(out[31], 1);
    }

    #[test]
    fn map_outputs_in_subgroup() {
        for u in [0u64, 1, 42] {
            let mut fp = vec![0u8; FP_LEN];
            fp[56..].copy_from_slice(&u.to_be_bytes());
            let (ok, out, gas) = run(0x10, &fp, 10_000);
            assert!(ok);
            assert_eq!(gas, MAP_FP_TO_G1_GAS);
            assert!(read_g1_subgroup(&out).is_some_and(|p| !p.is_zero()));

            let (ok, out, gas) = run(0x11, &[fp.clone(), fp].concat(), 100_000);
            assert!(ok);
            assert_eq!(gas, MAP_FP2_TO_G2_GAS);
            assert!(read_g2_subgroup(&out).is_some_and(|p| !p.is_zero()));
        }
    }

    #[test]
    fn invalid_inputs_consume_all_gas() {
        let fail = (false, vec![], 50_000);
        // Wrong length.
        assert_eq!(run(0x0b, &[0u8; 255], 50_000), fail);
        assert_eq!(run(0x0c, &[], 50_000), fail);
        // Out of gas.
        assert_eq!(run(0x0b, &[0u8; 256], 374), (false, vec![], 374));
        // Non-zero padding.
        let mut input = [write_g1(&g1()), write_g1(&g1())].concat();
        input[0] = 1;
        assert_eq!(run(0x0b, &input, 50_000), fail);
        // Field element == p (non-canonical).
        let mut fp = vec![0u8; FP_LEN];
        fp[16..].copy_from_slice(&Fq::MODULUS.to_bytes_be());
        assert_eq!(run(0x10, &fp, 50_000), fail);
        // Not on curve.
        let mut input = [write_g1(&g1()), write_g1(&g1())].concat();
        input[127] ^= 1;
        assert_eq!(run(0x0b, &input, 50_000), fail);
    }

    #[test]
    fn subgroup_check_only_for_msm_and_pairing() {
        // An on-curve point outside the subgroup: SSWU+isogeny output before
        // cofactor clearing.
        let p = WBMap::<ark_bls12_381::g1::Config>::map_to_curve(Fq::from(1u64)).unwrap();
        assert!(!p.is_in_correct_subgroup_assuming_on_curve());
        let input = [write_g1(&p), write_g1(&g1())].concat();
        assert!(run(0x0b, &input, 50_000).0);
        let input = [write_g1(&p), scalar(1)].concat();
        assert!(!run(0x0c, &input, 50_000).0);
        let input = [write_g1(&p), write_g2(&g2())].concat();
        assert!(!run(0x0f, &input, 500_000).0);
    }
}
