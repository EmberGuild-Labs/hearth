//! The chunk table: a flat, recursive list of TLV records.
//!
//! Everything after the header is a sequence of
//!
//! ```text
//! [type: 4 bytes ASCII] [length: u64 little-endian] [value: length bytes]
//! ```
//!
//! Because the length precedes the value, a reader can walk the whole table
//! with 12-byte reads and seeks, never touching a value it does not want.
//! That is what makes the summary tier (§2.6 of the spec) actually cheap: to
//! answer "what is this file, roughly" you seek past `DATA` entirely.
//!
//! ## The value encoding preamble
//!
//! Every value begins with two bytes:
//!
//! ```text
//! [codec: u8] [key slot: u8] ( [nonce: 24 bytes] if slot != 0 ) [body]
//! ```
//!
//! This is where the design document's two open questions get answered. Codec
//! is per-chunk rather than per-file, so a `SUMM` chunk can be read and
//! decompressed without decompressing `DATA` — chunk-level compression, as
//! §6 recommends. The key slot is per-chunk for the same reason: a `.emc`
//! file holds plaintext config and per-environment secrets side by side, each
//! keyed differently, which is the split-trust design of §2.7.
//!
//! Nothing about the outer TLV layout changes; the preamble is simply the
//! first two bytes of the value, so a tool that does not understand it still
//! parses the table correctly and skips what it cannot read.
//!
//! `DATA` and `SUMM` values are themselves chunk lists, encoded identically.
//! The tree is uniform all the way down, which is what lets one diff walker
//! work for every format in the family — and, because the nesting is the same
//! TLV layout, what lets [`ChunkList::scan_within`] address one row group or
//! one tile inside `DATA` with the same 12-byte reads the top level uses.

use crate::crypto::{self, KeyRing, NONCE_LEN};
use crate::error::{Error, Result};
use std::io::{Read, Seek, SeekFrom};

/// A four-character ASCII chunk type.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChunkType(pub [u8; 4]);

impl ChunkType {
    /// Embedded schema and validation rules for this file's payload.
    pub const SCHM: ChunkType = ChunkType(*b"SCHM");
    /// Signed provenance chain.
    pub const PROV: ChunkType = ChunkType(*b"PROV");
    /// Capability declaration, for formats a runtime interprets.
    pub const CAPS: ChunkType = ChunkType(*b"CAPS");
    /// Cheap summary / preview tier.
    pub const SUMM: ChunkType = ChunkType(*b"SUMM");
    /// Declarative migration rules.
    pub const MIGR: ChunkType = ChunkType(*b"MIGR");
    /// The full-fidelity payload. Format-specific sub-chunks live inside.
    pub const DATA: ChunkType = ChunkType(*b"DATA");
    /// Key slot descriptions for split-trust encryption.
    pub const KEYS: ChunkType = ChunkType(*b"KEYS");

    pub const fn new(s: &[u8; 4]) -> Self {
        ChunkType(*s)
    }

    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap_or("????")
    }

    pub fn parse(s: &str) -> Result<Self> {
        let b = s.as_bytes();
        if b.len() != 4 || !b.iter().all(|c| c.is_ascii_graphic()) {
            return Err(Error::BadChunkType(s.to_string()));
        }
        Ok(ChunkType([b[0], b[1], b[2], b[3]]))
    }

    /// True for the seven types the spine reserves. A format plugin must not
    /// use these for its own payload sub-chunks.
    pub fn is_reserved(&self) -> bool {
        matches!(
            *self,
            Self::SCHM
                | Self::PROV
                | Self::CAPS
                | Self::SUMM
                | Self::MIGR
                | Self::DATA
                | Self::KEYS
        )
    }
}

impl std::fmt::Display for ChunkType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::fmt::Debug for ChunkType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ChunkType({})", self.as_str())
    }
}

pub const CODEC_RAW: u8 = 0;
pub const CODEC_ZSTD: u8 = 1;

/// Compress values above this size. Below it zstd's frame header costs more
/// than it saves, and every byte of a small chunk is metadata someone may
/// want to read cheaply.
const COMPRESS_THRESHOLD: usize = 256;
const ZSTD_LEVEL: i32 = 12;

/// A nested child at or above this size compresses well enough alone that it
/// is left to do so, and is worth keeping independently decodable. Below it,
/// children are compressed together. 4 KiB is comfortably past the point
/// where zstd stops being dominated by its own frame overhead and starts
/// finding structure.
const INDEPENDENT_CHILD: usize = 4096;

/// How a chunk's value is stored. Kept alongside the decoded value so that
/// reading a file and writing it back preserves the author's intent instead
/// of silently decompressing or, worse, decrypting everything.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Encoding {
    pub codec: u8,
    /// 0 means plaintext. Any other value names a slot in the `KEYS` chunk.
    pub slot: u8,
}

impl Encoding {
    pub const PLAIN: Encoding = Encoding {
        codec: CODEC_RAW,
        slot: 0,
    };

    /// Compress when it is likely to pay for itself, never encrypt by default.
    pub fn auto(len: usize) -> Encoding {
        Encoding {
            codec: if len >= COMPRESS_THRESHOLD {
                CODEC_ZSTD
            } else {
                CODEC_RAW
            },
            slot: 0,
        }
    }

    pub fn sealed(slot: u8, len: usize) -> Encoding {
        Encoding {
            slot,
            ..Encoding::auto(len)
        }
    }
}

/// One decoded chunk. `value` is always plaintext and decompressed; the
/// `enc` field records how it was, or will be, stored.
#[derive(Clone, PartialEq, Eq)]
pub struct Chunk {
    pub ty: ChunkType,
    pub value: Vec<u8>,
    pub enc: Encoding,
}

impl std::fmt::Debug for Chunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Chunk({}, {} bytes)", self.ty, self.value.len())
    }
}

impl Chunk {
    pub fn new(ty: ChunkType, value: Vec<u8>) -> Self {
        let enc = Encoding::auto(value.len());
        Chunk { ty, value, enc }
    }

    /// A chunk stored verbatim — no compression. Used for values that are
    /// already compressed, such as an `.emi` tile or a PNG thumbnail.
    pub fn stored(ty: ChunkType, value: Vec<u8>) -> Self {
        Chunk {
            ty,
            value,
            enc: Encoding::PLAIN,
        }
    }

    pub fn text(ty: ChunkType, s: &str) -> Self {
        Chunk::new(ty, s.as_bytes().to_vec())
    }

    pub fn json(ty: ChunkType, v: &serde_json::Value) -> Result<Self> {
        Ok(Chunk::new(ty, serde_json::to_vec(v)?))
    }

    /// Nest a chunk list inside this chunk's value.
    ///
    /// Where the compression happens depends on how big the children are,
    /// because the two cases want opposite things:
    ///
    /// * **Many small children** — a document's paragraphs, a config's
    ///   values. Individually they are far too small for a compressor to
    ///   find anything in, but collectively they share a vocabulary. These
    ///   are stored raw and the whole list is compressed once, so zstd sees
    ///   one window across all of them. On prose this is the difference
    ///   between roughly matching gzip and losing to it threefold.
    ///
    /// * **Large children** — an image tile, a table's row group. Each is
    ///   already big enough to compress well on its own, and compressing
    ///   them individually keeps each one independently decodable, which is
    ///   what partial reads will want. These keep their own compression and
    ///   the parent stores the result verbatim rather than paying for a
    ///   second, useless pass over already-compressed bytes.
    ///
    /// Either way the value is compressed exactly once. The tiered-read
    /// property is unaffected: it operates on *top-level* chunks, so `SUMM`
    /// still decompresses without touching `DATA`.
    ///
    /// Encryption is per-child and always preserved; only the codec moves.
    pub fn list(ty: ChunkType, children: &ChunkList, keys: &KeyRing) -> Result<Self> {
        let bulky = children.iter().any(|c| c.value.len() >= INDEPENDENT_CHILD);
        if bulky {
            Ok(Chunk::stored(ty, children.encode(keys)?))
        } else {
            Ok(Chunk::new(ty, children.encode_flat(keys)?))
        }
    }

    pub fn as_str(&self) -> Result<&str> {
        std::str::from_utf8(&self.value)
            .map_err(|_| Error::Other(format!("{} chunk is not valid UTF-8", self.ty)))
    }

    pub fn as_json(&self) -> Result<serde_json::Value> {
        Ok(serde_json::from_slice(&self.value)?)
    }

    /// Parse this chunk's value as a nested chunk list.
    pub fn as_list(&self, keys: &KeyRing) -> Result<ChunkList> {
        ChunkList::decode(&self.value, keys)
    }

    pub fn sealed_to(mut self, slot: u8) -> Self {
        self.enc.slot = slot;
        self
    }

    /// Serialise value → preamble + optional nonce + codec body.
    fn encode_value(&self, keys: &KeyRing) -> Result<Vec<u8>> {
        let body = match self.enc.codec {
            CODEC_RAW => self.value.clone(),
            CODEC_ZSTD => zstd::encode_all(&self.value[..], ZSTD_LEVEL)
                .map_err(|e| Error::Other(format!("zstd: {e}")))?,
            other => return Err(Error::UnknownCodec(other)),
        };

        if self.enc.slot == 0 {
            let mut out = Vec::with_capacity(body.len() + 2);
            out.push(self.enc.codec);
            out.push(0);
            out.extend_from_slice(&body);
            return Ok(out);
        }

        // Associated data binds the ciphertext to its chunk type and slot, so
        // a sealed chunk cannot be moved to a different position in the file
        // and still authenticate.
        let key = keys.key(self.enc.slot)?;
        let aad = aad_for(self.ty, self.enc.slot);
        let (nonce, ct) = crypto::seal(key, &aad, &body)?;

        let mut out = Vec::with_capacity(ct.len() + 2 + NONCE_LEN);
        out.push(self.enc.codec);
        out.push(self.enc.slot);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    fn decode_value(ty: ChunkType, raw: &[u8], keys: &KeyRing) -> Result<Chunk> {
        if raw.len() < 2 {
            return Err(Error::Truncated("chunk value preamble"));
        }
        let enc = Encoding {
            codec: raw[0],
            slot: raw[1],
        };

        let body: Vec<u8> = if enc.slot == 0 {
            raw[2..].to_vec()
        } else {
            if raw.len() < 2 + NONCE_LEN {
                return Err(Error::Truncated("sealed chunk nonce"));
            }
            match keys.try_key(enc.slot) {
                Some(key) => {
                    let nonce: [u8; NONCE_LEN] = raw[2..2 + NONCE_LEN].try_into().unwrap();
                    let aad = aad_for(ty, enc.slot);
                    crypto::open(key, &nonce, &aad, &raw[2 + NONCE_LEN..])
                        .map_err(|_| Error::Decrypt { slot: enc.slot })?
                }
                // A locked chunk is not an error on its own. Reading the
                // public half of a split-trust file is the normal case, so
                // the sealed value is preserved verbatim and re-emitted
                // untouched on write.
                None => {
                    return Ok(Chunk {
                        ty,
                        value: raw.to_vec(),
                        enc: Encoding {
                            codec: SEALED_OPAQUE,
                            slot: enc.slot,
                        },
                    })
                }
            }
        };

        let value = match enc.codec {
            CODEC_RAW => body,
            CODEC_ZSTD => {
                zstd::decode_all(&body[..]).map_err(|e| Error::Other(format!("zstd: {e}")))?
            }
            other => return Err(Error::UnknownCodec(other)),
        };
        Ok(Chunk { ty, value, enc })
    }

    /// True when this chunk was read without its key and is being carried
    /// through as opaque bytes.
    pub fn is_locked(&self) -> bool {
        self.enc.codec == SEALED_OPAQUE
    }

    /// Decode one chunk from its stored value — the bytes a [`ChunkRef`]
    /// points at, preamble included. This is the single-record counterpart
    /// to [`ChunkList::decode`], for a reader that has seeked to exactly one
    /// chunk and wants nothing else in the file.
    pub fn decode_stored(ty: ChunkType, stored: &[u8], keys: &KeyRing) -> Result<Chunk> {
        Chunk::decode_value(ty, stored, keys)
    }
}

/// Where a chunk lies in the file, with nothing decoded.
///
/// A scan produces these, and a partial read works from them: type and
/// stored size are enough for a format to decide which children it wants
/// before paying to decompress any of them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ChunkRef {
    pub ty: ChunkType,
    /// Absolute file offset of the stored value, preamble included.
    pub at: u64,
    /// Length on disk: after compression and encryption. Not the size this
    /// decodes to, which nothing can know without decoding it.
    pub stored: u64,
}

impl std::fmt::Debug for ChunkRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}+{}", self.ty, self.at, self.stored)
    }
}

/// What a scan of one chunk's children found.
///
/// Three outcomes rather than an `Option`, because "there is nothing there",
/// "it is all one compressed stream" and "it is sealed" call for three
/// different things from the caller and only one of them is worth reporting
/// to a person.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Nested {
    /// Each child lies at its own offset and decodes on its own.
    Addressable(Vec<ChunkRef>),
    /// The parent's value is one compressed stream, so no child can be
    /// reached without decompressing all of it. This is what a payload of
    /// many small children looks like — see [`Chunk::list`] — and such a
    /// payload is small enough that a partial read would save nothing.
    Compressed,
    /// The parent is sealed to a key slot. Its children are not merely
    /// unreachable without decoding, they are unreadable without the key.
    Sealed(u8),
}

/// Sentinel codec used only in memory, never on disk: marks a chunk that was
/// read while locked and whose `value` therefore still holds its raw stored
/// form, preamble included.
const SEALED_OPAQUE: u8 = 0xFF;

fn aad_for(ty: ChunkType, slot: u8) -> [u8; 5] {
    [ty.0[0], ty.0[1], ty.0[2], ty.0[3], slot]
}

/// A flat, ordered list of chunks. Order is preserved on round-trip because
/// several payloads (a document's sections, a table's row groups) are
/// meaningfully ordered, and a diff that reported reordering as a rewrite
/// would defeat the point of the format.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct ChunkList(pub Vec<Chunk>);

impl ChunkList {
    pub fn new() -> Self {
        ChunkList(Vec::new())
    }

    pub fn push(&mut self, c: Chunk) -> &mut Self {
        self.0.push(c);
        self
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Chunk> {
        self.0.iter()
    }

    /// First chunk of a type. Reserved chunks appear at most once; payload
    /// sub-chunks may repeat, which is what `all` is for.
    pub fn get(&self, ty: ChunkType) -> Option<&Chunk> {
        self.0.iter().find(|c| c.ty == ty)
    }

    pub fn get_mut(&mut self, ty: ChunkType) -> Option<&mut Chunk> {
        self.0.iter_mut().find(|c| c.ty == ty)
    }

    pub fn all(&self, ty: ChunkType) -> impl Iterator<Item = &Chunk> {
        self.0.iter().filter(move |c| c.ty == ty)
    }

    pub fn require(&self, ty: ChunkType, name: &'static str) -> Result<&Chunk> {
        self.get(ty).ok_or(Error::MissingChunk(name))
    }

    /// Insert or replace a single-occurrence chunk, keeping its position if
    /// it already existed.
    pub fn set(&mut self, c: Chunk) {
        match self.0.iter_mut().find(|x| x.ty == c.ty) {
            Some(slot) => *slot = c,
            None => self.0.push(c),
        }
    }

    pub fn remove(&mut self, ty: ChunkType) -> Option<Chunk> {
        let i = self.0.iter().position(|c| c.ty == ty)?;
        Some(self.0.remove(i))
    }

    pub fn encode(&self, keys: &KeyRing) -> Result<Vec<u8>> {
        self.encode_with(keys, false)
    }

    /// Encode with every child's codec forced to raw, for a list that is
    /// about to be compressed as a whole. See [`Chunk::list`].
    pub fn encode_flat(&self, keys: &KeyRing) -> Result<Vec<u8>> {
        self.encode_with(keys, true)
    }

    fn encode_with(&self, keys: &KeyRing, flatten: bool) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        for c in &self.0 {
            // A locked chunk already holds its exact stored bytes.
            let v = if c.is_locked() {
                c.value.clone()
            } else if flatten && c.enc.codec != CODEC_RAW {
                Chunk {
                    ty: c.ty,
                    value: c.value.clone(),
                    enc: Encoding {
                        codec: CODEC_RAW,
                        slot: c.enc.slot,
                    },
                }
                .encode_value(keys)?
            } else {
                c.encode_value(keys)?
            };
            out.extend_from_slice(&c.ty.0);
            out.extend_from_slice(&(v.len() as u64).to_le_bytes());
            out.extend_from_slice(&v);
        }
        Ok(out)
    }

    pub fn decode(bytes: &[u8], keys: &KeyRing) -> Result<Self> {
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < bytes.len() {
            if i + 12 > bytes.len() {
                return Err(Error::Truncated("chunk record"));
            }
            let ty = ChunkType([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
            let len = u64::from_le_bytes(bytes[i + 4..i + 12].try_into().unwrap()) as usize;
            let start = i + 12;
            let end = start
                .checked_add(len)
                .ok_or(Error::Truncated("chunk value"))?;
            if end > bytes.len() {
                return Err(Error::Truncated("chunk value"));
            }
            out.push(Chunk::decode_value(ty, &bytes[start..end], keys)?);
            i = end;
        }
        Ok(ChunkList(out))
    }

    /// Walk the table's records without decoding any values. This is the
    /// seek-only path a viewer uses to find `SUMM`.
    pub fn scan<R: Read + Seek>(
        r: &mut R,
        table_offset: u64,
        table_len: u64,
    ) -> Result<Vec<ChunkRef>> {
        let mut out = Vec::new();
        let mut pos = table_offset;
        let end = table_offset + table_len;
        while pos + 12 <= end {
            r.seek(SeekFrom::Start(pos))?;
            let mut hdr = [0u8; 12];
            r.read_exact(&mut hdr)?;
            let ty = ChunkType([hdr[0], hdr[1], hdr[2], hdr[3]]);
            let len = u64::from_le_bytes(hdr[4..12].try_into().unwrap());
            let value_at = pos + 12;
            if value_at + len > end {
                return Err(Error::Truncated("chunk value"));
            }
            out.push(ChunkRef {
                ty,
                at: value_at,
                stored: len,
            });
            pos = value_at + len;
        }
        Ok(out)
    }

    /// The same walk, one level down: the children of `parent`, at their own
    /// absolute offsets, still without decoding anything.
    ///
    /// This is what makes a partial read possible. A `DATA` chunk whose
    /// children were each large enough to compress on their own is stored
    /// verbatim (see [`Chunk::list`]), so its children's records sit at fixed
    /// offsets in the file and any one of them — one row group, one tile —
    /// can be read and decompressed alone. A `DATA` chunk that was compressed
    /// as a whole cannot offer that, and says so rather than pretending.
    pub fn scan_within<R: Read + Seek>(r: &mut R, parent: ChunkRef) -> Result<Nested> {
        if parent.stored < 2 {
            return Err(Error::Truncated("chunk value preamble"));
        }
        r.seek(SeekFrom::Start(parent.at))?;
        let mut pre = [0u8; 2];
        r.read_exact(&mut pre)?;
        if pre[1] != 0 {
            return Ok(Nested::Sealed(pre[1]));
        }
        if pre[0] != CODEC_RAW {
            return Ok(Nested::Compressed);
        }
        Ok(Nested::Addressable(Self::scan(
            r,
            parent.at + 2,
            parent.stored - 2,
        )?))
    }
}

impl<'a> IntoIterator for &'a ChunkList {
    type Item = &'a Chunk;
    type IntoIter = std::slice::Iter<'a, Chunk>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_nested_tree() {
        let keys = KeyRing::empty();
        let mut inner = ChunkList::new();
        inner.push(Chunk::text(ChunkType::new(b"SECT"), "hello"));
        inner.push(Chunk::text(ChunkType::new(b"SECT"), &"long ".repeat(200)));

        let mut outer = ChunkList::new();
        outer.push(Chunk::list(ChunkType::DATA, &inner, &keys).unwrap());

        let bytes = outer.encode(&keys).unwrap();
        let back = ChunkList::decode(&bytes, &keys).unwrap();
        let data = back.require(ChunkType::DATA, "DATA").unwrap();
        let kids = data.as_list(&keys).unwrap();
        assert_eq!(kids.len(), 2);
        assert_eq!(kids.0[0].as_str().unwrap(), "hello");
        assert_eq!(kids.0[1].as_str().unwrap().len(), 1000);
    }

    #[test]
    fn a_nested_list_is_compressed_once_not_twice() {
        let keys = KeyRing::empty();
        // Paragraph-shaped children: each one is over the compression
        // threshold and internally unrepetitive, but they share a vocabulary
        // with each other. That is what real prose, real config and real
        // tabular data all look like, and it is the case where compressing
        // children separately throws away everything worth having.
        let words = [
            "measurement",
            "ridge",
            "instrument",
            "station",
            "calibration",
            "sample",
            "traverse",
            "ascent",
            "descent",
            "bearing",
        ];
        let mut rng: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            words[(rng >> 33) as usize % words.len()]
        };

        let mut inner = ChunkList::new();
        for _ in 0..200 {
            let mut para = String::new();
            while para.len() < 400 {
                para.push_str(next());
                para.push(' ');
            }
            inner.push(Chunk::text(ChunkType::new(b"SECT"), &para));
        }

        let once = Chunk::list(ChunkType::DATA, &inner, &keys).unwrap();
        let twice = Chunk::new(ChunkType::DATA, inner.encode(&keys).unwrap());

        let a = ChunkList(vec![once.clone()]).encode(&keys).unwrap().len();
        let b = ChunkList(vec![twice]).encode(&keys).unwrap().len();
        assert!(
            a * 2 < b,
            "one compression pass ({a}) should beat two ({b})"
        );

        // And it still decodes to exactly the same children.
        let back = once.as_list(&keys).unwrap();
        assert_eq!(back.len(), 200);
        assert_eq!(back.0[7].value, inner.0[7].value);
    }

    #[test]
    fn large_values_get_compressed_small_ones_do_not() {
        let keys = KeyRing::empty();
        let small = Chunk::text(ChunkType::new(b"NOTE"), "tiny");
        let big = Chunk::text(ChunkType::new(b"NOTE"), &"redundant ".repeat(500));
        assert_eq!(small.enc.codec, CODEC_RAW);
        assert_eq!(big.enc.codec, CODEC_ZSTD);

        let list = ChunkList(vec![big.clone()]);
        let encoded = list.encode(&keys).unwrap();
        assert!(
            encoded.len() < big.value.len() / 4,
            "zstd should shrink this"
        );
        assert_eq!(
            ChunkList::decode(&encoded, &keys).unwrap().0[0].value,
            big.value
        );
    }

    #[test]
    fn scan_finds_offsets_without_decoding() {
        let keys = KeyRing::empty();
        let mut list = ChunkList::new();
        list.push(Chunk::text(ChunkType::SUMM, "summary"));
        list.push(Chunk::text(ChunkType::DATA, &"payload ".repeat(1000)));
        let bytes = list.encode(&keys).unwrap();

        let mut cur = std::io::Cursor::new(bytes.clone());
        let found = ChunkList::scan(&mut cur, 0, bytes.len() as u64).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].ty, ChunkType::SUMM);
        assert_eq!(found[1].ty, ChunkType::DATA);
        assert_eq!(found[0].at, 12);
    }

    /// Incompressible bytes, so a child stays over [`INDEPENDENT_CHILD`]
    /// after zstd has had a go at it.
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

    #[test]
    fn one_bulky_child_can_be_read_without_its_siblings() {
        let keys = KeyRing::empty();
        let mut inner = ChunkList::new();
        for i in 0..5u8 {
            let mut v = noise(20_000);
            v[0] = i; // so the decoded children are distinguishable
            inner.push(Chunk::stored(ChunkType::new(b"TILE"), v));
        }
        let mut outer = ChunkList::new();
        outer.push(Chunk::list(ChunkType::DATA, &inner, &keys).unwrap());
        let bytes = outer.encode(&keys).unwrap();

        let mut cur = std::io::Cursor::new(bytes.clone());
        let top = ChunkList::scan(&mut cur, 0, bytes.len() as u64).unwrap();
        let Nested::Addressable(kids) = ChunkList::scan_within(&mut cur, top[0]).unwrap() else {
            panic!("bulky children should be individually addressable");
        };
        assert_eq!(kids.len(), 5);

        // Read the third child and nothing else.
        let want = kids[2];
        let slice = &bytes[want.at as usize..(want.at + want.stored) as usize];
        let c = Chunk::decode_stored(want.ty, slice, &keys).unwrap();
        assert_eq!(c.ty, ChunkType::new(b"TILE"));
        assert_eq!(c.value, inner.0[2].value);
        // …and reading it cost a fraction of the payload it sits in.
        assert!(want.stored * 4 < top[0].stored);
    }

    #[test]
    fn a_payload_of_small_children_says_it_cannot_be_seeked_into() {
        let keys = KeyRing::empty();
        let mut inner = ChunkList::new();
        for i in 0..50 {
            inner.push(Chunk::text(ChunkType::new(b"NODE"), &format!("value {i}")));
        }
        let mut outer = ChunkList::new();
        outer.push(Chunk::list(ChunkType::DATA, &inner, &keys).unwrap());
        let bytes = outer.encode(&keys).unwrap();

        let mut cur = std::io::Cursor::new(bytes.clone());
        let top = ChunkList::scan(&mut cur, 0, bytes.len() as u64).unwrap();
        assert_eq!(
            ChunkList::scan_within(&mut cur, top[0]).unwrap(),
            Nested::Compressed
        );
    }

    #[test]
    fn a_sealed_parent_reports_its_slot_rather_than_its_children() {
        let mut keys = KeyRing::empty();
        keys.add_slot(3, "prod", "correct horse").unwrap();
        let mut inner = ChunkList::new();
        inner.push(Chunk::stored(ChunkType::new(b"TILE"), noise(20_000)));
        let mut outer = ChunkList::new();
        outer.push(
            Chunk::list(ChunkType::DATA, &inner, &keys)
                .unwrap()
                .sealed_to(3),
        );
        let bytes = outer.encode(&keys).unwrap();

        let mut cur = std::io::Cursor::new(bytes.clone());
        let top = ChunkList::scan(&mut cur, 0, bytes.len() as u64).unwrap();
        assert_eq!(
            ChunkList::scan_within(&mut cur, top[0]).unwrap(),
            Nested::Sealed(3)
        );
    }

    #[test]
    fn truncation_is_reported_not_guessed() {
        let keys = KeyRing::empty();
        let list = ChunkList(vec![Chunk::text(ChunkType::DATA, "abcdefgh")]);
        let bytes = list.encode(&keys).unwrap();
        assert!(matches!(
            ChunkList::decode(&bytes[..bytes.len() - 3], &keys),
            Err(Error::Truncated(_))
        ));
    }
}
