//! bn254 on herumi's mcl, for native builds: C++ that JIT-compiles its field arithmetic (with
//! `mulx`/`adcx`/`adox` where the CPU has them), several times faster than arkworks here.

use std::sync::{Once, OnceLock};

use mcl_rust::{CurveType, Fp, Fp2, Fr, G1, G2, GT};

// Parts of mcl's C API that `mcl_rust` does not wrap.
unsafe extern "C" {
    fn mclBn_millerLoopVec(z: *mut GT, x: *const G1, y: *const G2, n: usize);
    fn mclBn_verifyOrderG2(do_verify: i32);
    fn mclBnFp_serialize(buf: *mut u8, max_buf_size: usize, x: *const Fp) -> usize;
}

/// The base field modulus p, big-endian.
const P: [u8; 32] = [
    0x30, 0x64, 0x4e, 0x72, 0xe1, 0x31, 0xa0, 0x29, 0xb8, 0x50, 0x45, 0xb6, 0x81, 0x81, 0x58, 0x5d,
    0x97, 0x81, 0x6a, 0x91, 0x68, 0x71, 0xca, 0x8d, 0x3c, 0x20, 0x8c, 0x16, 0xd8, 0x7c, 0xfd, 0x47,
];

/// The curve parameter x (often called u) of alt_bn128.
const X: u64 = 4965661367192848881;

/// Sets mcl up for alt_bn128 once per process. mcl's curve is global; anything else in the process
/// that uses mcl (evm2 in yevm-reth) sets up the same curve.
fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // SNARK is Ethereum's alt_bn128; mcl's BN254 is a different curve.
        assert!(
            mcl_rust::init(CurveType::SNARK),
            "mcl: alt_bn128 setup failed"
        );
        // `read_g2` checks the subgroup with the endomorphism below, much cheaper than mcl's
        // multiplication by r.
        unsafe { mclBn_verifyOrderG2(0) };
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
    if g1s.is_empty() {
        return Some(true);
    }
    // One Miller loop over all pairs and a single final exponentiation.
    let mut miller = GT::zero();
    unsafe { mclBn_millerLoopVec(&mut miller, g1s.as_ptr(), g2s.as_ptr(), g1s.len()) };
    let mut result = GT::zero();
    mcl_rust::final_exp(&mut result, &miller);
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
    let mut le = [0u8; 32];
    let len = unsafe { mclBnFp_serialize(le.as_mut_ptr(), le.len(), fp) };
    assert_eq!(len, 32, "mcl: field element serialization failed");
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
    (point.is_valid() && in_subgroup(&point)).then_some(point)
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

/// Whether a G2 point on the curve is in the order-r subgroup: for BN curves that is
/// `[x + 1]P + ψ([x]P) + ψ²([x]P) = ψ³([2x]P)`, with ψ the untwist-Frobenius-twist endomorphism
/// (El Housni, Guillevic, Piellard, "Co-factor clearing and subgroup membership testing on
/// pairing-friendly curves", eprint 2022/348). It takes a 63-bit multiplication where checking
/// `[r]P = 0` takes a 254-bit one.
fn in_subgroup(p: &G2) -> bool {
    // ψ(x, y) = (conj(x) · c_x, conj(y) · c_y) with c_x = ξ^((p-1)/3), c_y = ξ^((p-1)/2), ξ = u + 9.
    static COEFFS: OnceLock<(Fp2, Fp2)> = OnceLock::new();
    let (cx, cy) = COEFFS.get_or_init(|| {
        let fp2 = |re: &str, im: &str| {
            let mut value = Fp2::zero();
            assert!(value.d[0].set_str(re, 10) && value.d[1].set_str(im, 10));
            value
        };
        (
            fp2(
                "21575463638280843010398324269430826099269044274347216827212613867836435027261",
                "10307601595873709700152284273816112264069230130616436755625194854815875713954",
            ),
            fp2(
                "2821565182194536844548159561693502659359617185244120367078079554186484126554",
                "3505843767911556378687030309984248845540243509899259641013678093033130930403",
            ),
        )
    });
    let conj = |a: &Fp2| {
        let mut c = a.clone();
        Fp::neg(&mut c.d[1], &a.d[1]);
        c
    };
    // Conjugation is a field automorphism, so it applies to Jacobian coordinates as well.
    let psi = |a: &G2| {
        let mut image = G2::zero();
        (image.x, image.y, image.z) = (&conj(&a.x) * cx, &conj(&a.y) * cy, conj(&a.z));
        image
    };

    // [x]P by double-and-add: mcl's `G2::mul` uses the GLV endomorphism, which gives the right
    // product only for points already in the subgroup.
    let mut xp = p.clone();
    for bit in (0..X.ilog2()).rev() {
        let mut doubled = G2::zero();
        G2::dbl(&mut doubled, &xp);
        xp = doubled;
        if (X >> bit) & 1 == 1 {
            xp += p;
        }
    }
    let psi_xp = psi(&xp);
    let psi2_xp = psi(&psi_xp);
    let mut rhs = G2::zero();
    G2::dbl(&mut rhs, &psi(&psi2_xp));
    let mut lhs = psi2_xp;
    lhs += &psi_xp;
    lhs += &xp;
    lhs += p;
    lhs == rhs
}
