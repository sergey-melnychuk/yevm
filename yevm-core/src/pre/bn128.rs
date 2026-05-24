use bn::{AffineG1, AffineG2, Fq, Fq2, Fr, G1, G2, Group, Gt, pairing};

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

    let p1 = match read_point(&buf[0..64]) {
        Some(p) => p,
        None => return (false, vec![], gas_limit),
    };
    let p2 = match read_point(&buf[64..128]) {
        Some(p) => p,
        None => return (false, vec![], gas_limit),
    };

    let sum = p1 + p2;
    (true, encode_point(sum), GAS)
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

    let p = match read_point(&buf[0..64]) {
        Some(p) => p,
        None => return (false, vec![], gas_limit),
    };
    let scalar = Fr::from_slice(&buf[64..96]).unwrap_or_else(|_| Fr::zero());

    let result = p * scalar;
    (true, encode_point(result), GAS)
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

    let mut acc = Gt::one();
    for i in 0..num_pairs {
        let offset = i * 192;
        let g1 = match read_point(&input[offset..offset + 64]) {
            Some(p) => p,
            None => return (false, vec![], gas_limit),
        };

        // Read G2 point (4 x Fq coordinates: b_x_im, b_x_re, b_y_im, b_y_re)
        let ax = match Fq::from_slice(&input[offset + 64..offset + 96]) {
            Ok(f) => f,
            Err(_) => return (false, vec![], gas_limit),
        };
        let ay = match Fq::from_slice(&input[offset + 96..offset + 128]) {
            Ok(f) => f,
            Err(_) => return (false, vec![], gas_limit),
        };
        let bx = match Fq::from_slice(&input[offset + 128..offset + 160]) {
            Ok(f) => f,
            Err(_) => return (false, vec![], gas_limit),
        };
        let by = match Fq::from_slice(&input[offset + 160..offset + 192]) {
            Ok(f) => f,
            Err(_) => return (false, vec![], gas_limit),
        };

        let b_x = Fq2::new(ay, ax);
        let b_y = Fq2::new(by, bx);

        let g2 = if b_x.is_zero() && b_y.is_zero() {
            G2::zero()
        } else {
            match AffineG2::new(b_x, b_y) {
                Ok(p) => p.into(),
                Err(_) => return (false, vec![], gas_limit),
            }
        };

        acc = acc * pairing(g1, g2);
    }

    let mut out = vec![0u8; 32];
    if acc == Gt::one() {
        out[31] = 1;
    }
    (true, out, gas)
}

fn read_point(data: &[u8]) -> Option<G1> {
    let x = Fq::from_slice(&data[0..32]).ok()?;
    let y = Fq::from_slice(&data[32..64]).ok()?;
    if x.is_zero() && y.is_zero() {
        return Some(G1::zero());
    }
    AffineG1::new(x, y).ok().map(Into::into)
}

fn encode_point(p: G1) -> Vec<u8> {
    let mut out = vec![0u8; 64];
    if p.is_zero() {
        return out;
    }
    if let Some(affine) = AffineG1::from_jacobian(p) {
        affine.x().to_big_endian(&mut out[0..32]).unwrap();
        affine.y().to_big_endian(&mut out[32..64]).unwrap();
    }
    out
}
