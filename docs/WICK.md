<img src="../assets/wick.png" alt="Wick" width="96" align="right">

# Wick v1.0 — container specification

Wick is the container every Ember file format sits in. It defines the header,
the chunk table, hashing, provenance, capability declarations, migration and
encryption — once — so that `.emt`, `.emd`, `.emi`, `.emc` and `.emx` do not
each reinvent them.

A format built on Wick defines only what goes inside one chunk (`DATA`). It
gets everything else for free, and — more importantly — gets it *identically*,
so one tool reads every format in the family.

This document is normative for v1.0. It is implemented by
[`crates/libwick`](../crates/libwick), and every claim below is exercised by
that crate's tests.

**Contents** · [Byte layout](#1-byte-layout) · [Chunk table](#2-chunk-table) ·
[Reserved chunks](#3-reserved-chunks) · [Provenance](#4-provenance-prov) ·
[Capabilities](#5-capabilities-caps) · [Encryption](#6-encryption) ·
[Migration](#7-migration-migr) · [Versioning](#8-versioning) ·
[Deviations from the design document](#9-deviations-from-the-design-document)

---

## 1. Byte layout

```
Offset   Size      Field
0x00     4 bytes   Magic: "WICK"
0x04     2 bytes   Format tag, ASCII (see §1.2)
0x06     2 bytes   Spec version: major byte, then minor byte
0x08     4 bytes   Flags bitfield (u32)
0x0C     8 bytes   Chunk table offset (u64, from file start)
0x14     8 bytes   Chunk table length (u64, bytes)
0x1C     32 bytes  BLAKE3 of the chunk table bytes
0x3C     —         Header ends; the chunk table follows
```

The header is exactly **60 bytes** and fixed for all of v1. Any tool can
identify a Wick file, its payload format and its version from a single 60-byte
read without touching anything else.

Multi-byte integers are **little-endian**. The version is the one exception: it
is two separate bytes, major first, so a hex dump of `01 00` reads as "v1.0"
the way this document writes it.

### 1.1 Content hash

`content_hash` is the BLAKE3 of the bytes in
`[table_offset, table_offset + table_len)`. A reader **must** verify it before
using the payload. `libwick` does this on every read; skipping it is available
only for inspecting a file already known to be damaged.

BLAKE3 rather than SHA-256 because it is several times faster on the sizes
files actually are, and because EMBR already standardised on it — a shared
verification library across the two projects is worth more than an argument
about hash functions.

### 1.2 Format tags

| Tag  | Format  | Replaces               |
|------|---------|------------------------|
| `MT` | `.emt`  | `.txt`                 |
| `MD` | `.emd`  | `.pdf`                 |
| `MI` | `.emi`  | `.png`                 |
| `MC` | `.emc`  | `.json`/`.yaml`/`.toml` |
| `MX` | `.emx`  | `.csv`                 |
| `AR` | `.embr` | `.zip`/`.tar` — *reserved; see [§9](#9-deviations-from-the-design-document)* |

A tag is two printable ASCII bytes. Unregistered tags are permitted; a reader
that does not know one reports it by name rather than guessing.

### 1.3 Flags

```
bit 0   has a provenance chain
bit 1   has a capability declaration
bit 2   has a summary tier
bit 3   at least one chunk is encrypted
bit 4   split-trust: chunks are encrypted to more than one key
bits 5-31  reserved, must be zero
```

Flags let a reader decide what work a file requires before parsing the table:
whether signatures need verifying, whether a passphrase will be needed,
whether a cheap preview exists.

Flags are **derived, not authored**. A writer recomputes them from the chunks
actually present, so they can never disagree with the file's contents.

---

## 2. Chunk table

Everything from `table_offset` for `table_len` bytes is a flat sequence of TLV
records:

```
[type: 4 bytes ASCII] [length: u64 little-endian] [value: length bytes]
```

Because the length precedes the value, a reader walks the whole table with
12-byte reads and seeks, never touching a value it does not want. That is what
makes the summary tier ([§3](#3-reserved-chunks)) genuinely cheap: to answer
"what is this file, roughly", seek past `DATA` entirely.

In v1.0 `table_offset` is always `0x3C`. The field exists so a later version
can move the table (to the tail, say, as EMBR does) without a format break.

`length` is the **stored** length, including everything in §2.1. A reader can
always skip a chunk it does not understand.

### 2.1 Value encoding preamble

Every chunk value begins with two bytes:

```
[codec: u8] [key slot: u8] ( [nonce: 24 bytes] if slot != 0 ) [body]
```

| Codec | Meaning |
|---|---|
| 0 | raw |
| 1 | zstd |

Key slot `0` means plaintext. Any other value names a slot in `KEYS`
([§6](#6-encryption)).

Decoding order is **decrypt, then decompress**.

This is where the design document's two open questions are answered. Both
compression and encryption are per-chunk rather than per-file, so `SUMM` can be
read without decompressing `DATA`, and secrets can sit beside plaintext config
in one file. The outer TLV layout is unchanged — the preamble is simply the
first two bytes of the value.

### 2.2 Nesting

`DATA` and `SUMM` hold chunk lists, encoded identically and recursively. The
tree is uniform all the way down, which is what lets one diff walker work for
every format in the family — and, because a nested list is the same TLV layout
as the top level, what lets a reader address one child of `DATA` with the same
12-byte reads and seeks (§2.4).

### 2.3 Partial reads within `DATA`

When `DATA`'s value is stored raw — the case §2.4 calls *children at or over
4 KiB* — its children's records lie at fixed file offsets, and any one of them
can be read and decompressed without touching its siblings. This is what makes
a table larger than memory viewable: showing twenty rows of a million-row
`.emx` decompresses one `RGRP` and leaves the rest on disk.

A reader walks the child index the same way it walks the table:

1. Seek to `DATA`'s value and read the two preamble bytes.
2. If the codec is not raw, or the slot is not `0`, **stop**. The children are
   one compressed stream, or sealed, and cannot be reached individually.
3. Otherwise walk TLV records from `value_offset + 2` to the end of the value.
   Each record's absolute offset and stored length is a child that decodes
   alone, by §2.1, exactly as a top-level chunk does.

Case 2 is not a failure. A payload of many small children is one compressed
stream *because* that is how it compresses (§2.4), and it is small enough that
seeking into it would save nothing. The two cases are distinguishable and must
be reported apart: `hearth info --json` reports `payload.addressable` with a
`why` of `compressed` or `sealed`.

Three properties a partial read does **not** relax:

* **The content hash is still verified.** It covers the whole table, so
  checking it means reading the whole table — streamed in blocks rather than
  buffered. A partial read saves decompression and memory, not I/O. An
  integrity guarantee that lapses when it is inconvenient is not one.
* **Every other reserved chunk is read whole.** `SCHM`, `PROV`, `CAPS`,
  `MIGR`, `KEYS` and `SUMM` are all small by design.
* **A partially read file must not be written back.** Once decoded, the first
  row group of a million-row table is indistinguishable from a table that has
  one row group, and writing it would produce a valid, correctly hashed file
  with the rest of the payload silently gone. `libwick` refuses.

Children are taken as a **prefix**, never an arbitrary subset. Row groups,
tiles and blocks are ordered, and a payload with holes in it would render as a
table whose twenty-first row is not the twenty-first row.

### 2.4 Where compression happens

A nested list is compressed exactly once, and *where* depends on how large its
children are:

* **Children under 4 KiB** are stored raw and the whole list is compressed as
  one unit. A paragraph or a config value is far too small for a compressor to
  find anything in alone, but a document's paragraphs share a vocabulary with
  each other. Measured on a megabyte of prose, compressing the list as one unit
  rather than child-by-child is the difference between landing 22% above
  `gzip -9` and landing 185% above it.

* **Children at or over 4 KiB** — an image tile, a table's row group — keep
  their own compression and the parent stores the result verbatim. Each is
  already big enough to compress well alone, and staying independently
  decodable is what a partial read (§2.3) needs. This costs roughly 11% of ratio
  on a large table versus compressing the lot together, which is the price of
  being able to decode one row group without decoding the rest.

Either way, nothing is compressed twice.

---

## 3. Reserved chunks

Seven types are reserved by the spine. A format plugin must not use them for
its own payload sub-chunks. All are optional except `DATA`.

| Type   | Purpose |
|--------|---------|
| `DATA` | The full-fidelity payload. Format-specific sub-chunks live inside. |
| `SCHM` | Embedded schema and validation rules for this file's payload. |
| `PROV` | Signed, hash-linked provenance chain. |
| `CAPS` | Capability declaration, for formats a runtime interprets. |
| `SUMM` | Cheap summary / preview tier. |
| `MIGR` | Declarative migration rules. |
| `KEYS` | Key slot table for split-trust encryption. Always plaintext. |

### 3.1 `SCHM`

JSON. A deliberately small rule language — types, requiredness, ranges,
enumerations and units — plus an opaque `extra` object for rules only one
plugin understands.

```json
{
  "kind": "config",
  "version": 1,
  "fields": [
    {"path": "database.port", "type": "int", "required": true, "min": 1, "max": 65535},
    {"path": "hosts.*", "type": "string"},
    {"path": "throughput", "type": "float", "unit": "MB/s"}
  ]
}
```

`kind` names the payload the schema describes and is checked against the
file's format tag, so a `.emx` cannot carry a config schema and claim to
validate. `version` is the *payload* schema version, independent of the Wick
spec version in the header; it is what `MIGR` rules move between.

A numeric path segment matches any list index: `hosts.*` describes every
element of `hosts`. A schema pinned to today's element count would fail the
moment someone added a host.

The rule language is small on purpose. A schema that can express arbitrary
logic is a schema no other tool can fully implement, which defeats the point of
shipping it inside the file. Anything beyond these rules belongs in the
plugin's own validator.

### 3.2 `SUMM`

A chunk list holding a lower-fidelity version of `DATA`: an outline and word
count for text, a thumbnail and palette for images, a schema and sample rows
for tables. A consumer that only needs "what is this file" reads `SUMM` and
never touches `DATA`.

`SUMM` is **derived**. It must be rebuilt whenever `DATA` changes; `libwick`
does this after every migration, and a diff ignores it, because reporting a
word count that moved from 3 to 4 alongside the edit that moved it tells the
reader the same thing twice.

---

## 4. Provenance (`PROV`)

A JSON array of entries, oldest first. Each commits to the hash of the one
before it, so removing an entry or reordering two of them breaks every link
after it — the same construction as a git commit graph, scoped to a single
file.

```json
{
  "tool": "Hearth v0.1.0",
  "action": "converted from legacy .pdf",
  "timestamp": "2026-08-14T10:22:00Z",
  "prev_hash": "b3…",
  "content_hash": "b3…",
  "key": "ed25519 public key, hex",
  "signature": "hex"
}
```

`prev_hash` is `null` on the first entry. `key` and `signature` are both
optional and must be present together; an entry with one and not the other is
malformed.

### 4.1 Canonical form

Signatures are over a canonical byte string, **not** over the stored JSON,
because JSON has no canonical spelling — key order, whitespace and number
formatting all vary by writer, and a signature over one writer's output would
not verify against another's.

The canonical form is a fixed sequence of length-prefixed fields:

```
u32le(len) || bytes    for each of, in order:
  "wick-prov-1"        version marker
  tool
  action
  timestamp
  prev_hash            or "" when absent
  content_hash         or "" when absent
  key                  or "" when absent
```

An entry's hash — what the next entry's `prev_hash` points at — is the BLAKE3
of this string. The signature is Ed25519 over the same bytes. The key is part
of the signed material, so a signature cannot be transplanted onto a different
identity.

### 4.2 Verification

Verification reports four distinguishable outcomes, because they mean
different things:

* **Intact and fully signed** — every entry links correctly and is attributable.
* **Intact but partly unsigned** — tamper-evident, but not everything can be
  attributed to a key.
* **Broken at entry _n_** — a link or a signature failed, with the index.
* **Absent** — the file has no chain.

"Unsigned" is never reported as "valid". A tool that treats the absence of a
signature as a pass has removed the only thing signing was for.

### 4.3 On the design document's omission

The design document's `PROV` entry has no `key` field. It is added here
because a chain nobody can name the signer of is not verifiable: without a
public key there is nothing to check the signature against.

---

## 5. Capabilities (`CAPS`)

JSON. What a file is allowed to ask a runtime for.

```json
{
  "network": false,
  "filesystem": ["read:./data", "write:./out"],
  "env_read": ["HOME"],
  "max_memory_mb": 256
}
```

Relevant to `.emc` today and to any future format a runtime *interprets*
rather than merely displays.

This is **declarative, not advisory**. A Wick-aware runtime must refuse to
grant more than the file declares. The file does not ask nicely, and it cannot
ask for more later.

A filesystem rule is `read:<prefix>` or `write:<prefix>`. Paths are matched
after **lexical** normalisation: `.` is dropped and `..` is resolved without
touching the disk. Lexical rather than symlink-resolving is deliberate —
resolving symlinks would make the answer depend on the state of the disk at
check time, which is the classic time-of-check/time-of-use bug. A runtime that
needs symlink safety opens with `O_NOFOLLOW` instead.

A `write` grant implies `read` on the same prefix. A `read` grant never implies
`write`.

Hearth executes nothing, so it validates and reports rather than enforcing.
`libwick::caps::Grant` is the enforcement primitive for runtimes that do
execute something; it lives in the spine so that every consumer applies the
same path normalisation, since a capability check reimplemented per caller is a
capability check with a different bug in each caller.

---

## 6. Encryption

Chunk values may be encrypted individually, each to its own key slot. A `.emc`
file can hold public config in plaintext, production secrets under one
passphrase and staging secrets under another, all in one file — and a reader
holding one passphrase reads exactly its own slot.

* **Cipher**: XChaCha20-Poly1305, 24-byte random nonce. The nonce is large
  enough that random generation is safe without tracking a counter, which
  matters for a format whose writer has no memory of prior writes.
* **KDF**: Argon2id, 64 MiB, 3 passes, 1 lane, per-slot 16-byte salt.
* **Associated data**: `chunk type (4 bytes) || slot (1 byte)`. This binds a
  ciphertext to its position, so a sealed chunk cannot be moved elsewhere in
  the file and still authenticate.

`KEYS` is always plaintext and describes the slots without helping open them:

```json
[{"slot": 1, "label": "prod", "salt": "hex", "kdf": "argon2id", "alg": "xchacha20poly1305"}]
```

### 6.1 Reading without a key

A chunk sealed to a slot the reader cannot open is **not an error**. Reading
the public half of a split-trust file is the ordinary case. Such a chunk is
carried through as opaque bytes and re-emitted byte-for-byte on the next write,
so a tool holding only the staging passphrase can edit and save a file without
damaging the production half.

Tools built on this must refuse to *export* a payload that is silently missing
sealed values. Writing a config with a quietly absent password is how a
deployment breaks at three in the morning.

### 6.2 Sealing a whole file

Nothing above is specific to configuration. Sealing *every* content chunk to a
single slot turns any Wick file into one that can be handed to somebody over a
channel the sender does not trust — which is what `hearth encrypt` does.

This is a convention rather than a new mechanism, and the convention is which
two chunks stay in the clear:

* **`KEYS` must not be encrypted.** It carries the salt the passphrase is
  stretched against, so sealing it would shut the key inside the lock.
* **`PROV` is left readable**, so the chain can still be verified and a later
  write can extend it rather than starting a new one.

Everything else — `DATA`, `SCHM`, `SUMM`, `CAPS`, `MIGR` — is content and may
be sealed. A reader must state plainly what such a file still reveals: the
header, the chunk table, and therefore the format, the number of chunks and
the size of each. A Wick file is identifiable as one at a glance by design,
and encryption does not change that.

---

## 7. Migration (`MIGR`)

The problem: a reading tool otherwise has to know every historical version of
every schema it might encounter, forever. That knowledge lives in the tool,
ages badly, and is what nobody maintains.

Putting the upgrade rules in the file inverts it. A file written in 2026 knows
how to present itself to a reader written in 2031; the reader only has to know
how to *apply* rules.

```json
{
  "rules": [
    {
      "from": 1,
      "to": 2,
      "note": "database moved under db",
      "ops": [
        {"op": "rename_key", "from": "database", "to": "db"},
        {"op": "set_default", "path": "db.pool", "value": 10},
        {"op": "drop_key", "path": "debug"}
      ]
    }
  ]
}
```

`from` and `to` are **payload schema versions** (from `SCHM`), not Wick spec
versions. Those move independently: a config schema can reach v5 while the
container is still Wick v1.0.

Rules are a declarative transform table, never code. A step renames a chunk,
drops one, inserts one, or hands a named operation to the format plugin. It
cannot loop, branch, read the filesystem, or do anything else that would make
opening an untrusted file interesting.

**Spine operations**, available to every format:

| `op` | Arguments |
|---|---|
| `rename_chunk` | `from`, `to` — four-character chunk types |
| `drop_chunk` | `type` |
| `add_chunk` | `type`, `text` |

**Plugin operations** are anything else. The engine hands an unrecognised
operation to the format's plugin; an operation nobody handles is an **error**,
never a silent no-op, because a migration that half-runs leaves a file claiming
a version it does not have.

Planning is breadth-first, so a format that shipped 1→2→3 and later added a
direct 1→3 takes the short path.

---

## 8. Versioning

* A **major** version change alters the header or chunk encoding. A reader
  that does not know the major version refuses the file outright, because
  nothing after the header can be trusted.
* A **minor** version change adds chunk types or fields. Older readers skip
  what they do not recognise, because every chunk carries its length.
* **Payload** schema versions live in `SCHM` and move under `MIGR`.

This is what "fails predictably instead of silently misreading" means in
practice: three separate axes, each with a defined failure.

---

## 9. Deviations from the design document

The [original design document](../EFE.md) is a sketch with an explicit list of
open questions. Where the implementation departs from it, here is what changed
and why.

**Flag bit for split-trust.** §2.4 of the design assigns bit 3 to "encrypted"
and bit 4 to "split-trust", while §2.7 refers to split-trust as bit 5. The
bitfield table wins: it is **bit 4**.

**The value encoding preamble.** The design's TLV is `[type][length][value]`
with no room for per-chunk compression or per-chunk keys, both of which §6
lists as open questions. Rather than change the record layout, the codec and
key slot are the first two bytes of the *value*. The outer layout is exactly as
specified and a tool that ignores the preamble still parses and skips chunks
correctly.

**`key` in `PROV` entries.** Added; see [§4.3](#43-on-the-design-documents-omission).

**Migration versions are payload versions.** The design describes `MIGR` as
upgrading "from an older spec version". Container-level changes cannot be
expressed as a payload transform table, and every real migration is a payload
schema change, so `from`/`to` are `SCHM` versions.

**`content_hash` in `PROV` is optional.** It cannot be known until after the
entry is embedded, because embedding it changes the payload. A writer that can
compute it in a second pass should.

### Still open

* **`.embr` interop.** `AR` is reserved for it, but `.embr` predates this spec
  and has not been retrofitted onto Wick. Hearth does not read `.embr` today.
  The decision — retrofit, or keep them separate and make Hearth aware of both
  — is not yet made.
* **Signature scheme alignment with ChainTrace.** Ed25519 and BLAKE3 are the
  assumptions here; whether ChainTrace uses the same primitives, and whether a
  shared verification library is worth extracting, is unresolved.
* **Partial reads within `DATA`.** Large children are kept independently
  decodable in anticipation of this, but nothing yet seeks to a single row
  group or tile.
