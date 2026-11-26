//! bn254 on herumi's mcl, for native builds: C++ that JIT-compiles its field arithmetic (with
//! `mulx`/`adcx`/`adox` where the CPU has them), several times faster than arkworks here.

use std::sync::Once;

use mcl_rust::{CurveType, Fp, Fp2, Fr, G1, G2, GT};

/// The base field modulus p, big-endian.
const P: [u8; 32] = [
    0x30, 0x64, 0x4e, 0x72, 0xe1, 0x31, 0xa0, 0x29, 0xb8, 0x50, 0x45, 0xb6, 0x81, 0x81, 0x58, 0x5d,
    0x97, 0x81, 0x6a, 0x91, 0x68, 0x71, 0xca, 0x8d, 0x3c, 0x20, 0x8c, 0x16, 0xd8, 0x7c, 0xfd, 0x47,
];

/// Sets mcl up for alt_bn128 once per process. mcl's curve is global; anything else in the process
/// that uses mcl (evm2 in yevm-reth) sets up the same curve.
fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // SNARK is Ethereum's alt_bn128; mcl's BN254 is a different curve.
        // mcl's validity check of a G2 point includes the subgroup check.
        assert!(
            mcl_rust::init(CurveType::SNARK),
            "mcl: alt_bn128 setup failed"
        );
    });
}

pub(super) fn add(a: &[u8], b: &[u8]) -> Option<[u8; 64]> {
    init();
    let (a, b) = (read_g1(a)?, read_g1(b)?);
    let mut sum = G1::zero();
    G1::add(&mut sum, &a, &b);
    Some(encode_g1(&sum))
}

pub(super) fn mul(point: &[u8], scalar: &[u8]) -> Option<[u8; 64]> {
    init();
    let point = read_g1(point)?;
    let mut le: [u8; 32] = scalar.try_into().ok()?;
    le.reverse();
    let mut s = Fr::zero();
    // Any 256-bit scalar, reduced modulo the group order, as in geth.
    s.set_little_endian_mod(&le);
    let mut product = G1::zero();
    G1::mul(&mut product, &point, &s);
    Some(encode_g1(&product))
}

pub(super) fn pairing(input: &[u8]) -> Option<bool> {
    init();
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
    // A Miller loop per pair, their product, and a single final exponentiation.
    let mut product = GT::from_int(1);
    for (g1, g2) in g1s.iter().zip(&g2s) {
        let mut miller = GT::zero();
        mcl_rust::miller_loop(&mut miller, g1, g2);
        product = &product * &miller;
    }
    let mut result = GT::zero();
    mcl_rust::final_exp(&mut result, &product);
    Some(result.is_one())
}

/// A field element from 32 big-endian bytes, `None` unless it is below p.
fn read_fp(bytes: &[u8]) -> Option<Fp> {
    if bytes >= &P[..] {
        return None;
    }
    let mut le: [u8; 32] = bytes.try_into().ok()?;
    le.reverse();
    let mut fp = Fp::zero();
    fp.set_little_endian(&le).then_some(fp)
}

fn write_fp(fp: &Fp, out: &mut [u8]) {
    let mut le = fp.serialize();
    assert_eq!(le.len(), 32, "mcl: field element serialization failed");
    le.reverse();
    out.copy_from_slice(&le);
}

/// A G1 point from x ++ y (64 bytes), (0, 0) being the point at infinity. G1's cofactor is 1,
/// so every point on the curve is in the group.
fn read_g1(bytes: &[u8]) -> Option<G1> {
    let (x, y) = (read_fp(&bytes[..32])?, read_fp(&bytes[32..64])?);
    if x.is_zero() && y.is_zero() {
        return Some(G1::zero());
    }
    let mut point = G1::zero();
    (point.x, point.y, point.z) = (x, y, Fp::from_int(1));
    point.is_valid().then_some(point)
}

/// A G2 point from x_im ++ x_re ++ y_im ++ y_re (128 bytes), (0, 0) being the point at infinity;
/// it must be on the curve and in the order-r subgroup.
fn read_g2(bytes: &[u8]) -> Option<G2> {
    let fp2 = |im: &[u8], re: &[u8]| -> Option<Fp2> {
        let mut value = Fp2::zero();
        (value.d[0], value.d[1]) = (read_fp(re)?, read_fp(im)?);
        Some(value)
    };
    let x = fp2(&bytes[0..32], &bytes[32..64])?;
    let y = fp2(&bytes[64..96], &bytes[96..128])?;
    if x.is_zero() && y.is_zero() {
        return Some(G2::zero());
    }
    let mut point = G2::zero();
    (point.x, point.y) = (x, y);
    point.z.d[0] = Fp::from_int(1);
    point.is_valid().then_some(point)
}

/// x ++ y, all zero for the point at infinity.
fn encode_g1(point: &G1) -> [u8; 64] {
    let mut out = [0u8; 64];
    if !point.is_zero() {
        let mut affine = G1::zero();
        G1::normalize(&mut affine, point);
        write_fp(&affine.x, &mut out[..32]);
        write_fp(&affine.y, &mut out[32..]);
    }
    out
}
