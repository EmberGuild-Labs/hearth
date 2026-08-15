# The five formats

Every format here is a [Wick container](WICK.md) with a different payload
schema. This document describes what each one puts inside `DATA`, what it adds
over the format it replaces, and what it does not do.

Each is one crate, and each crate is small — because the container, the
hashing, the provenance chain, the encryption and the migration engine are
already written and shared.

| | Format | Replaces | Crate | The one thing it adds |
|---|---|---|---|---|
| <img src="../assets/emt-32.png" width="24"> | [`.emt`](#emt--text) | `.txt` | [`wick-emt`](../crates/wick-emt) | Structure that is stored, not guessed |
| <img src="../assets/emd-32.png" width="24"> | [`.emd`](#emd--documents) | `.pdf` | [`wick-emd`](../crates/wick-emd) | Reflow *and* pinned layout in one file |
| <img src="../assets/emi-32.png" width="24"> | [`.emi`](#emi--images) | `.png` | [`wick-emi`](../crates/wick-emi) | Tiles, so an edit is local and a diff is meaningful |
| <img src="../assets/emc-32.png" width="24"> | [`.emc`](#emc--configuration) | `.json`/`.yaml`/`.toml` | [`wick-emc`](../crates/wick-emc) | Split-trust secrets beside plaintext config |
| <img src="../assets/emx-32.png" width="24"> | [`.emx`](#emx--tables) | `.csv` | [`wick-emx`](../crates/wick-emx) | Arithmetic whose units are checked |

---

## `.emt` — text

**Tag** `MT` · **Imports** `.txt` `.text` `.log` `.md` `.markdown` `.rst` ·
**Exports** `.txt` `.md`

### What it adds over `.txt`

**Enforced UTF-8.** A `.txt` file has no encoding. It has whatever encoding the
writer happened to use, and every reader guesses. Import fails loudly, naming
the byte, rather than producing mojibake three tools downstream.

**Structure that is stored rather than guessed.** Each block is a chunk tagged
with what it is — heading, paragraph, code, list item, quotation. Markdown
expresses the same structure but only as a convention over characters: whether
`*text*` is emphasis or a literal asterisk depends on which of a dozen
implementations reads it. Here the question is answered once, at import, and
the answer is stored. Every later reader gets the same one.

**Section-level diffing.** Editing one paragraph of a long document changes one
chunk, so `hearth diff` names the paragraph.

### Exact round-trips

A section stores its text *and the exact bytes that separated it from the next
one*. Concatenating every section reproduces the source byte for byte —
trailing whitespace, mixed line endings, final newline or its absence.
Structure is an annotation layered over the original bytes, never a replacement
for them, so `txt → emt → txt` is an identity.

That is the property that makes the format adoptable without a leap of faith,
and it is why `.emt` rather than `.emd` is the default for `.md` and `.txt`
input.

### Payload

```
DATA
├── META   JSON: source name, whether structure was parsed, section count
└── SECT × n
        [kind u8][level u8][lang len u16][sep len u16][lang][sep][text]
```

`kind` is paragraph, heading, code, list, quote or rule. `level` is heading
depth or list nesting. `sep` is the exact bytes that followed the block.

`SUMM` holds an outline of the headings plus word count, character count and an
estimated reading time (at 238 wpm, the median adult silent-reading rate for
prose in Brysbaert's 2019 meta-analysis of 190 studies).

### What it does not do

Structure is only parsed for Markdown-shaped input. A `.txt` or a `.log`
becomes blank-line-separated paragraphs and nothing more, because plain text
does not assert anything else — a `#` at the start of a line in a log file is
not a heading, and guessing is exactly the ambiguity this format refuses.

---

## `.emd` — documents

**Tag** `MD` · **Imports** `.md` `.markdown` `.txt` `.pdf` ·
**Exports** `.pdf` `.md` `.txt`

### What it adds over `.pdf`

A PDF is a description of marks on a page. That is why it prints identically
everywhere, and why it is miserable to read on a phone, to search, to diff, or
to extract anything from: the document's *structure* was discarded when it was
produced, and everything downstream is trying to guess it back.

`.emd` stores the structure and treats layout as a rendering of it. Reflow to
any width for free.

**Then, when exact appearance is the point** — a contract, an invoice, a form —
the layout can be **pinned**. Every line's page, position, font and size is
computed once and stored in a `PINL` chunk. A pinned document renders from
those coordinates rather than from whatever layout engine happens to be reading
it, so it cannot drift between viewers or between versions of this tool. And it
still has its structure, so it is still searchable and still diffable.

Both halves live in the file at once. That is the thing a PDF cannot do.

```sh
hearth pin report.emd          # freeze the layout
hearth pin report.emd --undo   # back to reflowing
```

Validation catches a **stale pin** — a pinned layout that no longer covers
every block, which would render a document that is not the one stored. That is
worse than no pin at all, so it is an error rather than a warning.

### Payload

```
DATA
├── DMET   JSON: title, source, page size, import caveats
├── BLOK × n   JSON: {kind, level, lang, text}
└── PINL   JSON: [{block, page, x, y, font, size, text}]   (optional)
```

### PDF output

Written directly, using the base-14 fonts every PDF viewer is required to have,
so nothing is embedded and files stay small. Line breaking uses Adobe's
published Helvetica and Helvetica-Bold width tables; Courier is metrically
fixed at 600 units. Output is verified against a real renderer, not just
against itself.

### What it does not do

**PDF import is best-effort, and says so.** There is no reliable notion of a
paragraph in a PDF, headings are "text that happens to be larger", and reading
order is whatever order the generator emitted drawing operators in. Two-column
layouts, tables and figure captions come out interleaved. A scan comes out
empty, because there is no text in it to find.

Extraction walks the content streams, inflates the `FlateDecode` ones, and
collects the strings passed to the text-showing operators. What it had to guess
at is recorded in the file's `DMET` caveats and surfaced by `hearth validate`,
so the guesswork stays visible to whoever reads the result rather than being
quietly forgotten.

**Markdown export is regenerated, not reproduced.** Unlike `.emt`, `.emd` keeps
only the structure. A document that reflows has given up the right to claim
byte-exact round-trips, and pretending otherwise would be the dishonest option.
The structure *is* a fixed point — `parse(render(parse(x))) == parse(x)` — so
repeated conversions do not drift.

---

## `.emi` — images

**Tag** `MI` · **Imports** `.png` · **Exports** `.png`

### What it adds over `.png`

A PNG is one compressed stream. Change one pixel and the whole stream is
rewritten, so every version is an entirely new file and no tool can say what
changed without decoding both in full.

`.emi` stores the raster as a grid of 64×64 tiles, each its own chunk with its
own hash:

* **Lossless region patching.** Repainting a corner rewrites the tiles it
  touches, not the file.
* **A diff that means something.** Two versions are compared by tile hash, so
  `hearth diff` reports *where* the image changed in pixel coordinates, without
  decoding the tiles that match.
* **A cheap preview.** Thumbnail and palette live in `SUMM`, so a browser
  showing a directory of images reads a few kilobytes per file instead of
  decoding megapixels. `hearth thumbnail` never touches `DATA`, and neither
  does the macOS Quick Look preview, which embeds that same thumbnail.

A new one needs its dimensions — `hearth create canvas.emi --size 640x480`
gives a transparent canvas — because an image has no natural empty state and
every default size would be somebody's wrong one.

### Payload

```
DATA
├── IMHD   JSON: width, height, tile size, source, opaque
├── TILE × n   [tx u16][ty u16][w u16][h u16][RGBA8 rows]
├── VECT   an SVG overlay   (optional, stored and preserved)
└── EDIT × n   JSON edit-history notes   (optional)

SUMM
├── STAT   dimensions, megapixels, tile count
├── PALT   the eight most common colours and their shares
└── THMB   a real PNG thumbnail, at most 128px on its long edge
```

Validation checks that the tile grid covers the image exactly once — a gap or a
duplicate would decode to a plausible image with a stale or black region, which
is precisely the quiet corruption worth refusing.

### The trade

Pixels are stored as raw RGBA and compressed by the chunk layer, rather than as
PNG per tile. Raw because the container already compresses, and because a tile
stored decoded can be patched without a decode-modify-re-encode cycle.

The cost: on photographic content `.emi` lands within a few percent of the
equivalent PNG rather than beating it. On a synthetic 640×480 photo, 956 KB of
PNG becomes 921 KB — a 4% saving that is essentially noise. That is a real
trade, and it buys everything above.

### What it does not do

**JPEG import is declined, not half-implemented.** Decoding a lossy source into
a lossless container bakes in its artefacts and multiplies its size; storing
the stream verbatim would be a `.embr` archive wearing an image format's
extension. Neither is worth doing badly.

**Vector layers are stored, not rasterised.** A `VECT` chunk survives every
round trip, but an exported PNG is the raster layer alone.

---

## `.emc` — configuration

**Tag** `MC` · **Imports** `.json` `.yaml` `.yml` `.toml` ·
**Exports** `.json` `.yaml` `.toml`

### What it adds

Three things go wrong with `.json`, `.yaml` and `.toml` config, and none are
fixable inside those formats.

**1. The schema is somewhere else, or nowhere.** An external
`config.schema.json` drifts from the config it describes the first time someone
edits one without the other. `.emc` carries its rules in `SCHM`, covered by the
same content hash as the data, so they cannot separate.

**2. A config file is trusted with whatever the program is trusted with.**
Config decides what gets read, written and called out to, but declares none of
it. `.emc` carries a `CAPS` declaration, and a runtime that honours it refuses
to grant more than the file asked for. The file states its own blast radius.

```sh
hearth validate service.emc --policy policy.json
```

**3. Secrets and settings cannot live together.** The usual answer is two
files, one in a vault, and a deployment step that hopes they match. `.emc`
seals individual chunks to individual key slots:

```sh
hearth seal service.emc database.password --slot 1 --label prod
```

Public config stays readable without any passphrase. Holding the staging
passphrase reveals the staging secrets and nothing else. Rewriting the file
with only that passphrase leaves the production half byte-identical. And
exporting a config whose secrets are still sealed **fails**, rather than
writing one that is quietly missing a password.

Sealing is reversible — `hearth unseal service.emc database.password` — and a
whole config can be sealed at once with `--all`, which seals `SCHM` and `SUMM`
too. Without that, a file whose values are all encrypted still publishes their
names: the inferred schema lists every field, and the summary tier names the
top-level keys. What no amount of sealing hides is the container itself. The
header, the chunk table and the provenance chain are always plaintext, so a
stranger can always tell that a file is a `.emc`, how large its chunks are,
and who edited it when. Encrypting those would mean a format that cannot be
inspected at all, which is the property the whole spec exists to provide.

### Payload

```
DATA
└── NODE × n   [path segments][value]        one per leaf value

SECR × n       an encrypted group of NODEs, one per key slot
    ├── KEYL   the slot's label, so a locked reader can name what it lacks
    ├── SIDX   where each node sat in DATA, so unsealing restores the order
    └── NODE × n
```

Flat rather than nested, because that is what makes the diff readable:
comparing two lists of paths yields `database.port: 5432 → 5433`, where
comparing two trees yields "the database table changed" and leaves the reader
to hunt.

A path segment records whether it is a **key** or an **index**, so rebuilding
`servers.0.host` knows whether `0` addresses a list slot or a map key literally
spelled `0`. Storing the answer removes the guess.

### Fidelity

* **Key order is preserved.** A config that comes back alphabetised has been
  damaged even though every value survived.
* **Integers stay integers.** A port number is not `8080.0`, and a 64-bit id
  does not survive a trip through an `f64`.
* **Empty containers survive.** `plugins: []` is a statement; dropping it would
  turn it into an absent key.
* **TOML datetimes survive via the schema.** There is no datetime in the
  internal value model — adding one would push a single source language's type
  system into every format in the family — so a datetime is carried as a string
  whose `SCHM` field type is `datetime`, and exporting to TOML restores it.
  This is the embedded schema doing real work rather than only describing.
* **YAML complex keys are refused, not mangled.** YAML permits any node as a
  key; nothing else in the family does, and silently stringifying one would
  make the round trip lossy in a way nobody would notice.

### Migration operations

Beyond the spine's three: `rename_key`, `drop_key`, `set_default`. All take
dotted paths and apply to a prefix and everything beneath it.

---

## `.emx` — tables

**Tag** `MX` · **Imports** `.csv` `.tsv` · **Exports** `.csv` `.json`

### What it adds over `.csv`

CSV has no types. Every reader guesses, and the guesses differ: one tool
decides a column of `01`, `02`, `03` is numeric and drops the leading zeros,
another keeps them as text, and the two disagree forever after. JSON has types
but no units, which is the same failure one level up — a column of numbers
labelled `distance` is metres or miles depending on who you ask.

`.emx` writes both down, and then does something with them.

### Unit-mismatched arithmetic fails loud

A column can be **computed** — it stores a formula rather than values, declared
in the CSV header so a plain spreadsheet can round-trip it:

```
distance (km),elapsed (h),speed (km/h) = distance / elapsed
120,2,
90,1.5,
```

```sh
hearth recompute journey.emx     # 2 cells filled
```

The formula's units are checked against the column's declared unit
**symbolically, from the schema alone, with no rows involved**:

```
$ hearth validate broken.emx
error speed: formula "distance + elapsed": cannot add km [m] and min [s]
```

That error is available on an empty table — literally so, since a table can be
created with columns and no rows and still be checked:

```sh
hearth create journey.emx --columns "distance (km), elapsed (h), speed (km/h) = distance / elapsed"
```

`--columns` takes the same header syntax the CSV above uses, because there is
no reason to invent a second one. The alternative to a checked formula — a
plausible wrong number that survives into a report — is the failure mode the
format exists to remove.

Mixed scales convert rather than failing: `1 km + 500 m` is 1.5 km.

A unit is a scale factor and a map from base dimension to exponent. Symbols the
table does not know become their own base dimension, which is deliberate:
`USD`, `requests` and `bushels` are perfectly good units to check arithmetic
with. It also means `USD` and `EUR` are simply different dimensions — correct,
since there is no fixed conversion between them.

The expression language is numbers, column references, `+ - * / ^`, unary minus
and parentheses. No functions, no conditionals, no lookups: a formula that can
do arbitrary work is a formula nobody can check.

### Payload

```
DATA
├── COLS   JSON: [{name, type, unit, expr, doc}]
└── RGRP × n   [rows u32][cols u32][column-major cells]
```

Row groups hold up to 512 rows and are stored column-major, so like values sit
together and the chunk's compression finds the redundancy in a column of
timestamps or repeated categories. Grouping keeps a diff proportional to the
edit: a changed cell in a million-row table dirties one group.

```
$ hearth diff before.emx after.emx
~ row 1 · distance  3400 -> 3450
```

### Fidelity

* **Leading zeros stay text.** `007` is a part number, not seven. Any value
  whose text form is not what parsing it back would produce is treated as an
  identifier wearing digits.
* **An empty cell is null, not zero and not an empty string.** Collapsing that
  distinction is how a mean silently shifts, and an expression touching an
  empty cell yields empty rather than zero.

### Migration operations

`rename_column`, `set_unit`, `set_formula`. Units and formulas are parsed
before they are stored, so a migration cannot install one that does not
compile.

---

## Adding a sixth format

Implement [`libwick::Plugin`](../crates/libwick/src/plugin.rs) and add one line
to [`crates/hearth/src/registry.rs`](../crates/hearth/src/registry.rs).

Six methods are required — tag, extension, name, description, imports, exports
— plus `import`, `export` and `render`. Everything else has a default:

| Method | Default | Override when |
|---|---|---|
| `validate` | no issues | the format has rules `SCHM` cannot express |
| `diff` | the spine's structural walk | you can name the change better than "chunk 4 changed" |
| `summarize` | no summary tier | a cheap preview is possible |
| `migrate_op` | unhandled | the format needs its own migration operations |
| `schema_version` | 1 | the payload schema has moved |

A plugin never touches the header, the chunk table, hashing, provenance,
encryption or the migration engine. Those are the same for every format, and
reimplementing them per format is exactly the failure the spine exists to
prevent.
