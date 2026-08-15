//! **Wick** — the container spine every Ember file format sits on.
//!
//! `.emt`, `.emd`, `.emi`, `.emc` and `.emx` are not five file formats. They
//! are one container with five payload schemas, and this crate is the
//! container. A format plugin describes what goes inside `DATA`; everything
//! below is written once and shared:
//!
//! | Property | Where it lives | What it buys |
//! |---|---|---|
//! | Self-describing | [`schema`] — the `SCHM` chunk | validation rules travel with the data, so they cannot drift out of sync with it |
//! | Semantically diffable | [`chunks`] + [`diff`] | payloads are a chunk tree, so a diff reports meaning, not byte noise |
//! | Provenance-aware | [`provenance`] — the `PROV` chunk | a hash-linked, signed edit history a file can prove on its own |
//! | Tiered | [`file::Peek`] — the `SUMM` chunk | "what is this, roughly" without parsing the payload |
//! | Forward-migrating | [`migrate`] — the `MIGR` chunk | old files carry their own upgrade rules; readers fail predictably, never silently |
//!
//! Two properties the design document raised as open questions are settled
//! here at the chunk level rather than the file level: compression and
//! encryption are both per-chunk, so a summary can be read without
//! decompressing a payload and secrets can sit beside plaintext config in one
//! file. See [`chunks`] for the encoding.
//!
//! # Reading a file
//!
//! ```no_run
//! use libwick::{WickFile, Peek, ChunkType};
//!
//! // Cheap: header and chunk offsets only.
//! let peek = Peek::open("report.emt")?;
//! println!("{} v{}", peek.tag(), peek.version());
//!
//! // Full: verifies the content hash, decodes the tree.
//! let file = WickFile::read("report.emt")?;
//! for section in file.data()?.iter() {
//!     println!("{} ({} bytes)", section.ty, section.value.len());
//! }
//! # Ok::<(), libwick::Error>(())
//! ```

pub mod caps;
pub mod chunks;
pub mod crypto;
pub mod diff;
pub mod error;
pub mod file;
pub mod header;
pub mod hex;
pub mod migrate;
pub mod plugin;
pub mod provenance;
pub mod schema;
pub mod time;
pub mod value;

pub use caps::{Access, Capabilities, Grant};
pub use chunks::{Chunk, ChunkList, ChunkRef, ChunkType, Encoding, Nested};
pub use crypto::{Identity, KeyRing};
pub use diff::{Change, ChangeKind};
pub use error::{Error, Result};
pub use file::{Peek, WickFile};
pub use header::{Flags, Header, Tag, Version, HEADER_LEN, MAGIC, SPEC_VERSION};
pub use migrate::{Op, Rule, RuleSet};
pub use plugin::{Enough, Payload, Plugin, RenderOpts, Source};
pub use provenance::{Chain, ChainReport, ProvEntry};
pub use schema::{FieldRule, Issue, Schema, Severity};
pub use value::Value;

/// Version of this crate, reported in `PROV` entries so a file records which
/// build of the spine wrote it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Identify a Wick file from its first bytes without opening a reader.
///
/// Returns `None` for anything that is not a Wick file, which is how the
/// converter decides whether a path is a legacy file to import or an Ember
/// file to export.
pub fn sniff(bytes: &[u8]) -> Option<(Tag, Version)> {
    if bytes.len() < HEADER_LEN || bytes[..4] != MAGIC {
        return None;
    }
    Some((
        Tag([bytes[4], bytes[5]]),
        Version {
            major: bytes[6],
            minor: bytes[7],
        },
    ))
}

/// The same check against a path, reading only the header.
pub fn sniff_path(path: impl AsRef<std::path::Path>) -> Option<(Tag, Version)> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = [0u8; HEADER_LEN];
    let n = f.read(&mut head).ok()?;
    sniff(&head[..n])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_identifies_wick_files_and_rejects_others() {
        let mut f = WickFile::new(Tag::new(b"MX"));
        f.set_data(&ChunkList::new()).unwrap();
        let bytes = f.to_bytes().unwrap();

        let (tag, ver) = sniff(&bytes).unwrap();
        assert_eq!(tag.as_str(), "MX");
        assert_eq!(ver, SPEC_VERSION);

        assert!(sniff(b"PK\x03\x04 this is a zip").is_none());
        assert!(sniff(b"").is_none());
    }
}
