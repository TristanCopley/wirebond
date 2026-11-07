//! Standard base64 with padding: the control protocol's `key` (SPEC §16) and [`crate::Msg::k`]. As a
//! serde `with` module: `#[serde(with = "wirebond_core::b64")]` on a `Vec<u8>`.

use serde::{Deserialize, Deserializer, Serializer};

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        s.extend((0..4).map(|i| if i <= c.len() { ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' }));
    }
    s
}

/// Reads what `encode` writes: each four characters are three bytes, and one or two `=` that end
/// them stand for a byte or two fewer. Anything else is None.
pub fn decode(s: &str) -> Option<Vec<u8>> {
    let s = s.as_bytes();
    if !s.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for c in s.chunks(4) {
        let pad = c.iter().rev().take_while(|&&ch| ch == b'=').count();
        if pad > 2 {
            return None;
        }
        let mut n = 0u32;
        for &ch in &c[..4 - pad] {
            n = n << 6 | ALPHABET.iter().position(|&a| a == ch)? as u32;
        }
        out.extend_from_slice(&(n << (6 * pad)).to_be_bytes()[1..4 - pad]);
    }
    Some(out)
}

pub fn serialize<S: Serializer>(b: &[u8], s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&encode(b))
}

pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    decode(&String::deserialize(d)?).ok_or_else(|| serde::de::Error::custom("bad base64"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn round_trip() {
        for n in 0..70 {
            let b: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(super::decode(&super::encode(&b)).unwrap(), b);
        }
        assert_eq!(super::encode(b"wirebond"), "d2lyZWJvbmQ=");
        assert!(super::decode("d2lyZWJvbmQ").is_none());
    }
}
