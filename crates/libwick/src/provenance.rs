//! The `PROV` chunk: a hash-linked, signed record of everything that has
//! touched this file.
//!
//! Each entry commits to the previous entry's hash, so removing an entry or
//! reordering two of them breaks every link after it — the same construction
//! as a git commit graph, scoped to a single file. A file can therefore prove
//! its own history without anyone having to trust an external log.
//!
//! Signing is optional and per-entry. An unsigned entry is still hash-linked,
//! so it cannot be altered without detection, but it does not prove *who*
//! made the change. `hearth verify-chain` reports the difference plainly
//! rather than treating "unsigned" as "valid".
//!
//! ## Canonical form
//!
//! Signatures are over a canonical byte string, not over the stored JSON,
//! because JSON has no canonical spelling — key order, whitespace and number
//! formatting all vary by writer, and a signature over one writer's output
//! would not verify against another's. The canonical form is a fixed
//! sequence of length-prefixed fields, so it is identical everywhere.

use crate::chunks::{Chunk, ChunkType};
use crate::crypto::{self, Identity};
use crate::error::{Error, Result};
use crate::hex;
use serde::{Deserialize, Serialize};

/// Bumped if the canonical form ever changes, so old signatures are read
/// with the rules they were made under instead of silently failing.
const CANON_VERSION: &str = "wick-prov-1";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProvEntry {
    /// The tool and version that made the change: "Hearth v0.1.0".
    pub tool: String,
    /// What it did, in human words: "converted from legacy .pdf".
    pub action: String,
    /// RFC 3339, UTC.
    pub timestamp: String,
    /// BLAKE3 of the previous entry's canonical form. `None` for the first.
    #[serde(default)]
    pub prev_hash: Option<String>,
    /// BLAKE3 of the file's payload as of this entry. Optional, because the
    /// hash cannot be known until after the entry is embedded — writers that
    /// can compute it in a second pass should.
    #[serde(default)]
    pub content_hash: Option<String>,
    /// Hex Ed25519 public key of the signer. The design document omits this,
    /// but a chain nobody can name the signer of is not verifiable; without
    /// it a verifier has no key to check the signature against.
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub signature: Option<String>,
}

impl ProvEntry {
    pub fn new(tool: &str, action: &str) -> Self {
        ProvEntry {
            tool: tool.to_string(),
            action: action.to_string(),
            timestamp: crate::time::now_rfc3339(),
            prev_hash: None,
            content_hash: None,
            key: None,
            signature: None,
        }
    }

    /// The bytes a signature covers and a hash is taken over. Every field
    /// except the signature itself is included, each length-prefixed so that
    /// no combination of field values can be rearranged into another.
    pub fn canonical(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut field = |s: &str| {
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        };
        field(CANON_VERSION);
        field(&self.tool);
        field(&self.action);
        field(&self.timestamp);
        field(self.prev_hash.as_deref().unwrap_or(""));
        field(self.content_hash.as_deref().unwrap_or(""));
        field(self.key.as_deref().unwrap_or(""));
        out
    }

    /// This entry's hash, which the next entry will point back to.
    pub fn hash(&self) -> String {
        hex::encode(blake3::hash(&self.canonical()).as_bytes())
    }

    pub fn sign_with(&mut self, id: &Identity) {
        self.key = Some(id.public_hex());
        // The key is part of the canonical form, so it must be set before
        // signing, not after.
        self.signature = Some(id.sign(&self.canonical()));
    }

    pub fn is_signed(&self) -> bool {
        self.signature.is_some() && self.key.is_some()
    }
}

/// The whole chain, oldest first.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Chain(pub Vec<ProvEntry>);

impl Chain {
    pub fn new() -> Self {
        Chain(Vec::new())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn entries(&self) -> &[ProvEntry] {
        &self.0
    }

    /// Link an entry onto the end and sign it if an identity is available.
    pub fn append(&mut self, mut e: ProvEntry, id: Option<&Identity>) {
        e.prev_hash = self.0.last().map(|p| p.hash());
        if let Some(id) = id {
            e.sign_with(id);
        }
        self.0.push(e);
    }

    pub fn decode(chunk: &Chunk) -> Result<Self> {
        Ok(serde_json::from_slice(&chunk.value)?)
    }

    pub fn encode(&self) -> Result<Chunk> {
        Ok(Chunk::new(ChunkType::PROV, serde_json::to_vec(&self.0)?))
    }

    /// Walk the chain and check every link and every signature.
    ///
    /// Returns a report instead of a bool because "valid but unsigned",
    /// "valid and signed by two different keys" and "broken at entry 3" are
    /// three different answers a user needs to be able to tell apart.
    pub fn verify(&self) -> ChainReport {
        let mut report = ChainReport {
            entries: self.0.len(),
            signed: 0,
            unsigned: 0,
            signers: Vec::new(),
            broken_at: None,
        };

        for (i, e) in self.0.iter().enumerate() {
            let expected = if i == 0 {
                None
            } else {
                Some(self.0[i - 1].hash())
            };
            if e.prev_hash != expected {
                report.broken_at = Some(Error::Provenance {
                    entry: i,
                    why: match (&e.prev_hash, &expected) {
                        (None, Some(_)) => "entry claims to be first but is not".into(),
                        (Some(_), None) => {
                            "first entry points at a predecessor that is not in the file".into()
                        }
                        _ => "prev_hash does not match the previous entry".into(),
                    },
                });
                return report;
            }

            match (&e.key, &e.signature) {
                (Some(k), Some(s)) => match crypto::verify(k, s, &e.canonical()) {
                    Ok(()) => {
                        report.signed += 1;
                        if !report.signers.contains(k) {
                            report.signers.push(k.clone());
                        }
                    }
                    Err(why) => {
                        report.broken_at = Some(Error::Provenance { entry: i, why });
                        return report;
                    }
                },
                (None, None) => report.unsigned += 1,
                _ => {
                    report.broken_at = Some(Error::Provenance {
                        entry: i,
                        why: "entry has a key without a signature, or the reverse".into(),
                    });
                    return report;
                }
            }
        }
        report
    }
}

#[derive(Debug)]
pub struct ChainReport {
    pub entries: usize,
    pub signed: usize,
    pub unsigned: usize,
    /// Distinct public keys that signed at least one entry.
    pub signers: Vec<String>,
    pub broken_at: Option<Error>,
}

impl ChainReport {
    pub fn is_intact(&self) -> bool {
        self.broken_at.is_none()
    }

    /// Intact *and* every entry attributable to a key.
    pub fn is_fully_signed(&self) -> bool {
        self.is_intact() && self.unsigned == 0 && self.entries > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain_of(n: usize, id: Option<&Identity>) -> Chain {
        let mut c = Chain::new();
        for i in 0..n {
            c.append(ProvEntry::new("Hearth v0.1.0", &format!("step {i}")), id);
        }
        c
    }

    #[test]
    fn an_unsigned_chain_is_intact_but_not_attributable() {
        let c = chain_of(3, None);
        let r = c.verify();
        assert!(r.is_intact());
        assert!(!r.is_fully_signed());
        assert_eq!(r.unsigned, 3);
    }

    #[test]
    fn a_signed_chain_verifies_and_names_its_signer() {
        let id = Identity::generate().unwrap();
        let c = chain_of(3, Some(&id));
        let r = c.verify();
        assert!(r.is_fully_signed());
        assert_eq!(r.signers, vec![id.public_hex()]);
    }

    #[test]
    fn removing_an_entry_breaks_the_chain() {
        let mut c = chain_of(4, None);
        c.0.remove(1);
        let r = c.verify();
        assert!(!r.is_intact());
        assert_eq!(
            match r.broken_at {
                Some(Error::Provenance { entry, .. }) => entry,
                other => panic!("expected a provenance error, got {other:?}"),
            },
            1
        );
    }

    #[test]
    fn editing_an_entry_invalidates_its_signature() {
        let id = Identity::generate().unwrap();
        let mut c = chain_of(2, Some(&id));
        c.0[1].action = "something else entirely".into();
        assert!(!c.verify().is_intact());
    }

    #[test]
    fn reordering_entries_is_detected() {
        let mut c = chain_of(3, None);
        c.0.swap(1, 2);
        assert!(!c.verify().is_intact());
    }

    #[test]
    fn survives_a_json_round_trip() {
        let id = Identity::generate().unwrap();
        let c = chain_of(3, Some(&id));
        let back = Chain::decode(&c.encode().unwrap()).unwrap();
        assert!(back.verify().is_fully_signed());
    }
}
