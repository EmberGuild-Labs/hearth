//! One error type for the whole spine.
//!
//! Callers get a typed error rather than a string because two of them are
//! load-bearing: `UnsupportedSpec` is what triggers a migration attempt, and
//! `NeedKey` is what tells a CLI to prompt for a passphrase.

use crate::header::Version;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    /// The file does not begin with `WICK`.
    NotWick,
    /// A structure ended before it said it would.
    Truncated(&'static str),
    /// The spec major version is one this build cannot parse at all.
    UnsupportedSpec(Version),
    /// The stored content hash does not match the bytes on disk.
    HashMismatch {
        expected: String,
        actual: String,
    },
    BadTag(String),
    BadVersion(String),
    /// A chunk type must be four printable ASCII bytes.
    BadChunkType(String),
    /// A chunk's value encoding preamble names a codec this build lacks.
    UnknownCodec(u8),
    /// The chunk is encrypted to a key slot no key was supplied for.
    NeedKey {
        slot: u8,
        label: String,
    },
    /// Decryption produced a MAC failure: wrong passphrase, or tampering.
    Decrypt {
        slot: u8,
    },
    /// A required chunk is absent.
    MissingChunk(&'static str),
    /// Provenance chain verification failed, with the index of the bad entry.
    Provenance {
        entry: usize,
        why: String,
    },
    /// No migration path exists from the file's version to the target.
    NoMigrationPath {
        from: Version,
        to: Version,
    },
    Json(serde_json::Error),
    Io(std::io::Error),
    /// Anything a caller wants to report through the same channel.
    Other(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotWick => write!(f, "not a Wick file (no WICK magic at offset 0)"),
            Error::Truncated(what) => write!(f, "file is truncated: {what} ends early"),
            Error::UnsupportedSpec(v) => write!(
                f,
                "file is Wick spec v{v}, this build understands v{}.x",
                crate::header::SPEC_VERSION.major
            ),
            Error::HashMismatch { expected, actual } => write!(
                f,
                "content hash mismatch: header says {expected}, payload hashes to {actual}"
            ),
            Error::BadTag(s) => write!(f, "'{s}' is not a two-character format tag"),
            Error::BadVersion(s) => write!(f, "'{s}' is not a major.minor version"),
            Error::BadChunkType(s) => {
                write!(f, "'{s}' is not a four-character ASCII chunk type")
            }
            Error::UnknownCodec(c) => write!(
                f,
                "chunk uses value codec {c}, which this build does not know"
            ),
            Error::NeedKey { slot, label } => {
                write!(
                    f,
                    "chunk is encrypted to key slot {slot} ({label}); no key supplied"
                )
            }
            Error::Decrypt { slot } => write!(
                f,
                "could not decrypt key slot {slot}: wrong passphrase, or the chunk was altered"
            ),
            Error::MissingChunk(t) => write!(f, "file has no {t} chunk"),
            Error::Provenance { entry, why } => {
                write!(f, "provenance entry {entry} failed verification: {why}")
            }
            Error::NoMigrationPath { from, to } => {
                write!(f, "no MIGR rule chain from v{from} to v{to}")
            }
            Error::Json(e) => write!(f, "malformed JSON in chunk: {e}"),
            Error::Io(e) => write!(f, "{e}"),
            Error::Other(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}
