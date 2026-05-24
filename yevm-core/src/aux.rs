use yevm_base::{Acc, Int};
use yevm_misc::keccak256;

pub fn is_precompile(addr: &Acc) -> bool {
    let id = addr.as_u64();
    addr.as_ref()[..12].iter().all(|&b| b == 0) && ((1u64..=0xa).contains(&id) || id == 0x100)
}

/// CREATE address: keccak256(rlp([sender, nonce]))[12:]
pub fn create_address(sender: &Acc, nonce: u64) -> Acc {
    // RLP([addr, nonce]): list of [20-byte addr, nonce]
    let mut payload = Vec::with_capacity(23);
    payload.push(0x94); // string length 20
    payload.extend_from_slice(sender.as_ref());
    if nonce == 0 {
        payload.push(0x80); // RLP encoding of 0
    } else if nonce < 128 {
        payload.push(nonce as u8);
    } else {
        let n = nonce.to_be_bytes();
        let start = n.iter().position(|&b| b != 0).unwrap_or(7);
        payload.push(0x80 + (8 - start) as u8);
        payload.extend_from_slice(&n[start..]);
    }
    let mut buf = Vec::with_capacity(payload.len() + 1);
    if payload.len() < 56 {
        buf.push(0xc0 + payload.len() as u8);
    } else {
        buf.push(0xf7 + 1); // length of length
        buf.push(payload.len() as u8);
    }
    buf.extend(payload);
    let hash = keccak256(&buf);
    Acc::from(&hash.as_ref()[12..])
}

/// CREATE2 address: keccak256(0xff ++ sender ++ salt ++ keccak256(init_code))[12:]
pub fn create2_address(sender: &Acc, salt: &Int, init_code_hash: &Int) -> Acc {
    let mut preimage = Vec::with_capacity(85);
    preimage.push(0xff);
    preimage.extend_from_slice(sender.as_ref());
    preimage.extend_from_slice(salt.as_ref());
    preimage.extend_from_slice(init_code_hash.as_ref());
    let hash = keccak256(&preimage);
    Acc::from(&hash.as_ref()[12..])
}
