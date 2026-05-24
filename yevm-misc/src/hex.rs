use std::fmt;

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hex<const N: usize>([u8; N]);

impl<const N: usize> Hex<N> {
    pub const N: usize = N;
    pub const ZERO: Self = Self::zero();
    pub const ONE: Self = Self::one();
    pub const MAX: Self = Self::max();

    pub const fn new(bytes: [u8; N]) -> Self {
        Self(bytes)
    }

    pub const fn zero() -> Self {
        Self([0; N])
    }

    pub const fn one() -> Self {
        let mut buf = [0; N];
        buf[N - 1] = 1;
        Self(buf)
    }

    pub const fn max() -> Self {
        Self([0xff; N])
    }

    pub fn is_zero(&self) -> bool {
        self == &Self::ZERO
    }

    pub fn as_usize(&self) -> usize {
        const K: usize = size_of::<usize>();
        let mut b = [0u8; K];
        b.copy_from_slice(self.to::<K>().as_ref());
        usize::from_be_bytes(b)
    }

    pub fn as_usize_checked(&self) -> Option<usize> {
        const K: usize = size_of::<usize>();
        if self.0[..N - K].iter().any(|&b| b != 0) {
            return None;
        }
        Some(self.as_usize())
    }

    pub fn as_u128(&self) -> u128 {
        const K: usize = size_of::<u128>();
        let mut b = [0u8; K];
        b.copy_from_slice(self.to::<K>().as_ref());
        u128::from_be_bytes(b)
    }

    pub fn as_u64(&self) -> u64 {
        const K: usize = size_of::<u64>();
        let mut b = [0u8; K];
        b.copy_from_slice(self.to::<K>().as_ref());
        u64::from_be_bytes(b)
    }

    pub fn as_u32(&self) -> u32 {
        const K: usize = size_of::<u32>();
        let mut b = [0u8; K];
        b.copy_from_slice(self.to::<K>().as_ref());
        u32::from_be_bytes(b)
    }

    pub fn as_u16(&self) -> u16 {
        const K: usize = size_of::<u16>();
        let mut b = [0u8; K];
        b.copy_from_slice(self.to::<K>().as_ref());
        u16::from_be_bytes(b)
    }

    pub fn as_u8(&self) -> u8 {
        self.0[N - 1]
    }
}

impl<const N: usize> AsRef<[u8]> for Hex<N> {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl<const N: usize> From<&[u8]> for Hex<N> {
    fn from(value: &[u8]) -> Self {
        assert!(value.len() <= N, "data loss detected");
        let mut buffer = [0u8; N];
        let take = value.len().min(N);
        let skip = N - take;
        buffer[skip..].copy_from_slice(&value[..take]);
        Self(buffer)
    }
}

impl<const N: usize> From<usize> for Hex<N> {
    fn from(value: usize) -> Self {
        Self::from(&value.to_be_bytes()[..])
    }
}

impl<const N: usize> From<u128> for Hex<N> {
    fn from(value: u128) -> Self {
        Self::from(&value.to_be_bytes()[..])
    }
}

impl<const N: usize> From<u64> for Hex<N> {
    fn from(value: u64) -> Self {
        Self::from(&value.to_be_bytes()[..])
    }
}

impl<const N: usize> From<u32> for Hex<N> {
    fn from(value: u32) -> Self {
        Self::from(&value.to_be_bytes()[..])
    }
}

impl<const N: usize> From<u16> for Hex<N> {
    fn from(value: u16) -> Self {
        Self::from(&value.to_be_bytes()[..])
    }
}

impl<const N: usize> From<u8> for Hex<N> {
    fn from(value: u8) -> Self {
        Self::from(&[value][..])
    }
}

impl<const N: usize> From<i32> for Hex<N> {
    fn from(value: i32) -> Self {
        Self::from(value.max(0) as u32)
    }
}

impl<const N: usize> From<i64> for Hex<N> {
    fn from(value: i64) -> Self {
        Self::from(value.max(0) as u64)
    }
}

impl<const N: usize> Hex<N> {
    pub fn to<const M: usize>(self) -> Hex<M> {
        let mut buffer = [0; M];
        if M > N {
            buffer[(M - N)..].copy_from_slice(&self.0);
        } else {
            buffer[..].copy_from_slice(&self.0[(N - M)..]);
        }
        Hex(buffer)
    }
}

impl<const N: usize> Default for Hex<N> {
    fn default() -> Self {
        Self([0; N])
    }
}

impl<const N: usize> fmt::Display for Hex<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("0x")?;
        for b in &self.0 {
            write!(f, "{:02x}", b)?;
        }
        Ok(())
    }
}

impl<const N: usize> fmt::Debug for Hex<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("0x")?;
        for b in &self.0 {
            write!(f, "{:02x}", b)?;
        }
        Ok(())
    }
}

impl<const N: usize> serde::Serialize for Hex<N> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de, const N: usize> serde::Deserialize<'de> for Hex<N> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        let hex = s.strip_prefix("0x").unwrap_or(&s);
        let ok = hex.as_bytes().iter().all(|&c| c.is_ascii_hexdigit());
        if !ok {
            return Err(serde::de::Error::custom("hex literal invalid"));
        }
        if hex.len() > 2 * N {
            return Err(serde::de::Error::custom("hex literal too long"));
        }
        let bytes = parse(&s);
        Ok(Self(bytes))
    }
}

pub fn parse_vec(s: &str) -> Result<Vec<u8>, &'static str> {
    let skip = if s.len() >= 2 && s.as_bytes()[0] == b'0' && s.as_bytes()[1] == b'x' {
        2
    } else {
        0
    };
    let len = s.len() - skip;
    let n = len.div_ceil(2);
    let offset = n * 2 - len;
    let mut ret = vec![0u8; n];
    for j in skip..s.len() {
        let c = s.as_bytes()[j];
        let d = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => return Err("hex literal invalid"),
        };
        let k = j - skip;
        let i = offset + k;
        let b = i / 2;
        if i.is_multiple_of(2) {
            ret[b] = d << 4;
        } else {
            ret[b] |= d;
        }
    }
    Ok(ret)
}

pub const fn parse<const N: usize>(s: &str) -> [u8; N] {
    let skip = if s.len() >= 2 && s.as_bytes()[0] == b'0' && s.as_bytes()[1] == b'x' {
        2
    } else {
        0
    };
    if s.len() - skip > N * 2 {
        panic!("hex literal too long");
    }
    let len = s.len() - skip;
    let offset = N * 2 - len;
    let mut ret = [0u8; N];
    let mut j = skip;
    while j < s.len() {
        let c = s.as_bytes()[j];
        let d = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => panic!("hex literal invalid"),
        };
        let i = offset + (j - skip);
        let b = i / 2;
        if i.is_multiple_of(2) {
            ret[b] = d << 4;
        } else {
            ret[b] |= d;
        }
        j += 1;
    }
    ret
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_empty() {
        assert_eq!(parse::<4>(""), [0x00, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn test_parse_single_nibble() {
        assert_eq!(parse::<1>("f"), [0x0f]);
        assert_eq!(parse::<4>("f"), [0x00, 0x00, 0x00, 0x0f]);
    }

    #[test]
    fn test_parse_single_byte() {
        assert_eq!(parse::<1>("ff"), [0xff]);
        assert_eq!(parse::<4>("ff"), [0x00, 0x00, 0x00, 0xff]);
    }

    #[test]
    fn test_parse_exact() {
        assert_eq!(parse::<4>("deadbeef"), [0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(parse::<4>("00000000"), [0x00, 0x00, 0x00, 0x00]);
        assert_eq!(parse::<4>("ffffffff"), [0xff, 0xff, 0xff, 0xff]);
    }

    #[test]
    fn test_parse_short() {
        assert_eq!(parse::<4>("beef"), [0x00, 0x00, 0xbe, 0xef]);
        assert_eq!(parse::<4>("1"), [0x00, 0x00, 0x00, 0x01]);
    }

    #[test]
    fn test_parse_with_prefix() {
        assert_eq!(parse::<4>("0xdeadbeef"), [0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(parse::<1>("0xff"), [0xff]);
        assert_eq!(parse::<4>("0xbeef"), [0x00, 0x00, 0xbe, 0xef]);
        assert_eq!(parse::<4>("0x1"), [0x00, 0x00, 0x00, 0x01]);
    }

    #[test]
    #[should_panic]
    fn test_parse_too_long() {
        let _ = parse::<4>("deadbeef00");
    }

    #[test]
    #[should_panic]
    fn test_parse_too_long_with_prefix() {
        let _ = parse::<4>("0xdeadbeef00");
    }

    #[test]
    #[should_panic]
    fn test_parse_invalid_char() {
        let _ = parse::<4>("xyz0");
    }

    #[test]
    fn test_display() {
        let h = Hex::<4>::from(&[0x00, 0x00, 0xbe, 0xef][..]);
        assert_eq!(h.to_string(), "0x0000beef");
        let z = Hex::<4>::ZERO;
        assert_eq!(z.to_string(), "0x00000000");
        let one = Hex::<4>::ONE;
        assert_eq!(one.to_string(), "0x00000001");
    }

    #[test]
    fn test_debug() {
        let h = Hex::<4>::from(&[0x00, 0x00, 0xbe, 0xef][..]);
        assert_eq!(format!("{:?}", h), "0x0000beef");
    }

    #[test]
    fn test_serde_roundtrip() {
        let h = Hex::<4>::from(&[0xde, 0xad, 0xbe, 0xef][..]);
        let s = serde_json::to_string(&h).unwrap();
        let h2: Hex<4> = serde_json::from_str(&s).unwrap();
        assert_eq!(h, h2);
    }

    #[test]
    fn test_parse_vec_empty() {
        assert_eq!(parse_vec("").unwrap(), vec![] as Vec<u8>);
        assert_eq!(parse_vec("0x").unwrap(), vec![] as Vec<u8>);
    }

    #[test]
    fn test_parse_vec_no_prefix() {
        assert_eq!(parse_vec("abcd").unwrap(), vec![0xab, 0xcd]);
        assert_eq!(parse_vec("ff").unwrap(), vec![0xff]);
        assert_eq!(parse_vec("1").unwrap(), vec![0x01]);
        assert_eq!(parse_vec("deadbeef").unwrap(), vec![0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn test_parse_vec_with_prefix() {
        assert_eq!(parse_vec("0xabcd").unwrap(), vec![0xab, 0xcd]);
        assert_eq!(parse_vec("0xff").unwrap(), vec![0xff]);
        assert_eq!(parse_vec("0x1").unwrap(), vec![0x01]);
        assert_eq!(
            parse_vec("0xdeadbeef").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
    }

    #[test]
    fn test_parse_vec_odd_nibble() {
        assert_eq!(parse_vec("abc").unwrap(), vec![0x0a, 0xbc]);
        assert_eq!(parse_vec("0xabc").unwrap(), vec![0x0a, 0xbc]);
    }

    #[test]
    fn test_parse_vec_invalid_char() {
        assert!(parse_vec("0xgg").is_err());
    }
}
