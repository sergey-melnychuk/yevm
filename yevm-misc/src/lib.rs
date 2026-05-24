use crate::hex::Hex;

pub mod buf;
pub mod hex;
pub mod http;

#[cfg(not(target_arch = "wasm32"))]
pub fn keccak256(data: &[u8]) -> Hex<32> {
    Hex::new(keccak_asm::Keccak256::digest(data).into())
}

#[cfg(target_arch = "wasm32")]
pub fn keccak256(data: &[u8]) -> Hex<32> {
    use tiny_keccak::{Hasher, Keccak};
    let mut h = Keccak::v256();
    let mut out = [0u8; 32];
    h.update(data);
    h.finalize(&mut out);
    Hex::new(out)
}
