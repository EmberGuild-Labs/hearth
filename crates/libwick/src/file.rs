//! Reading and writing whole Wick files, and the cheap tiered read.
//!
//! Two access paths, deliberately:
//!
//! * [`WickFile`] loads everything, verifies the content hash, and hands back
//!   a mutable tree. This is what conversion, migration and editing use.
//! * [`Peek`] reads the header and the chunk *offsets* only. It answers "what
//!   is this file, roughly" — format, version, flags, what chunks exist, and
//!   the summary tier if there is one — without decompressing the payload.
//!   A file browser showing a thousand files uses this and never touches
//!   `DATA` at all, which is the whole point of §2.6.
//!
//! Between them sits [`WickFile::read_partial`], which reads the whole table
//! but decodes only as many of `DATA`'s children as the caller asks for. That
//! is what makes a table larger than memory viewable: the file is walked, but
//! only one row group is ever decompressed.

use crate::caps::Capabilities;
use crate::chunks::{Chunk, ChunkList, ChunkRef, ChunkType, Nested};
use crate::crypto::{Identity, KeyRing, KeySlot};
use crate::error::{Error, Result};
use crate::header::{Flags, Header, Tag, Version, HEADER_LEN, SPEC_VERSION};
use crate::hex;
use crate::plugin::Enough;
use crate::provenance::{Chain, ProvEntry};
use crate::schema::Schema;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

pub struct WickFile {
    pub header: Header,
    pub chunks: ChunkList,
    /// Slots the file declares, plus whichever of them are unlocked.
    pub keys: KeyRing,
    /// The encoded table exactly as read, kept so that unlocking a slot can
    /// re-decode without another trip to disk.
    raw_table: Vec<u8>,
    /// True when `DATA` holds only its leading children because the file was
    /// opened with [`WickFile::read_partial`].
    partial: bool,
}

impl WickFile {
    pub fn new(tag: Tag) -> Self {
        WickFile {
            header: Header::new(tag),
            chunks: ChunkList::new(),
            keys: KeyRing::empty(),
            raw_table: Vec::new(),
            partial: false,
        }
    }

    pub fn read(path: impl AsRef<Path>) -> Result<Self> {
        Self::read_opt(path, true)
    }

    /// `verify_hash: false` is for inspecting a file that is already known to
    /// be damaged. Nothing else should pass false — a content hash that is
    /// checked only when convenient is not an integrity guarantee.
    pub fn read_opt(path: impl AsRef<Path>, verify_hash: bool) -> Result<Self> {
        let bytes = std::fs::read(path.as_ref())?;
        Self::from_bytes(&bytes, verify_hash)
    }

    pub fn from_bytes(bytes: &[u8], verify_hash: bool) -> Result<Self> {
        let header = Header::decode(bytes)?;
        let start = header.table_offset as usize;
        let end = start
            .checked_add(header.table_len as usize)
            .ok_or(Error::Truncated("chunk table"))?;
        if end > bytes.len() {
            return Err(Error::Truncated("chunk table"));
        }
        let raw_table = bytes[start..end].to_vec();

        if verify_hash {
            let actual = blake3::hash(&raw_table);
            if actual.as_bytes() != &header.content_hash {
                return Err(Error::HashMismatch {
                    expected: hex::encode(&header.content_hash),
                    actual: hex::encode(actual.as_bytes()),
                });
            }
        }

        // Two passes: the key slot table has to be readable before anything
        // sealed can be, and it is always stored in plaintext.
        let bare = ChunkList::decode(&raw_table, &KeyRing::empty())?;
        let keys = match bare.get(ChunkType::KEYS) {
            Some(c) => KeyRing::with_slots(serde_json::from_slice::<Vec<KeySlot>>(&c.value)?),
            None => KeyRing::empty(),
        };

        Ok(WickFile {
            header,
            chunks: bare,
            keys,
            raw_table,
            partial: false,
        })
    }

    /// Read a file, decoding only as much of `DATA` as the caller needs.
    ///
    /// `enough` is handed the children decoded so far, in order, before each
    /// one, and says whether they already hold what the caller will use. Every
    /// other chunk — `SUMM`, `SCHM`, `PROV`, `CAPS`, `MIGR`, `KEYS` — is read
    /// in full, because all of them are small by design and a file that
    /// skipped its provenance chain to save a kilobyte would be answering a
    /// question nobody asked.
    ///
    /// Two things this does *not* save, stated because the difference is the
    /// whole point:
    ///
    /// * **It still reads every byte of the table.** The content hash covers
    ///   the whole of it, so verifying integrity means hashing all of it. The
    ///   hash is streamed in blocks rather than skipped: an integrity
    ///   guarantee that lapses whenever it is inconvenient is not one.
    /// * **It decodes the children it keeps twice** — once to ask `enough`,
    ///   once when [`WickFile::data`] is called. That prefix is by definition
    ///   the small part, and the alternative is a second way to hold a payload
    ///   in memory. A caller that answers [`Enough::All`] is asked before
    ///   anything is decoded and so pays none of it.
    ///
    /// What it saves is decompression and memory over the *rest* of `DATA`,
    /// which for a table of a million rows is all of it but one row group.
    ///
    /// The resulting file refuses to be written — see [`WickFile::to_bytes`].
    /// It also decides how much to read before any passphrase is supplied, so
    /// a payload sealed to a locked slot is read whole.
    pub fn read_partial(
        path: impl AsRef<Path>,
        enough: impl Fn(&ChunkList) -> Enough,
    ) -> Result<Self> {
        let path = path.as_ref();
        let peek = Peek::open(path)?;
        let mut f = BufReader::new(File::open(path)?);
        verify_table(&mut f, &peek.header)?;

        let Some(data) = peek.find(ChunkType::DATA) else {
            // Nothing to be partial about. Fall back rather than refuse: a
            // caller asking for less than everything is happy with less.
            return Self::read_opt(path, false);
        };
        let Nested::Addressable(kids) = ChunkList::scan_within(&mut f, data)? else {
            return Self::read_opt(path, false);
        };

        // The key slots have to be readable before a sealed child is, and
        // KEYS is always plaintext — the same two-pass order `from_bytes`
        // uses, done here against the file rather than a buffer.
        let keys = match peek.find(ChunkType::KEYS) {
            Some(r) => {
                let c = Chunk::decode_stored(r.ty, &read_at(&mut f, r)?, &KeyRing::empty())?;
                KeyRing::with_slots(serde_json::from_slice::<Vec<KeySlot>>(&c.value)?)
            }
            None => KeyRing::empty(),
        };

        let mut taken = ChunkList::new();
        let mut kept = 0usize;
        loop {
            match enough(&taken) {
                Enough::Yes => break,
                // Asked before the first child is touched, so a format with
                // no use for a prefix pays nothing to say so.
                Enough::All => return Self::read_opt(path, false),
                Enough::More => {}
            }
            let Some(k) = kids.get(kept) else { break };
            taken.push(Chunk::decode_stored(k.ty, &read_at(&mut f, *k)?, &keys)?);
            kept += 1;
        }

        // Reassemble a well-formed table from the chunks actually read, then
        // decode it exactly as a whole-file read would. Doing it in bytes
        // rather than in decoded chunks keeps one decoder, and keeps
        // `raw_table` honest for a later `unlock`.
        let end = match kept.checked_sub(1).and_then(|i| kids.get(i)) {
            Some(last) => last.at + last.stored,
            // `kept` is zero only when nothing was asked for, or DATA has no
            // children at all.
            None => data.at + 2,
        };
        let child_bytes = read_range(&mut f, data.at + 2, end)?;
        let mut raw_table = Vec::with_capacity(peek.header.table_len as usize);
        for r in &peek.chunks {
            raw_table.extend_from_slice(&r.ty.0);
            // Matched by offset, not by type: `DATA` is single-occurrence by
            // spec, and a file that broke that rule should come out of a
            // partial read damaged in a way the hash catches, not silently
            // rewritten with one payload copied over another.
            if r.at == data.at {
                raw_table.extend_from_slice(&((child_bytes.len() + 2) as u64).to_le_bytes());
                raw_table.extend_from_slice(&[crate::chunks::CODEC_RAW, 0]);
                raw_table.extend_from_slice(&child_bytes);
            } else {
                raw_table.extend_from_slice(&r.stored.to_le_bytes());
                raw_table.extend_from_slice(&read_at(&mut f, *r)?);
            }
        }

        Ok(WickFile {
            header: peek.header,
            chunks: ChunkList::decode(&raw_table, &keys)?,
            keys,
            raw_table,
            partial: kept < kids.len(),
        })
    }

    /// True when this file holds only the leading part of its payload.
    ///
    /// A count taken from a partial file describes the read, not the file,
    /// and anything that reports one has to say which it means.
    pub fn is_partial(&self) -> bool {
        self.partial
    }

    /// Supply a passphrase for one slot and re-read the sealed chunks it
    /// covers. Chunks belonging to other slots stay locked and are carried
    /// through untouched on the next write.
    pub fn unlock(&mut self, slot: u8, passphrase: &str) -> Result<()> {
        self.keys.unlock(slot, passphrase)?;
        self.chunks = ChunkList::decode(&self.raw_table, &self.keys)?;
        Ok(())
    }

    pub fn locked_slots(&self) -> Vec<u8> {
        let mut v: Vec<u8> = self
            .chunks
            .iter()
            .filter(|c| c.is_locked())
            .map(|c| c.enc.slot)
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    // ---- reserved chunk accessors ---------------------------------------

    pub fn data(&self) -> Result<ChunkList> {
        self.chunks
            .require(ChunkType::DATA, "DATA")?
            .as_list(&self.keys)
    }

    pub fn set_data(&mut self, list: &ChunkList) -> Result<()> {
        let c = Chunk::list(ChunkType::DATA, list, &self.keys)?;
        self.chunks.set(c);
        Ok(())
    }

    pub fn summary(&self) -> Result<Option<ChunkList>> {
        match self.chunks.get(ChunkType::SUMM) {
            Some(c) if c.is_locked() => Ok(None),
            Some(c) => Ok(Some(c.as_list(&self.keys)?)),
            None => Ok(None),
        }
    }

    /// The slot a reserved chunk is sealed to, when it is present but locked.
    ///
    /// A sealed schema or summary reads as absent, which is the only thing a
    /// reader without the passphrase can do with it — but "absent" and
    /// "sealed" are different facts and a tool that conflates them cannot
    /// tell somebody what passphrase would help.
    pub fn sealed_slot(&self, ty: ChunkType) -> Option<u8> {
        self.chunks
            .get(ty)
            .filter(|c| c.is_locked())
            .map(|c| c.enc.slot)
    }

    pub fn set_summary(&mut self, list: &ChunkList) -> Result<()> {
        let c = Chunk::list(ChunkType::SUMM, list, &self.keys)?;
        self.chunks.set(c);
        Ok(())
    }

    pub fn schema(&self) -> Result<Option<Schema>> {
        self.chunks
            .get(ChunkType::SCHM)
            .filter(|c| !c.is_locked())
            .map(Schema::decode)
            .transpose()
    }

    pub fn set_schema(&mut self, s: &Schema) -> Result<()> {
        self.chunks.set(s.encode()?);
        Ok(())
    }

    pub fn caps(&self) -> Result<Option<Capabilities>> {
        self.chunks
            .get(ChunkType::CAPS)
            .map(Capabilities::decode)
            .transpose()
    }

    pub fn set_caps(&mut self, c: &Capabilities) -> Result<()> {
        self.chunks.set(c.encode()?);
        Ok(())
    }

    pub fn chain(&self) -> Result<Chain> {
        match self.chunks.get(ChunkType::PROV) {
            Some(c) => Chain::decode(c),
            None => Ok(Chain::new()),
        }
    }

    /// Append a provenance entry, signing it if an identity is available.
    /// Every mutation Hearth makes goes through here; that is what keeps the
    /// chain complete rather than best-effort.
    pub fn record(&mut self, tool: &str, action: &str, id: Option<&Identity>) -> Result<()> {
        let mut chain = self.chain()?;
        chain.append(ProvEntry::new(tool, action), id);
        self.chunks.set(chain.encode()?);
        Ok(())
    }

    pub fn migrations(&self) -> Result<Option<crate::migrate::RuleSet>> {
        match self.chunks.get(ChunkType::MIGR) {
            Some(c) => Ok(Some(serde_json::from_slice(&c.value)?)),
            None => Ok(None),
        }
    }

    pub fn set_migrations(&mut self, rules: &crate::migrate::RuleSet) -> Result<()> {
        self.chunks
            .set(Chunk::new(ChunkType::MIGR, serde_json::to_vec(rules)?));
        Ok(())
    }

    /// Declare a new encryption slot and unlock it for writing.
    pub fn add_key_slot(&mut self, slot: u8, label: &str, passphrase: &str) -> Result<()> {
        self.keys.add_slot(slot, label, passphrase)?;
        let c = Chunk::new(ChunkType::KEYS, serde_json::to_vec(self.keys.slots())?);
        self.chunks.set(c);
        Ok(())
    }

    /// Forget a slot no chunk is sealed to any more, and drop `KEYS` with it
    /// when it was the last one.
    ///
    /// A file that declares a key slot nothing uses invites the obvious wrong
    /// conclusion — that something in it is still encrypted.
    pub fn remove_key_slot(&mut self, slot: u8) -> Result<()> {
        if self.chunks.iter().any(|c| c.enc.slot == slot) {
            return Err(Error::Other(format!(
                "slot {slot} still has chunks sealed to it"
            )));
        }
        self.keys.remove_slot(slot);
        if self.keys.slots().is_empty() {
            self.chunks.remove(ChunkType::KEYS);
        } else {
            let c = Chunk::new(ChunkType::KEYS, serde_json::to_vec(self.keys.slots())?);
            self.chunks.set(c);
        }
        Ok(())
    }

    /// Seal everything in the file that is *content* to `slot`, and report
    /// what was sealed.
    ///
    /// Two chunks are deliberately left in the clear, and which two is the
    /// whole design of this operation:
    ///
    /// * **`KEYS`** must stay plaintext. It holds the salt a passphrase is
    ///   stretched against, so encrypting it would shut the key inside the
    ///   lock it opens. §3 of the spec says the same thing.
    /// * **`PROV`** stays readable so the chain can still be verified, and so
    ///   a later write can extend it rather than starting a new one. The cost
    ///   is that timestamps, actions and signing keys remain visible, which a
    ///   caller is expected to say out loud rather than let a user assume
    ///   otherwise.
    ///
    /// The header and the chunk table are structure, not content, and are
    /// never encrypted: the format tag and the size of each chunk are visible
    /// on any Wick file and always will be.
    pub fn seal_payload(&mut self, slot: u8) -> Vec<ChunkType> {
        self.reseal(slot, |c| {
            c.enc.slot == 0 && !matches!(c.ty, ChunkType::KEYS | ChunkType::PROV)
        })
    }

    /// The reverse: bring every chunk sealed to `slot` back into the clear.
    ///
    /// The caller must have unlocked `slot` first, or the values still hold
    /// ciphertext and this would write it out as though it were plaintext.
    pub fn unseal_payload(&mut self, slot: u8) -> Result<Vec<ChunkType>> {
        if !self.keys.is_unlocked(slot) {
            return Err(Error::NeedKey {
                slot,
                label: self.keys.label(slot),
            });
        }
        Ok(self.reseal(0, |c| c.enc.slot == slot))
    }

    fn reseal(&mut self, to: u8, want: impl Fn(&Chunk) -> bool) -> Vec<ChunkType> {
        let moving: Vec<Chunk> = self.chunks.iter().filter(|c| want(c)).cloned().collect();
        let mut done = Vec::new();
        for c in moving {
            done.push(c.ty);
            self.chunks.set(c.sealed_to(to));
        }
        done
    }

    // ---- writing ---------------------------------------------------------

    /// Recompute the header's flags from what is actually present, so the
    /// flags can never disagree with the chunk table.
    fn sync_flags(&mut self) {
        let mut f = Flags::default();
        f.set(
            Flags::PROVENANCE,
            self.chunks.get(ChunkType::PROV).is_some(),
        );
        f.set(
            Flags::CAPABILITIES,
            self.chunks.get(ChunkType::CAPS).is_some(),
        );
        f.set(Flags::SUMMARY, self.chunks.get(ChunkType::SUMM).is_some());

        let sealed: Vec<u8> = self
            .chunks
            .iter()
            .filter(|c| c.enc.slot != 0)
            .map(|c| c.enc.slot)
            .collect();
        f.set(Flags::ENCRYPTED, !sealed.is_empty());
        f.set(
            Flags::SPLIT_TRUST,
            sealed.iter().any(|s| Some(s) != sealed.first()),
        );
        self.header.flags = f;
    }

    pub fn to_bytes(&mut self) -> Result<Vec<u8>> {
        // A partial read holds the first row group of a million-row table and
        // is indistinguishable, once decoded, from a table that has one row
        // group. Writing it would produce a valid file, correctly hashed,
        // with the rest of the payload silently gone.
        if self.partial {
            return Err(Error::Other(
                "this file was read partially and does not hold all of its payload; \
                 writing it would drop the rest"
                    .into(),
            ));
        }
        self.sync_flags();
        let table = self.chunks.encode(&self.keys)?;
        self.header.table_offset = HEADER_LEN as u64;
        self.header.table_len = table.len() as u64;
        self.header.content_hash = *blake3::hash(&table).as_bytes();

        let mut out = Vec::with_capacity(HEADER_LEN + table.len());
        out.extend_from_slice(&self.header.encode());
        out.extend_from_slice(&table);
        Ok(out)
    }

    /// Write via a temporary file in the same directory and rename over the
    /// target. A half-written Wick file is unrecoverable — the content hash
    /// covers the whole table — so the write has to be all or nothing.
    pub fn write(&mut self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let bytes = self.to_bytes()?;
        let tmp = path.with_extension(format!(
            "{}.tmp{}",
            path.extension().and_then(|e| e.to_str()).unwrap_or("wick"),
            std::process::id()
        ));
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
        Ok(())
    }
}

/// Read `n` bytes at an absolute offset.
fn read_range<R: Read + Seek>(r: &mut R, from: u64, to: u64) -> Result<Vec<u8>> {
    r.seek(SeekFrom::Start(from))?;
    let mut buf = vec![0u8; (to - from) as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

fn read_at<R: Read + Seek>(r: &mut R, at: ChunkRef) -> Result<Vec<u8>> {
    read_range(r, at.at, at.at + at.stored)
}

/// Hash the chunk table in blocks and check it against the header.
///
/// The whole-file path can hash the buffer it already holds; a partial read
/// has no such buffer and must not grow one, so it streams. 64 KiB is large
/// enough that the syscall overhead disappears and small enough that peak
/// memory stays a constant regardless of how big the table is.
fn verify_table<R: Read + Seek>(r: &mut R, header: &Header) -> Result<()> {
    r.seek(SeekFrom::Start(header.table_offset))?;
    let mut hasher = blake3::Hasher::new();
    let mut left = header.table_len;
    let mut buf = vec![0u8; 64 * 1024];
    while left > 0 {
        let n = (left as usize).min(buf.len());
        r.read_exact(&mut buf[..n])
            .map_err(|_| Error::Truncated("chunk table"))?;
        hasher.update(&buf[..n]);
        left -= n as u64;
    }
    let actual = hasher.finalize();
    if actual.as_bytes() != &header.content_hash {
        return Err(Error::HashMismatch {
            expected: hex::encode(&header.content_hash),
            actual: hex::encode(actual.as_bytes()),
        });
    }
    Ok(())
}

/// Everything the header and chunk offsets can tell you, for the price of a
/// handful of seeks.
#[derive(Debug)]
pub struct Peek {
    pub header: Header,
    /// Where each top-level chunk lies, with nothing decoded.
    pub chunks: Vec<ChunkRef>,
    pub file_len: u64,
}

impl Peek {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let f = File::open(path.as_ref())?;
        let file_len = f.metadata()?.len();
        let mut r = BufReader::new(f);
        let mut head = [0u8; HEADER_LEN];
        // Read what is there rather than insisting on a full header: a file
        // shorter than 60 bytes is almost always something that is not a
        // Wick file at all, and "not a Wick file" is a far more useful
        // answer than "truncated header".
        let mut got = 0;
        while got < HEADER_LEN {
            match r.read(&mut head[got..])? {
                0 => break,
                n => got += n,
            }
        }
        if got < 4 || head[..4] != crate::header::MAGIC {
            return Err(Error::NotWick);
        }
        if got < HEADER_LEN {
            return Err(Error::Truncated("header"));
        }
        let header = Header::decode(&head)?;
        let chunks = ChunkList::scan(&mut r, header.table_offset, header.table_len)?;
        Ok(Peek {
            header,
            chunks,
            file_len,
        })
    }

    pub fn tag(&self) -> Tag {
        self.header.tag
    }

    pub fn version(&self) -> Version {
        self.header.version
    }

    pub fn has(&self, ty: ChunkType) -> bool {
        self.chunks.iter().any(|c| c.ty == ty)
    }

    pub fn find(&self, ty: ChunkType) -> Option<ChunkRef> {
        self.chunks.iter().copied().find(|c| c.ty == ty)
    }

    /// Check the content hash without holding the table in memory.
    ///
    /// A whole-file read hashes the buffer it already has. This streams
    /// instead, so a tool that reports on a gigabyte file can still say
    /// whether it is intact without becoming a gigabyte allocation.
    pub fn verify(&self, path: impl AsRef<Path>) -> Result<()> {
        let mut r = BufReader::new(File::open(path.as_ref())?);
        verify_table(&mut r, &self.header)
    }

    /// Where one chunk's children lie, for the price of one seek per child
    /// and no decoding at all.
    ///
    /// This is the index a partial read works from, and the honest answer to
    /// "how big is a row group" — `stored` is what it costs to read one.
    pub fn children(&self, path: impl AsRef<Path>, ty: ChunkType) -> Result<Option<Nested>> {
        let Some(parent) = self.find(ty) else {
            return Ok(None);
        };
        let mut r = BufReader::new(File::open(path.as_ref())?);
        Ok(Some(ChunkList::scan_within(&mut r, parent)?))
    }

    /// True when this file predates the running build and should be migrated.
    pub fn is_outdated(&self) -> bool {
        self.header.version < SPEC_VERSION
    }

    /// Read one chunk, seeking past everything else. This is the operation
    /// that makes the summary tier worth having.
    pub fn read_chunk(
        &self,
        path: impl AsRef<Path>,
        ty: ChunkType,
        keys: &KeyRing,
    ) -> Result<Option<Chunk>> {
        let Some(r) = self.find(ty) else {
            return Ok(None);
        };
        let mut f = File::open(path.as_ref())?;
        let stored = read_at(&mut f, r)?;
        Ok(Some(Chunk::decode_stored(ty, &stored, keys)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("libwick-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Incompressible bytes. A repeated byte would zstd down to nothing and
    /// make any size comparison against it meaningless.
    fn noise(n: usize) -> Vec<u8> {
        let mut x: u64 = 0x2545F491_4F6CDD1D;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect()
    }

    fn sample() -> WickFile {
        let mut f = WickFile::new(Tag::new(b"MT"));
        let mut data = ChunkList::new();
        data.push(Chunk::text(ChunkType::new(b"SECT"), "hello world"));
        f.set_data(&data).unwrap();
        f.record("libwick test", "created", None).unwrap();
        f
    }

    #[test]
    fn write_then_read_round_trips() {
        let p = scratch("roundtrip").join("a.emt");
        sample().write(&p).unwrap();

        let back = WickFile::read(&p).unwrap();
        assert_eq!(back.header.tag.as_str(), "MT");
        assert_eq!(back.header.version, SPEC_VERSION);
        assert!(back.header.flags.has(Flags::PROVENANCE));
        assert_eq!(back.data().unwrap().0[0].as_str().unwrap(), "hello world");
        assert!(back.chain().unwrap().verify().is_intact());
    }

    #[test]
    fn a_flipped_byte_is_caught_by_the_content_hash() {
        let p = scratch("corrupt").join("a.emt");
        sample().write(&p).unwrap();

        let mut bytes = std::fs::read(&p).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        std::fs::write(&p, &bytes).unwrap();

        assert!(matches!(
            WickFile::read(&p),
            Err(Error::HashMismatch { .. })
        ));
    }

    #[test]
    fn peek_reads_the_summary_without_the_payload() {
        let p = scratch("peek").join("a.emt");
        let mut f = sample();
        let mut summ = ChunkList::new();
        summ.push(Chunk::text(ChunkType::new(b"TEXT"), "a greeting"));
        f.set_summary(&summ).unwrap();
        let mut big = ChunkList::new();
        big.push(Chunk::stored(ChunkType::new(b"SECT"), noise(200_000)));
        f.set_data(&big).unwrap();
        f.write(&p).unwrap();

        let peek = Peek::open(&p).unwrap();
        assert_eq!(peek.tag().as_str(), "MT");
        assert!(peek.has(ChunkType::SUMM));
        assert!(peek.has(ChunkType::DATA));

        let keys = KeyRing::empty();
        let summ = peek
            .read_chunk(&p, ChunkType::SUMM, &keys)
            .unwrap()
            .unwrap();
        let data_len = peek.find(ChunkType::DATA).unwrap().stored;
        // The summary read is orders of magnitude smaller than the payload
        // it stands in for; that is the entire justification for the tier.
        assert!(summ.value.len() * 100 < data_len as usize);
    }

    fn yes_if(done: bool) -> Enough {
        if done {
            Enough::Yes
        } else {
            Enough::More
        }
    }

    /// A payload shaped like a real one: many children, each big enough to
    /// be kept independently decodable.
    fn grouped(n: usize) -> WickFile {
        let mut f = WickFile::new(Tag::new(b"MX"));
        let mut data = ChunkList::new();
        data.push(Chunk::text(ChunkType::new(b"COLS"), "a,b,c"));
        for i in 0..n {
            let mut v = noise(8_000);
            v[0] = i as u8;
            data.push(Chunk::stored(ChunkType::new(b"RGRP"), v));
        }
        f.set_data(&data).unwrap();
        f
    }

    #[test]
    fn a_partial_read_decodes_the_prefix_and_leaves_the_rest_on_disk() {
        let p = scratch("partial").join("t.emx");
        grouped(40).write(&p).unwrap();

        // Stop as soon as two row groups are in hand.
        let f = WickFile::read_partial(&p, |taken| {
            yes_if(taken.all(ChunkType::new(b"RGRP")).count() >= 2)
        })
        .unwrap();

        assert!(f.is_partial());
        let data = f.data().unwrap();
        assert_eq!(data.all(ChunkType::new(b"RGRP")).count(), 2);
        // COLS came first and is still there — a partial read of a table
        // without its column names would be unreadable.
        assert_eq!(data.get(ChunkType::new(b"COLS")).unwrap().value, b"a,b,c");
        // And the groups it did read are the right ones, in order.
        assert_eq!(data.0[1].value[0], 0);
        assert_eq!(data.0[2].value[0], 1);
    }

    #[test]
    fn a_reader_that_needs_no_payload_decodes_none_of_it() {
        // What a summary render asks for: the file's other chunks, and not
        // one row group.
        let p = scratch("partial-none").join("t.emx");
        let mut src = grouped(12);
        let mut summ = ChunkList::new();
        summ.push(Chunk::text(ChunkType::new(b"STAT"), "12 groups"));
        src.set_summary(&summ).unwrap();
        src.write(&p).unwrap();

        let f = WickFile::read_partial(&p, |_| Enough::Yes).unwrap();
        assert!(f.is_partial());
        assert!(f.data().unwrap().is_empty());
        // The tier it was opened for is intact, as is the provenance chain.
        assert_eq!(
            f.summary().unwrap().unwrap().0[0].as_str().unwrap(),
            "12 groups"
        );
        assert!(f.chain().unwrap().verify().is_intact());
    }

    #[test]
    fn a_format_that_needs_the_whole_payload_is_never_asked_twice() {
        let p = scratch("partial-whole").join("t.emx");
        grouped(8).write(&p).unwrap();

        // `Enough::All` is the default answer, and the point of it is that
        // asking costs nothing: the spine gives up before decoding anything.
        let asked = std::cell::Cell::new(0usize);
        let f = WickFile::read_partial(&p, |_| {
            asked.set(asked.get() + 1);
            Enough::All
        })
        .unwrap();

        assert_eq!(asked.get(), 1, "asked more than once");
        assert!(!f.is_partial());
        assert_eq!(f.data().unwrap().all(ChunkType::new(b"RGRP")).count(), 8);
    }

    #[test]
    fn a_partial_read_that_wants_everything_is_not_partial() {
        let p = scratch("partial-all").join("t.emx");
        grouped(3).write(&p).unwrap();

        let f = WickFile::read_partial(&p, |_| Enough::More).unwrap();
        assert!(!f.is_partial());
        assert_eq!(f.data().unwrap().all(ChunkType::new(b"RGRP")).count(), 3);
        assert_eq!(
            f.data().unwrap(),
            WickFile::read(&p).unwrap().data().unwrap()
        );
    }

    #[test]
    fn a_partial_read_still_verifies_the_content_hash() {
        let p = scratch("partial-hash").join("t.emx");
        grouped(20).write(&p).unwrap();

        // Damage a byte in the *last* row group — the one a partial read
        // never decodes. It must still be caught: the hash covers the table,
        // not the part of it somebody happened to want.
        let mut bytes = std::fs::read(&p).unwrap();
        let last = bytes.len() - 20;
        bytes[last] ^= 0xFF;
        std::fs::write(&p, &bytes).unwrap();

        assert!(matches!(
            WickFile::read_partial(&p, |taken| yes_if(taken.len() >= 2)),
            Err(Error::HashMismatch { .. })
        ));
    }

    #[test]
    fn a_partial_file_refuses_to_be_written() {
        let p = scratch("partial-write").join("t.emx");
        grouped(10).write(&p).unwrap();

        let mut f = WickFile::read_partial(&p, |taken| yes_if(taken.len() >= 2)).unwrap();
        let err = f
            .write(scratch("partial-write").join("out.emx"))
            .unwrap_err();
        assert!(
            err.to_string().contains("drop the rest"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_payload_that_cannot_be_seeked_into_is_read_whole() {
        // Many small children: `Chunk::list` compresses the lot as one
        // stream, so there is nothing to seek to and nothing to save.
        let p = scratch("partial-small").join("c.emc");
        let mut f = WickFile::new(Tag::new(b"MC"));
        let mut data = ChunkList::new();
        for i in 0..50 {
            data.push(Chunk::text(ChunkType::new(b"NODE"), &format!("k{i}=v{i}")));
        }
        f.set_data(&data).unwrap();
        f.write(&p).unwrap();

        let back = WickFile::read_partial(&p, |taken| yes_if(!taken.is_empty())).unwrap();
        assert!(!back.is_partial());
        assert_eq!(back.data().unwrap().len(), 50);
    }

    #[test]
    fn the_child_index_reports_sizes_without_decoding_them() {
        let p = scratch("index").join("t.emx");
        grouped(6).write(&p).unwrap();

        let peek = Peek::open(&p).unwrap();
        let Some(Nested::Addressable(kids)) = peek.children(&p, ChunkType::DATA).unwrap() else {
            panic!("row groups should be individually addressable");
        };
        assert_eq!(kids.len(), 7); // COLS + 6 groups
        assert_eq!(kids[0].ty, ChunkType::new(b"COLS"));
        assert!(kids[1..].iter().all(|k| k.ty == ChunkType::new(b"RGRP")));
        // Offsets are absolute and in order, each starting where the last ends.
        for w in kids.windows(2) {
            assert_eq!(w[0].at + w[0].stored + 12, w[1].at);
        }
        assert!(peek.children(&p, ChunkType::MIGR).unwrap().is_none());
    }

    #[test]
    fn split_trust_keeps_other_slots_readable_and_intact() {
        let p = scratch("split").join("a.emc");
        let mut f = WickFile::new(Tag::new(b"MC"));
        f.add_key_slot(1, "prod", "correct horse").unwrap();
        f.add_key_slot(2, "staging", "battery staple").unwrap();

        let mut public = ChunkList::new();
        public.push(Chunk::text(ChunkType::new(b"NODE"), "port=8080"));
        f.set_data(&public).unwrap();
        f.chunks
            .push(Chunk::text(ChunkType::new(b"SECR"), "prod_token=hunter2").sealed_to(1));
        f.chunks
            .push(Chunk::text(ChunkType::new(b"SECR"), "stg_token=letmein").sealed_to(2));
        f.write(&p).unwrap();

        // No passphrase: public data readable, both secrets opaque.
        let mut back = WickFile::read(&p).unwrap();
        assert!(back.header.flags.has(Flags::ENCRYPTED));
        assert!(back.header.flags.has(Flags::SPLIT_TRUST));
        assert_eq!(back.data().unwrap().0[0].as_str().unwrap(), "port=8080");
        assert_eq!(back.locked_slots(), vec![1, 2]);

        // One passphrase: exactly one secret opens.
        back.unlock(1, "correct horse").unwrap();
        assert_eq!(back.locked_slots(), vec![2]);
        let opened: Vec<_> = back
            .chunks
            .all(ChunkType::new(b"SECR"))
            .filter(|c| !c.is_locked())
            .map(|c| c.as_str().unwrap().to_string())
            .collect();
        assert_eq!(opened, vec!["prod_token=hunter2"]);

        // Rewriting while a slot is locked must not damage it.
        let out = scratch("split").join("b.emc");
        back.write(&out).unwrap();
        let mut re = WickFile::read(&out).unwrap();
        re.unlock(2, "battery staple").unwrap();
        assert!(re
            .chunks
            .all(ChunkType::new(b"SECR"))
            .any(|c| !c.is_locked() && c.as_str().unwrap() == "stg_token=letmein"));
    }

    #[test]
    fn a_wrong_passphrase_fails_loudly() {
        let p = scratch("wrongpass").join("a.emc");
        let mut f = WickFile::new(Tag::new(b"MC"));
        f.add_key_slot(1, "prod", "right").unwrap();
        f.chunks
            .push(Chunk::text(ChunkType::new(b"SECR"), "token=x").sealed_to(1));
        f.write(&p).unwrap();

        let mut back = WickFile::read(&p).unwrap();
        assert!(matches!(
            back.unlock(1, "wrong"),
            Err(Error::Decrypt { slot: 1 })
        ));
    }
}
