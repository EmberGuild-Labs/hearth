//! The fixed 60-byte Wick header.
//!
//! Every Wick file starts with this, and it is deliberately the only part of
//! the format with a fixed layout. A tool can identify a file, its payload
//! format, its spec version and what work reading it will require from one
//! 60-byte read, without parsing anything else.
//!
//! ```text
//! Offset  Size  Field
//! 0x00    4     Magic "WICK"
//! 0x04    2     Format tag, ASCII, e.g. "MT"
//! 0x06    2     Spec version: major byte then minor byte
//! 0x08    4     Flags bitfield (little-endian u32)
//! 0x0C    8     Chunk table offset (little-endian u64)
//! 0x14    8     Chunk table length (little-endian u64)
//! 0x1C    32    BLAKE3 of the chunk table bytes
//! 0x3C          End of header
//! ```
//!
//! Multi-byte integers are little-endian. The version is the one exception:
//! it is stored as two separate bytes, major first, so that a hex dump of
//! `01 00` reads as "v1.0" the way the spec writes it.

use crate::error::{Error, Result};

/// `WICK`. Present at offset 0 of every file in the ecosystem.
pub const MAGIC: [u8; 4] = *b"WICK";

/// Size of the header in bytes. Fixed for all of spec v1.
pub const HEADER_LEN: usize = 0x3C;

/// The spec version this build writes.
pub const SPEC_VERSION: Version = Version { major: 1, minor: 0 };

/// A two-character ASCII format tag: `MT`, `MD`, `MI`, `MC`, `MX`, `AR`.
///
/// The tag is what lets one reader dispatch to the right payload plugin. It
/// lives in the header rather than in the payload so that dispatch costs a
/// 6-byte read.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Tag(pub [u8; 2]);

impl Tag {
    pub const fn new(s: &[u8; 2]) -> Self {
        Tag(*s)
    }

    pub fn as_str(&self) -> &str {
        // Constructors reject non-ASCII, so this cannot fail.
        std::str::from_utf8(&self.0).unwrap_or("??")
    }

    pub fn parse(s: &str) -> Result<Self> {
        let b = s.as_bytes();
        if b.len() != 2 || !b.iter().all(|c| c.is_ascii_graphic()) {
            return Err(Error::BadTag(s.to_string()));
        }
        Ok(Tag([b[0], b[1]]))
    }
}

impl std::fmt::Display for Tag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::fmt::Debug for Tag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Tag({})", self.as_str())
    }
}

/// Spec version, not payload version. A `.emt` written under Wick v1.0 has
/// version 1.0 here regardless of how many times its own text schema changes;
/// payload schema versions live in `SCHM`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Version {
    pub major: u8,
    pub minor: u8,
}

impl Version {
    pub const fn new(major: u8, minor: u8) -> Self {
        Version { major, minor }
    }

    pub fn parse(s: &str) -> Result<Self> {
        let (a, b) = s
            .split_once('.')
            .ok_or_else(|| Error::BadVersion(s.to_string()))?;
        Ok(Version {
            major: a.parse().map_err(|_| Error::BadVersion(s.to_string()))?,
            minor: b.parse().map_err(|_| Error::BadVersion(s.to_string()))?,
        })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// Header flags. A reader decides how much work a file will be before it
/// touches the chunk table: whether signatures need verifying, whether a
/// passphrase will be needed, whether a cheap summary tier exists.
///
/// Note for anyone comparing this against the original design document: §2.4
/// there assigns bit 3 to "encrypted" and bit 4 to "split-trust", while §2.7
/// refers to split-trust as bit 5. The bitfield table wins; this is bit 4.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Flags(pub u32);

impl Flags {
    pub const PROVENANCE: u32 = 1 << 0;
    pub const CAPABILITIES: u32 = 1 << 1;
    pub const SUMMARY: u32 = 1 << 2;
    pub const ENCRYPTED: u32 = 1 << 3;
    pub const SPLIT_TRUST: u32 = 1 << 4;

    pub fn has(self, bit: u32) -> bool {
        self.0 & bit != 0
    }

    pub fn set(&mut self, bit: u32, on: bool) {
        if on {
            self.0 |= bit;
        } else {
            self.0 &= !bit;
        }
    }

    /// Human-readable list, for `hearth info`.
    pub fn names(self) -> Vec<&'static str> {
        let mut v = Vec::new();
        for (bit, name) in [
            (Self::PROVENANCE, "provenance"),
            (Self::CAPABILITIES, "capabilities"),
            (Self::SUMMARY, "summary"),
            (Self::ENCRYPTED, "encrypted"),
            (Self::SPLIT_TRUST, "split-trust"),
        ] {
            if self.has(bit) {
                v.push(name);
            }
        }
        v
    }
}

#[derive(Clone, Debug)]
pub struct Header {
    pub tag: Tag,
    pub version: Version,
    pub flags: Flags,
    pub table_offset: u64,
    pub table_len: u64,
    pub content_hash: [u8; 32],
}

impl Header {
    pub fn new(tag: Tag) -> Self {
        Header {
            tag,
            version: SPEC_VERSION,
            flags: Flags::default(),
            table_offset: HEADER_LEN as u64,
            table_len: 0,
            content_hash: [0; 32],
        }
    }

    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0x00..0x04].copy_from_slice(&MAGIC);
        b[0x04..0x06].copy_from_slice(&self.tag.0);
        b[0x06] = self.version.major;
        b[0x07] = self.version.minor;
        b[0x08..0x0C].copy_from_slice(&self.flags.0.to_le_bytes());
        b[0x0C..0x14].copy_from_slice(&self.table_offset.to_le_bytes());
        b[0x14..0x1C].copy_from_slice(&self.table_len.to_le_bytes());
        b[0x1C..0x3C].copy_from_slice(&self.content_hash);
        b
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() < HEADER_LEN {
            return Err(Error::Truncated("header"));
        }
        if b[0x00..0x04] != MAGIC {
            return Err(Error::NotWick);
        }
        let version = Version {
            major: b[0x06],
            minor: b[0x07],
        };
        // Forward-migrating means failing predictably, not guessing. A major
        // version we do not know changes the header or chunk encoding itself,
        // so nothing below this point can be trusted.
        if version.major != SPEC_VERSION.major {
            return Err(Error::UnsupportedSpec(version));
        }
        Ok(Header {
            tag: Tag([b[0x04], b[0x05]]),
            version,
            flags: Flags(u32::from_le_bytes(b[0x08..0x0C].try_into().unwrap())),
            table_offset: u64::from_le_bytes(b[0x0C..0x14].try_into().unwrap()),
            table_len: u64::from_le_bytes(b[0x14..0x1C].try_into().unwrap()),
            content_hash: b[0x1C..0x3C].try_into().unwrap(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips() {
        let mut h = Header::new(Tag::new(b"MT"));
        h.flags.set(Flags::PROVENANCE, true);
        h.flags.set(Flags::SUMMARY, true);
        h.table_len = 4096;
        h.content_hash = [7; 32];

        let back = Header::decode(&h.encode()).unwrap();
        assert_eq!(back.tag.as_str(), "MT");
        assert_eq!(back.version, SPEC_VERSION);
        assert!(back.flags.has(Flags::PROVENANCE));
        assert!(!back.flags.has(Flags::ENCRYPTED));
        assert_eq!(back.table_len, 4096);
        assert_eq!(back.content_hash, [7; 32]);
    }

    #[test]
    fn header_is_exactly_sixty_bytes() {
        assert_eq!(HEADER_LEN, 60);
        assert_eq!(Header::new(Tag::new(b"MX")).encode().len(), 60);
    }

    #[test]
    fn rejects_foreign_files_and_future_majors() {
        assert!(matches!(
            Header::decode(&[0u8; HEADER_LEN]),
            Err(Error::NotWick)
        ));

        let mut b = Header::new(Tag::new(b"MT")).encode();
        b[0x06] = 9;
        assert!(matches!(Header::decode(&b), Err(Error::UnsupportedSpec(_))));
    }
}
