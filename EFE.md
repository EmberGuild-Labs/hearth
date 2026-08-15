# The Ember File Ecosystem

> **This is the original design document, kept as written.** The project is
> now called **Hearth**; *Wick* is the container spec and *Ember* is the
> family of formats, as [README.md](README.md#naming) explains. The build
> roadmap and open questions below have largely been answered — see
> [docs/WICK.md](docs/WICK.md) for what was actually built, and the
> [roadmap](README.md#roadmap) for what is left. This file is here because
> the reasoning that started the project is worth keeping, not because it
> describes the current state.

A family of file formats — text, documents, images, config, and tabular data — that all share one underlying container spec instead of each reinventing binary layout, versioning, and diffing from scratch.

`.embr` already exists as the effective-zip / archive replacement. This document specs out **everything else**: the shared spine those formats sit on, and the app that converts legacy files into the ecosystem.

---

## Naming

**Wick** — the spine specification. Every Ember format (`.emt`, `.emd`, `.emi`, `.emc`, `.emx`) is a Wick container with a different payload schema on top. "Wick" because it's the thin, shared core that everything else burns on — consistent with the Ember/EmberGuild naming, without colliding with `.embr`.

**Hearth** — the hub application. Converts legacy files (`.txt`, `.pdf`, `.png`, `.json`/`.yaml`/`.toml`, `.csv`) into their Ember equivalents and back, and serves as the universal viewer/diff/validate tool for the whole family. "Hearth" because it's the one place where all the Wick-based formats come together.

Naming chain: **Ember** (the project/brand) → **Wick** (the spec every format shares) → **Hearth** (the app that lights and reads them).

If you want alternatives: the spine could also be **Filament** or **Core**, and the hub could be **Forge** (ties more directly to EmberForge/EmberGuild) if you'd rather keep the fire metaphor tighter to your existing naming. Wick/Hearth is my recommendation because it reads clearly at a glance and doesn't overload "Forge," which you're already using for the IDE.

---

## 1. Design goals

Every format built on Wick must get these five properties for free, without reimplementing them:

1. **Self-describing** — a file carries its own schema/validation rules. No external `.schema.json` that can silently drift out of sync with the data it validates.
2. **Semantically diffable** — payloads are stored as a chunk tree, not a flat blob, so a diff tool reports *what changed in meaning*, not line-noise from reordered bytes.
3. **Provenance-aware** — an optional signed chain records which tool touched the file, when, and what changed. A file can prove its own edit history without trusting an external log.
4. **Tiered** — a file can hold a full-fidelity payload *and* a cheap summary/preview tier, so a consumer can ask for "the small version" without parsing the whole thing.
5. **Forward-migrating** — old files carry their own upgrade path. A new parser reading an old file (or an old parser hitting a newer file) fails predictably instead of silently misreading it.

---

## 2. Wick container format (the spine)

### 2.1 Byte layout

```
Offset   Size      Field
0x00     4 bytes   Magic: "WICK"
0x04     2 bytes   Format tag (ASCII, e.g. "MT" = .emt, "MD" = .emd, "MI" = .emi, "MC" = .emc, "MX" = .emx)
0x06     2 bytes   Spec version (major.minor, e.g. 0x0100 = v1.0)
0x08     4 bytes   Flags (bitfield — see 2.4)
0x0C     8 bytes   Chunk table offset (u64, byte offset from file start)
0x14     8 bytes   Chunk table length (u64, bytes)
0x1C     32 bytes  Content hash (BLAKE3 of full payload, post-header)
0x3C     —         Header ends here; chunk table + payload follow
```

Header is fixed-width and always readable in a single 60-byte read, so any tool can identify a Wick file, its format tag, and its version without touching the payload.

### 2.2 Chunk table

Everything after the header is a flat list of TLV (Type-Length-Value) chunks:

```
[chunk type: 4 bytes ASCII]  [length: u64]  [value: length bytes]
```

Reserved chunk types every Wick file may include, regardless of format-specific payload chunks:

| Type | Purpose |
|---|---|
| `SCHM` | Embedded schema/validation rules for this file's payload |
| `PROV` | Provenance chain (see 2.3) |
| `CAPS` | Capability/permissions header (see 2.5) — only relevant for executable-adjacent formats like `.emc` |
| `SUMM` | Cheap summary/preview tier (see 2.6) |
| `MIGR` | Embedded migration script/rules for upgrading this file from an older spec version |
| `DATA` | The actual full-fidelity payload — format-specific sub-chunks live inside this |

A format spec (`.emt`, `.emd`, etc.) defines what goes *inside* `DATA`; it never needs to redefine the outer chunk mechanics, versioning, or hashing.

### 2.3 Provenance chain (`PROV`)

A `PROV` chunk is a linked list of signed entries:

```
{
  "tool": "Hearth v0.3.1",
  "action": "converted from legacy .pdf",
  "timestamp": "2026-08-14T10:22:00Z",
  "prev_hash": "<blake3 of previous PROV entry, or null if first>",
  "signature": "<ed25519 signature over this entry + prev_hash>"
}
```

Each entry signs over the previous entry's hash, so the chain can't be truncated or reordered without breaking verification — same principle as a git commit graph or a blockchain, applied to a single file's edit history. This is the piece that overlaps with ChainTrace; a shared verification library between the two is worth planning for.

### 2.4 Flags bitfield

```
bit 0   — has provenance chain
bit 1   — has embedded capability header
bit 2   — has summary/preview tier
bit 3   — encrypted (payload chunks are ciphertext; see 2.7)
bit 4   — split-trust encryption (different chunks keyed differently)
bit 5-31 — reserved
```

A reader can decide what work is needed just from the flags, before touching the chunk table.

### 2.5 Capability header (`CAPS`) — for `.emc` primarily

```
{
  "network": false,
  "filesystem": ["read:./data", "write:./out"],
  "env_read": ["HOME"],
  "max_memory_mb": 256
}
```

Any Wick-aware runtime that executes or interprets a file's contents (relevant to `.emc` config-as-code and any future scripting format) must refuse to grant more than what's declared here. Declarative, not advisory — the runtime enforces it, the file doesn't ask nicely.

### 2.6 Tiered content (`SUMM`)

`SUMM` holds a compressed, lower-fidelity version of `DATA` — for text, a summary/outline; for images, a thumbnail + palette; for tabular data, a schema + row count + sample rows. A consumer that only needs "what is this file, roughly" reads `SUMM` and never touches `DATA` at all.

### 2.7 Encryption (split-trust)

When flag bit 5 is set, individual top-level chunks (not the whole payload) can be encrypted to different keys. A `.emc` file can hold non-secret config in plaintext chunks and secrets in per-environment-keyed chunks in the same file — same principle as Deadbolt's dual-passphrase design, applied per-chunk instead of per-volume.

### 2.8 Migration (`MIGR`)

Contains a small rule set (not arbitrary code — a declarative transform table) describing how to upgrade this file's `DATA` schema from its current version to the next. A Wick-aware reader encountering an old-version file applies `MIGR` rules in sequence rather than requiring the *reading tool* to know every historical schema.

---

## 3. Format family

| Extension | Replaces | Format-tag | What it adds on top of Wick |
|---|---|---|---|
| `.embr` | .zip/.tar | `AR` | *(already built)* content-addressed, deduplicated archive |
| `.emt` | .txt | `MT` | Enforced UTF-8, optional semantic sectioning (headings/code blocks) without Markdown's ambiguity |
| `.emd` | .pdf | `MD` | Reflowable by default, exact-layout pinning optional, no per-viewer rendering drift |
| `.emi` | .png/.jpg | `MI` | Raster + optional vector layer + edit history in one file; lossless region patching |
| `.emc` | .json/.yaml/.toml (config) | `MC` | Self-validating (embedded `SCHM`), capability-scoped, split-trust secrets |
| `.emx` | .csv/.json (tabular) | `MX` | Typed, unit-aware columns; unit-mismatched math fails loud instead of corrupting silently |

Build order recommendation: **`.emt` first.** It's the simplest payload schema, so it forces you to get the chunk table, provenance chain, and migration mechanics working before anything with harder payload semantics (image binary layout, PDF-style reflow engine) is on the table. Everything after that reuses the same spine library.

---

## 4. Hearth (the hub app)

### 4.1 Responsibilities

1. **Convert** — legacy format in, Ember format out, and back. `hearth convert report.pdf` → `report.emd`.
2. **View** — a universal viewer that reads the header + flags of any Wick file and renders the right way, without the user needing format-specific tools.
3. **Diff** — `hearth diff a.emx b.emx` walks the chunk trees and reports semantic changes, not byte diffs.
4. **Validate** — checks a file's `DATA` against its embedded `SCHM`, and its `PROV` chain signatures, in one pass.
5. **Migrate** — `hearth migrate old.emc` applies embedded `MIGR` rules to bring a file to the current spec version.

### 4.2 Architecture

Hearth itself should be thin — a CLI/GUI shell around one shared `libwick` core, plus a small plugin per format that only defines:

- the `DATA` sub-chunk schema for that format,
- a legacy-format importer (e.g. PDF → `.emd`),
- a legacy-format exporter (`.emd` → PDF, for compatibility with the outside world),
- a renderer (how to display it, for the `view` command).

This keeps the spine logic (chunk table, provenance, hashing, migration engine) in exactly one place, and adding a new Ember format later means writing one plugin, not a whole new app.

```
hearth/
├── libwick/              # spine: chunk table, header, provenance, hashing, migration engine
│   ├── header.rs
│   ├── chunks.rs
│   ├── provenance.rs
│   └── migrate.rs
├── plugins/
│   ├── emt/               # .emt <-> .txt
│   ├── emd/               # .emd <-> .pdf
│   ├── emi/                # .emi <-> .png/.jpg
│   ├── emc/               # .emc <-> .json/.yaml/.toml
│   └── emx/               # .emx <-> .csv/.json
├── cli/                    # `hearth convert|view|diff|validate|migrate`
└── gui/                    # optional viewer/editor shell
```

### 4.3 CLI sketch

```
hearth convert <file>              # auto-detects legacy format, outputs Ember equivalent
hearth convert <file> --to pdf     # export back to a legacy format
hearth view <file.emX>             # universal viewer
hearth diff <a.emX> <b.emX>        # semantic diff
hearth validate <file.emX>         # schema + provenance check
hearth migrate <file.emX>          # bring to current spec version
hearth verify-chain <file.emX>     # walk and verify the PROV signature chain
```

---

## 5. Build roadmap

1. `libwick` core — header parsing, chunk table read/write, BLAKE3 hashing, basic CLI that can identify any Wick file and dump its chunk table.
2. `.emt` plugin — simplest payload, proves the round-trip (`.txt` → `.emt` → `.txt`) and the diff tool.
3. Provenance chain — signing, verification, `hearth verify-chain`.
4. `.emc` plugin — schema embedding + capability header + split-trust encryption; this is the one with the most overlap with SentinelKit/Deadbolt work, worth prioritizing after `.emt`.
5. `.emx` plugin — typed/unit-aware tabular data.
6. `.emi` and `.emd` — held for last; both need real payload engines (image codec / reflow layout) rather than just structured data, so they're the highest-effort formats.
7. Hearth GUI — once at least three plugins exist, build the viewer shell on top of them.

---

## 6. Open questions before locking the spec

- **Hash algorithm**: BLAKE3 assumed above for speed; worth confirming against whatever ChainTrace already standardizes on, so provenance chains are verifiable with one shared library across projects.
- **Signature scheme**: Ed25519 assumed for `PROV` entries; same consideration.
- **Compression**: chunk-level (each `DATA` sub-chunk compressed independently, better for partial reads) vs. whole-payload (better ratio, worse for tiered/partial access). Chunk-level is recommended given the tiered-content goal in 2.6.
- **`.embr` interop**: since `.embr` predates this spec, worth deciding whether to retrofit it onto Wick (format-tag `AR`) for full ecosystem consistency, or keep it standalone and just make Hearth aware of both.
