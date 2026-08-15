//! Lowercase hex, because hashes, keys and signatures all cross the JSON
//! boundary in `PROV` and `KEYS` and need one canonical spelling.
//!
//! This is a dependency the project deliberately does not take. Hex is
//! fifteen lines and every crate in a public repository is one more thing a
//! newcomer has to audit before they trust the build.

const DIGITS: &[u8; 16] = b"0123456789abcdef";

pub fn encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 0x0F) as usize] as char);
    }
    s
}

/// Returns `None` rather than an error type: every caller is already
/// producing its own domain-specific message about *what* failed to parse.
pub fn decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if !b.len().is_multiple_of(2) {
        return None;
    }
    let nibble = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(b.len() / 2);
    for pair in b.chunks(2) {
        out.push((nibble(pair[0])? << 4) | nibble(pair[1])?);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let bytes: Vec<u8> = (0u8..=255).collect();
        assert_eq!(decode(&encode(&bytes)).unwrap(), bytes);
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(decode("abc").is_none());
        assert!(decode("zz").is_none());
        assert_eq!(decode("").unwrap(), Vec::<u8>::new());
        assert_eq!(decode("FF").unwrap(), vec![255]);
    }
}
