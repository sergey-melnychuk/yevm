pub mod acc;
pub mod dto;
pub mod int;

pub use acc::*;
pub use int::*;

#[cfg(test)]
mod tests {
    use yevm_misc::hex::Hex;

    use super::*;

    fn roundtrip<T: for<'a> From<&'a [u8]> + AsRef<[u8]>>(s: &str) -> String {
        let buf = hex::decode(s).unwrap_or_default();
        let t = T::from(&buf);
        hex::encode(t.as_ref())
    }

    fn check<const N: usize>(s: &str) {
        let actual = roundtrip::<Hex<N>>(s);
        let expected = s.chars().take(s.len().min(N * 2)).collect::<String>();
        let zeroes = N * 2 - (N * 2).min(s.len());
        let zeroes = "0".repeat(zeroes);
        let expected = format!("{zeroes}{expected}");
        assert_eq!(actual, expected, "case '{s}'");
    }

    #[test]
    fn test_int_roundtrip() {
        for s in [
            "ff",
            "deadbeef",
            "0102030405060708090a",
            "aabbccddeeff00112233445566778899aabbccdd",
            "aabbccddeeff00112233445566778899aabbccddee",
            "aabbccddeeff00112233445566778899aabbccddeeff001122",
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
        ] {
            check::<{ Int::N }>(s);
        }
    }

    #[test]
    fn test_acc_roundtrip() {
        for s in [
            "ff",
            "deadbeef",
            "0102030405060708090a",
            "aabbccddeeff00112233445566778899aabbccdd",
        ] {
            check::<{ Acc::N }>(s);
        }
    }
}
