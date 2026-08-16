//! Split-trust encryption and the identity keys provenance is signed with.
//!
//! Two unrelated pieces of cryptography live here because they share a home
//! in the container, not because they share a mechanism:
//!
//! * **Key slots** encrypt individual chunks. A file can hold plaintext
//!   config and two sets of environment secrets, each sealed to a different
//!   passphrase, and a reader holding one passphrase reads exactly its own
//!   slot and passes the rest through untouched.
//! * **Identity keys** are Ed25519 pairs used to sign provenance entries.
//!   They never encrypt anything.
//!
//! Chunk encryption is XChaCha20-Poly1305 with a 24-byte random nonce, keyed
//! by Argon2id over a passphrase and a per-slot salt. The nonce is large
//! enough that random generation is safe without tracking a counter, which
//! matters for a file format where the writer has no memory of prior writes.

use crate::error::{Error, Result};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use std::collections::BTreeMap;

pub const NONCE_LEN: usize = 24;
pub const KEY_LEN: usize = 32;
pub const SALT_LEN: usize = 16;

/// Argon2id parameters. 64 MiB and three passes is the OWASP-recommended
/// floor at time of writing; it costs about a tenth of a second per unlock,
/// which is invisible next to opening a file and expensive to do a billion
/// times.
const ARGON_MEM_KIB: u32 = 65_536;
const ARGON_PASSES: u32 = 3;
const ARGON_LANES: u32 = 1;

pub fn random_bytes(n: usize) -> Result<Vec<u8>> {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).map_err(|e| Error::Other(format!("no system randomness: {e}")))?;
    Ok(b)
}

/// Derive a slot key from a passphrase and that slot's stored salt.
pub fn derive_key(passphrase: &str, salt: &[u8]) -> Result<[u8; KEY_LEN]> {
    let params = argon2::Params::new(ARGON_MEM_KIB, ARGON_PASSES, ARGON_LANES, Some(KEY_LEN))
        .map_err(|e| Error::Other(format!("argon2 params: {e}")))?;
    let a = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut out = [0u8; KEY_LEN];
    a.hash_password_into(passphrase.as_bytes(), salt, &mut out)
        .map_err(|e| Error::Other(format!("argon2: {e}")))?;
    Ok(out)
}

pub fn seal(
    key: &[u8; KEY_LEN],
    aad: &[u8],
    plaintext: &[u8],
) -> Result<([u8; NONCE_LEN], Vec<u8>)> {
    let cipher = XChaCha20Poly1305::new(&Key::from(*key));
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|e| Error::Other(format!("no system randomness: {e}")))?;
    let ct = cipher
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| Error::Other("encryption failed".into()))?;
    Ok((nonce, ct))
}

pub fn open(
    key: &[u8; KEY_LEN],
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>> {
    let cipher = XChaCha20Poly1305::new(&Key::from(*key));
    cipher
        .decrypt(
            &XNonce::from(*nonce),
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| Error::Other("authentication failed".into()))
}

/// One entry of the `KEYS` chunk: everything a reader needs to know a slot
/// exists, what it is for, and how to derive its key — but nothing that
/// helps derive it without the passphrase.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct KeySlot {
    pub slot: u8,
    /// What this slot is for: "prod", "staging", "personal notes".
    pub label: String,
    /// Hex Argon2id salt, unique per slot.
    pub salt: String,
    #[serde(default = "default_kdf")]
    pub kdf: String,
    #[serde(default = "default_alg")]
    pub alg: String,
}

fn default_kdf() -> String {
    "argon2id".into()
}

fn default_alg() -> String {
    "xchacha20poly1305".into()
}

/// The keys a reader or writer currently holds, plus the slot table it read
/// from the file. Missing keys are normal: reading the public half of a
/// split-trust file is the common case, not an error.
#[derive(Clone, Default)]
pub struct KeyRing {
    slots: Vec<KeySlot>,
    keys: BTreeMap<u8, [u8; KEY_LEN]>,
}

impl KeyRing {
    pub fn empty() -> Self {
        KeyRing::default()
    }

    /// Load the slot table a file declared, without unlocking anything.
    pub fn with_slots(slots: Vec<KeySlot>) -> Self {
        KeyRing {
            slots,
            keys: BTreeMap::new(),
        }
    }

    pub fn slots(&self) -> &[KeySlot] {
        &self.slots
    }

    pub fn slot(&self, n: u8) -> Option<&KeySlot> {
        self.slots.iter().find(|s| s.slot == n)
    }

    pub fn label(&self, n: u8) -> String {
        self.slot(n)
            .map(|s| s.label.clone())
            .unwrap_or_else(|| "unnamed".into())
    }

    pub fn is_unlocked(&self, n: u8) -> bool {
        self.keys.contains_key(&n)
    }

    /// Declare a new slot and unlock it in one step, for a writer.
    pub fn add_slot(&mut self, slot: u8, label: &str, passphrase: &str) -> Result<()> {
        let salt = random_bytes(SALT_LEN)?;
        let key = derive_key(passphrase, &salt)?;
        self.slots.push(KeySlot {
            slot,
            label: label.to_string(),
            salt: crate::hex::encode(&salt),
            kdf: default_kdf(),
            alg: default_alg(),
        });
        self.keys.insert(slot, key);
        Ok(())
    }

    /// Drop a slot and any key held for it. The caller is responsible for
    /// having moved whatever was sealed to it somewhere else first.
    pub fn remove_slot(&mut self, slot: u8) {
        self.slots.retain(|s| s.slot != slot);
        self.keys.remove(&slot);
    }

    /// Unlock a slot the file already declares.
    pub fn unlock(&mut self, slot: u8, passphrase: &str) -> Result<()> {
        let s = self
            .slot(slot)
            .ok_or_else(|| Error::Other(format!("file declares no key slot {slot}")))?;
        if s.kdf != "argon2id" {
            return Err(Error::Other(format!(
                "slot {slot} uses unknown KDF '{}'",
                s.kdf
            )));
        }
        let salt = crate::hex::decode(&s.salt)
            .ok_or_else(|| Error::Other(format!("slot {slot} has a malformed salt")))?;
        let key = derive_key(passphrase, &salt)?;
        self.keys.insert(slot, key);
        Ok(())
    }

    /// Unlock whichever slot the passphrase happens to fit. A wrong
    /// passphrase cannot be detected here — it produces a key that simply
    /// fails to authenticate later — so this is only a convenience for the
    /// common single-slot case.
    pub fn unlock_by_label(&mut self, label: &str, passphrase: &str) -> Result<u8> {
        let slot = self
            .slots
            .iter()
            .find(|s| s.label == label)
            .map(|s| s.slot)
            .ok_or_else(|| Error::Other(format!("no key slot labelled '{label}'")))?;
        self.unlock(slot, passphrase)?;
        Ok(slot)
    }

    pub fn try_key(&self, slot: u8) -> Option<&[u8; KEY_LEN]> {
        self.keys.get(&slot)
    }

    pub fn key(&self, slot: u8) -> Result<&[u8; KEY_LEN]> {
        self.keys.get(&slot).ok_or_else(|| Error::NeedKey {
            slot,
            label: self.label(slot),
        })
    }
}

/// An Ed25519 identity used to sign provenance entries.
pub struct Identity {
    signing: ed25519_dalek::SigningKey,
}

impl Identity {
    pub fn generate() -> Result<Self> {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed)
            .map_err(|e| Error::Other(format!("no system randomness: {e}")))?;
        Ok(Identity {
            signing: ed25519_dalek::SigningKey::from_bytes(&seed),
        })
    }

    pub fn from_secret_hex(s: &str) -> Result<Self> {
        let b = crate::hex::decode(s.trim())
            .ok_or_else(|| Error::Other("secret key is not hex".into()))?;
        let seed: [u8; 32] = b
            .as_slice()
            .try_into()
            .map_err(|_| Error::Other("secret key must be 32 bytes".into()))?;
        Ok(Identity {
            signing: ed25519_dalek::SigningKey::from_bytes(&seed),
        })
    }

    pub fn secret_hex(&self) -> String {
        crate::hex::encode(&self.signing.to_bytes())
    }

    pub fn public_hex(&self) -> String {
        crate::hex::encode(&self.signing.verifying_key().to_bytes())
    }

    pub fn sign(&self, message: &[u8]) -> String {
        use ed25519_dalek::Signer;
        crate::hex::encode(&self.signing.sign(message).to_bytes())
    }
}

/// Verify a detached signature against a hex public key. Returns a reason on
/// failure rather than a bool, because every caller wants to report why.
pub fn verify(
    public_hex: &str,
    signature_hex: &str,
    message: &[u8],
) -> std::result::Result<(), String> {
    use ed25519_dalek::Verifier;
    let pk = crate::hex::decode(public_hex).ok_or("public key is not hex")?;
    let pk: [u8; 32] = pk
        .as_slice()
        .try_into()
        .map_err(|_| "public key is not 32 bytes")?;
    let vk = ed25519_dalek::VerifyingKey::from_bytes(&pk)
        .map_err(|_| "public key is not a valid point")?;

    let sig = crate::hex::decode(signature_hex).ok_or("signature is not hex")?;
    let sig: [u8; 64] = sig
        .as_slice()
        .try_into()
        .map_err(|_| "signature is not 64 bytes")?;
    vk.verify(message, &ed25519_dalek::Signature::from_bytes(&sig))
        .map_err(|_| "signature does not match this key and content".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_and_open_round_trip() {
        let key = [3u8; KEY_LEN];
        let (nonce, ct) = seal(&key, b"DATA\x01", b"secret payload").unwrap();
        let pt = open(&key, &nonce, b"DATA\x01", &ct).unwrap();
        assert_eq!(pt, b"secret payload");
    }

    #[test]
    fn associated_data_binds_the_chunk_to_its_slot() {
        let key = [3u8; KEY_LEN];
        let (nonce, ct) = seal(&key, b"DATA\x01", b"secret").unwrap();
        // Same key, same ciphertext, different chunk identity: must fail.
        assert!(open(&key, &nonce, b"DATA\x02", &ct).is_err());
    }

    #[test]
    fn tampering_is_detected() {
        let key = [9u8; KEY_LEN];
        let (nonce, mut ct) = seal(&key, b"", b"hello").unwrap();
        ct[0] ^= 0x01;
        assert!(open(&key, &nonce, b"", &ct).is_err());
    }

    #[test]
    fn signatures_verify_and_reject() {
        let id = Identity::generate().unwrap();
        let sig = id.sign(b"provenance entry");
        assert!(verify(&id.public_hex(), &sig, b"provenance entry").is_ok());
        assert!(verify(&id.public_hex(), &sig, b"different content").is_err());

        let other = Identity::generate().unwrap();
        assert!(verify(&other.public_hex(), &sig, b"provenance entry").is_err());
    }

    #[test]
    fn an_identity_survives_a_hex_round_trip() {
        let id = Identity::generate().unwrap();
        let back = Identity::from_secret_hex(&id.secret_hex()).unwrap();
        assert_eq!(id.public_hex(), back.public_hex());
    }
}
