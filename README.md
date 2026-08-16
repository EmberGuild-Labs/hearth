<img src="assets/hearth.png" alt="Hearth" width="128" align="right">

# Hearth

Five file formats — text, documents, images, config and tables — that share one
container instead of each reinventing binary layout, versioning, integrity and
diffing from scratch.

**Hearth** is the project and the command. **Wick** is the container spec all
five formats share. **Ember** is the family of formats built on it, which is
where the `em` in `.emt`, `.emd`, `.emi`, `.emc` and `.emx` comes from. See
[Naming](#naming).

```sh
hearth convert report.md            # -> report.emt
hearth create notes.emt             # or start an empty one
hearth edit notes.emt               # opens in $EDITOR, comes back in
hearth view readings.emx --summary  # what is this file, without reading it
hearth diff before.emc after.emc    # database.port  5432 -> 5433
hearth validate journey.emx         # unit-checked arithmetic
```

On macOS, `macos/build-app.sh` gives the five formats their own Finder icons,
a Quick Look preview read from the summary tier, and a window that edits a
file and saves it back through the same round trip `hearth edit` uses.

[Wick specification](docs/WICK.md) · [The five formats](docs/FORMATS.md) ·
[Original design document](EFE.md)

<img src="assets/family.png" alt="Wick, Hearth, .emt, .emd, .emi, .emc, .emx" width="100%">

---

## Why one container

Every file format solves the same five problems, badly and separately. A file
should be able to carry its own validation rules, prove its own edit history,
be compared by meaning rather than by bytes, offer a cheap preview, and know
how to upgrade itself. Almost none do, because doing it is a project in itself
and nobody does it five times.

So it gets done once. Every Ember format is a Wick container with a different
payload, and gets all five for free:

| Property | How | What it buys |
|---|---|---|
| **Self-describing** | a `SCHM` chunk | validation rules travel with the data, so they cannot drift out of sync with it |
| **Semantically diffable** | payload is a chunk tree | a diff reports what changed in meaning, not line noise |
| **Provenance-aware** | a signed, hash-linked `PROV` chain | a file can prove its own history without an external log |
| **Tiered** | a `SUMM` chunk read past `DATA` | "what is this file, roughly" without parsing the payload |
| **Forward-migrating** | a `MIGR` rule table | old files carry their own upgrade path; readers fail predictably |

The formats themselves are then small. `wick-emt` is under 800 lines including
its tests, because everything hard was already written.

| | Format | Replaces | The one thing it adds |
|---|---|---|---|
| <img src="assets/emt-32.png" width="22"> | `.emt` | `.txt` | Structure that is stored, not guessed. Byte-exact round-trips. |
| <img src="assets/emd-32.png" width="22"> | `.emd` | `.pdf` | Reflow *and* pinned layout, in one file. |
| <img src="assets/emi-32.png" width="22"> | `.emi` | `.png` | Tiles, so an edit is local and a diff says *where*. |
| <img src="assets/emc-32.png" width="22"> | `.emc` | `.json`/`.yaml`/`.toml` | Secrets sealed beside plaintext config. |
| <img src="assets/emx-32.png" width="22"> | `.emx` | `.csv` | Arithmetic whose units are checked. |

`.embr`, the archive format that replaces `.zip`, is a
[separate project](https://github.com/EmberGuild-Labs/embr) and predates this
spec. See [Roadmap](#roadmap).

---

## Install

**What you need.** [Rust](https://rustup.rs) **1.85 or newer** — the
encryption crates are edition 2024 and will not build on anything older. That
is the only requirement. There are no system libraries to install, no C
toolchain beyond what Rust already brings, and nothing to configure
afterwards.

Linux and macOS are built and tested on every commit. Windows is not — the
code has no deliberate Unix dependency beyond the signing key's file mode,
which is already behind a `cfg`, but nothing verifies that and it is not a
claim worth making until something does. The application in [macOS
integration](#macos-integration) is macOS-only by nature.

If you do not have Rust:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
exec $SHELL                       # pick up the new PATH
rustc --version                   # 1.85.0 or newer
```

Already have it, but older? `rustup update stable`.

**Install `hearth`.** There is no package on crates.io or Homebrew yet, so it
is built from source. This takes a couple of minutes the first time:

```sh
git clone https://github.com/EmberGuild-Labs/hearth.git
cd hearth
cargo install --path crates/hearth
```

**Check it worked.**

```sh
hearth --version                  # hearth 0.1.0
hearth formats                    # the five formats this build reads and writes
```

If `hearth: command not found`, the install directory is not on your `PATH`:

```sh
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.zshrc   # or ~/.bashrc
exec $SHELL
```

**Try it on a real file.** Nothing is created outside the directory you are
in, and the original is never touched:

```sh
cd examples
hearth convert notes.md           # -> notes.emt
hearth info notes.emt             # what it is, without decoding the payload
hearth view notes.emt             # what is in it
hearth convert notes.emt back.md  # and back out again
diff notes.md back.md             # byte-identical
```

[Try it](#try-it) below walks through the rest.

### Optional: sign what you edit

Every change a file goes through is recorded in its provenance chain whether
or not you do this. Signing adds *who*, so a reader can verify the chain
against your public key rather than just checking it is unbroken:

```sh
hearth key generate               # writes ~/.config/hearth/identity.key
hearth key show                   # the public half, safe to share
```

The key is a file with mode `600`. It is yours, it never leaves your machine,
and nothing prompts for it. See [Provenance and
signing](#provenance-and-signing).

### Optional: the macOS application

Gives the five formats their own Finder icons, a Quick Look preview, and a
window that opens and edits a file — including a spreadsheet grid for
tables:

```sh
./macos/build-app.sh              # installs to /Applications
./macos/build-app.sh ~/Applications
./macos/build-app.sh --uninstall
```

It needs the Xcode command line tools (`xcode-select --install`). See [macOS
integration](#macos-integration) for what it installs and why each part is
needed.

### Working on Hearth itself

Skip the install and run from the build directory:

```sh
cargo build --workspace
cargo run -p hearth -- convert examples/notes.md /tmp/notes.emt
cargo run -p hearth -- info /tmp/notes.emt

cargo test --workspace                              # 253 tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
./macos/test-grid.sh                                # macOS only
python3 assets/make_art.py                          # icons are generated
```

CI runs all of those on Linux and macOS. [CLAUDE.md](CLAUDE.md) is the short
version of how the code is organised and what the conventions are.

### Uninstalling

```sh
cargo uninstall hearth
./macos/build-app.sh --uninstall      # if you installed the app
rm -rf ~/.config/hearth               # if you generated a signing key
```

Uninstalling does not touch any `.em*` files you made. They are self-contained
and any future build reads them.

### If something goes wrong

| Symptom | Cause |
|---|---|
| `edition 2024 is required` or a wall of parse errors while building | Rust older than 1.85 — `rustup update stable` |
| `hearth: command not found` after installing | `~/.cargo/bin` is not on your `PATH`, see above |
| `not a Wick file` | the file is not an Ember file, whatever its extension says |
| `content hash mismatch` | the file was damaged after it was written; `hearth info` still reports its header |
| `file is Wick spec v…, this build understands v…` | run `hearth migrate <file>` |
| Finder shows blank icons after installing the app | Finder's icon cache; log out and back in |
| Quick Look shows nothing | enable Hearth under System Settings → General → Login Items & Extensions → Quick Look |
| macOS refuses to open the app | it is unsigned; right-click it in Finder and choose Open once |

---

## Try it

The [`examples/`](examples) directory has one input per format.

```sh
cd examples

hearth convert notes.md              # -> notes.emt
hearth convert service.json          # -> service.emc
hearth convert readings.csv          # -> readings.emx
hearth convert journey.csv           # -> journey.emx, with a computed column
```

**Look at a file without decoding it.** `hearth info` reads the header and the
chunk offsets only, so it costs the same on a one-gigabyte file as on a small
one:

```
$ hearth info notes.emt
format:    MT (.emt, text)
spec:      Wick v1.0
flags:     0x00000005  provenance, summary
table:     4 chunks, 1005 B at offset 60
hash:      561d40fa1b7cff25…

chunks:
  DATA       410 B  at 72         full-fidelity payload
  SCHM       152 B  at 494        embedded schema
  SUMM       230 B  at 658        summary / preview tier
  PROV       165 B  at 900        provenance chain

payload:   one compressed stream, too small in its parts to be worth reading separately

provenance: 1 entries, 0 signed
  latest: 2026-08-14T22:23:29Z — converted from legacy .md (Hearth v0.1.0)
```

On a file whose payload *is* worth seeking into — a table, an image — that
last line becomes an index instead, and `hearth view --limit` uses it to read
one row group rather than a file. See
[Reading part of a big file](#reading-part-of-a-big-file).

**Read the cheap tier.** `--summary` renders from `SUMM` and never touches the
payload:

```
$ hearth view notes.emt --summary
10 sections, 67 words, about 0.3 min to read

Measurements taken on the north ridge, 14 August. The wind was steady

outline:
  Field Notes
    Method
```

**Diff by meaning.**

```
$ hearth diff service.emc service-v2.emc
~ database.port  5432 -> 5433
- debug  was false
+ verbose  = true
```

Not "bytes 412–487 changed". `hearth diff` exits 1 when there are differences,
so it composes in scripts the way `diff` does.

**Check a file against its own rules.**

```
$ hearth validate journey-broken.emx
content hash verified
provenance chain intact: 1 entries, 1 signed
error speed: formula "distance + elapsed": cannot add km [m] and min [s]
hearth: 1 error(s) — the file does not satisfy its own rules
```

That error came from the schema alone. No rows were needed to find it.

**Start something new, and edit it.** `hearth create` writes an empty file of
any format, and `hearth edit` opens one in your editor:

```
$ hearth create journey.emx --columns "distance (km), elapsed (min), speed (km/min) = distance / elapsed"
journey.emx  (table, 913 B)

$ hearth edit journey.emx
editing journey.emx as .csv in vim ($EDITOR)
journey.emx  (2 change(s))
+ rows 0..2  2 rows appended
~ column distance  type string -> int
```

An edit goes out as the legacy format the file round-trips through, comes back
in through the ordinary importer, and replaces the payload *inside the
container it started in* — so the provenance chain gains a link rather than
starting again, and the capability declaration and migration rules survive.
`hearth edit --to txt` picks a different dialect when the format has one.

The two halves are separable, which is how the macOS window edits a file
without reimplementing any of this:

```sh
hearth edit notes.emt --export /tmp/draft   # prints the dialect: md
hearth edit notes.emt --from /tmp/draft     # ...and puts it back
```

**Round-trip anything back out.**

```sh
hearth convert notes.emt back.md        # byte-identical to notes.md
hearth convert notes.md report.emd      # the same source, as a document
hearth convert report.emd --to pdf      # a real PDF
```

Naming the output is enough — its extension says which format you meant, in
either direction. `--to`, `--as` and `-o` are still there for when the name
does not say, and a name that contradicts a flag is an error rather than a
coin toss.

---

## Commands

```
hearth convert <file>                  legacy in, Ember out
hearth convert <file> <output>         the output's extension picks the format
hearth convert <file> --to pdf         Ember out, legacy in
hearth convert <file> --as emd         when two formats could take the input
hearth create <file> [--edit]          start an empty file of any format
hearth edit <file> [--to ext]          edit it in $EDITOR, container intact
hearth open <file>                     edit it in a window (macOS)
hearth view <file> [--summary]         universal viewer
hearth preview <file> [--full]         a one-page HTML preview; what Quick Look shows
hearth diff <a> <b>                    semantic diff; exit 1 if they differ
hearth validate <file> [--policy p]    schema, rules, capabilities, provenance
hearth migrate <file> [--dry-run]      apply the file's own MIGR rules
hearth verify-chain <file>             walk and verify the PROV signatures
hearth info <file>                     header and chunk table, no payload read
hearth formats                         what this build handles
hearth key generate | show             the identity used for signing
hearth rules show | set <file> <r>     read or attach migration rules
hearth seal <file> <paths...>          move config values into a key slot
hearth seal <file> --all               seal the whole config, schema included
hearth unseal <file> <paths...>        bring sealed values back out
hearth get <file> <path>               print one config value
hearth set <file> <path> <value>       set one config value, no editor
hearth unset <file> <path>             remove one, or a subtree
hearth pin <file.emd> [--undo]         freeze or release a document's layout
hearth recompute <file.emx>            re-evaluate computed columns
hearth thumbnail <file.emi>            preview PNG, from the summary tier
```

`--help` on any of them for the full set of options.

### Scripts and agents

Every reporting command takes `--json`, `-` means stdin and stdout, nothing
ever waits for a human, and data goes to stdout while everything else goes to
stderr:

```sh
hearth info notes.emt --json | jq .provenance.entries
hearth set service.emc listen.port 9090          # one value, no round trip
cat notes.md | hearth -q convert - --src md -o - > notes.emt
hearth validate service.emc --json | jq -e .ok   # exit 1 if it fails its rules
```

[docs/AGENTS.md](docs/AGENTS.md) is the full contract — output shapes, exit
statuses, what refuses and why, and where these formats are *not* worth using.

### Reading part of a big file

A `DATA` payload made of large parts — a table's row groups, an image's tiles
— keeps each part independently compressed, so any one of them can be read on
its own. `hearth info` shows the index, and `hearth view --limit` uses it:

```sh
hearth info big.emx --json | jq '.payload.children[1]'
# {"type": "RGRP", "offset": 322, "length": 3566}

hearth view big.emx --limit 5     # decodes one row group, not three million rows
```

On a 20 MB `.emx` holding three million rows, mean of ten runs:

| | time | peak memory |
|---|---:|---:|
| `hearth info` | 0.032s | 3.4 MB |
| `hearth view --limit 5` | 0.033s | 3.9 MB |
| `hearth view --summary` | 0.033s | 3.5 MB |
| `hearth preview` (Quick Look) | 0.033s | 3.6 MB |
| whole-file read (`hearth validate`) | 1.040s | 871 MB |

The footer still says `… 2999995 more rows`, because the count comes from the
file's summary tier rather than from the part that was read. The content hash
is verified either way — it covers the whole chunk table, so checking it costs
a pass over the file. What a partial read saves is decompression and memory,
not I/O.

A payload of many *small* parts is stored as one compressed stream instead,
because that is how it compresses. `hearth info --json` says so
(`"addressable": false`) rather than inventing an index, and such a payload is
small enough that seeking into it would save nothing anyway.

### Provenance and signing

Every write records what touched the file, when, and what it did. Entries are
hash-linked, so removing or reordering one breaks every link after it.

Signing is optional and off by default — Hearth will not create an identity
behind your back. To turn it on:

```sh
hearth key generate      # writes ~/.config/hearth/identity.key, mode 0600
```

```
$ hearth verify-chain report.emt
  0  signed    2026-08-14T21:59:58Z  converted from legacy .md
      by Hearth v0.1.0
      key 672835c2f3534363…

chain intact: 1 entries, 1 signed by 1 key(s), 0 unsigned
```

An unsigned chain is reported as unsigned, never as valid. It is still
tamper-evident; it just cannot say *who*.

For CI, set `$HEARTH_KEY` to a hex secret key, or `$HEARTH_KEY_FILE` to a path.

### Split-trust config

```sh
hearth seal service.emc database.password --slot 1 --label prod
```

Public config stays readable by anyone. The sealed value needs the passphrase:

```sh
hearth view service.emc                      # everything except the password
hearth view service.emc --unlock 1           # everything
hearth convert service.emc --to json         # refuses: a secret is sealed
```

Rewriting the file while a slot is locked leaves that slot byte-identical, so a
tool holding one passphrase can edit a file without damaging the half it cannot
read. For scripts, `--passphrase-env VAR` avoids the prompt.

Sealing is reversible, which is what stops it being a trap:

```sh
hearth unseal service.emc database.password --slot 1
```

Each sealed value remembers where it sat in the config, so unsealing puts it
back in its own place rather than at the end — `seal` then `unseal` exports
byte-for-byte the same file it started as, key order included.

`--all` seals every value **and the schema and summary tier with them**. That
distinction matters: sealing only the values leaves `SCHM` listing every field
name and `SUMM` naming the top-level keys, so a file full of sealed secrets
would still announce that it has a `database.password`. What remains readable
after `--all` is the header, the chunk table and the provenance chain — that
it is a `.emc`, how big each chunk is, and who edited it when. The container
is never encrypted, only its contents.

---

## macOS integration

```sh
./macos/build-app.sh              # installs to /Applications
./macos/build-app.sh --uninstall  # and removes it again
```

That builds **Hearth.app**, which started out existing for one reason: macOS
will not attach an icon to a bare file extension. The icon has to be exported
by an installed application that declares the type through a Uniform Type
Identifier, so the bundle declares all five — `xyz.ember.emt` through
`xyz.ember.emx` — each with the icon from `assets/`.

Given a bundle, two more things follow:

**A window.** Double-click a file — or run `hearth open <file>` — and it
opens in Hearth: the payload as editable text, the container's facts above
it, Save and Revert below. ⌘N makes a new one, asking for the format and for
whatever that format cannot invent — columns for a table, a size for an
image — and creating it with `hearth create`. Saving runs `hearth edit --from` on the file, the
same code path a terminal edit takes, so a save from the window keeps the
provenance chain, refuses a file with sealed values, and rebuilds the summary
tier. None of that logic is written twice; the window is a view onto the
tool, not a second opinion about what an Ember file is. Images open
read-only, because this app has no pixel editor and a Save button that cannot
work is worse than none.

**A grid for tables.** A `.emx` opens as a spreadsheet rather than a wall of
commas: the unit in each column heading, `ƒ` on the computed ones, numbers
right-aligned, rows you can add and delete. Type into a cell and it is
checked against the column's declared type before it is accepted — `abc` in
an `int` column is refused in the status line and the old value goes back,
rather than being saved and rejected later.

It loads and saves through `hearth convert --to json` and `hearth edit --to
json --from`, not CSV, and the reason is types. A CSV round trip re-infers
every column from its values, so a `sample_id` column of `0071, 0072` comes
back as the numbers 71 and 72. The JSON form carries the declared type, the
unit and the formula, so the grid shows the column the file already declared
and hands back the same one. (`.emx` could export JSON but not read it until
this needed it; now `hearth edit --to json` round-trips from the terminal
too.)

Past 50,000 rows the window says so instead of opening: a grid holds every
row in memory and a save rewrites the whole file, so there is a size where
`hearth view --limit` is simply the right tool. `macos/test-grid.sh` checks
the part of this that cannot be checked by looking at it — which typed cells
are accepted, and whether a saved table is the table that was loaded.

**Quick Look.** Press space on a file and the preview is rendered from its
`SUMM` chunk. This is the tier's whole reason for being: Finder asking "what
is this" about a file the user may never open, answered without decoding a
payload. The extension is a thin shell around `hearth preview`, so the pane
and the terminal show the same thing and either can be debugged from the
other. A `.emi` preview carries its picture with it — a Quick Look extension
is sandboxed and cannot fetch anything, so the image is embedded in the page,
and it is the raster itself rather than the summary thumbnail. That one
decode is the exception to the paragraph above, and the page says so: a
thumbnail stretched to fill a pane is a blurry answer to a question the file
can answer sharply.

The three parts of this that are not obvious, all of them learned by watching
it fail:

- **The bundle must be re-signed after its `Info.plist` is written.** macOS
  ignores type declarations from a bundle whose signature does not verify, so
  signing is the last step of the build rather than part of compiling.
- **The outer signature must not be `--deep`.** A deep re-sign re-signs the
  Quick Look extension too and strips the sandbox entitlement it needs,
  leaving a bundle that looks correctly signed and silently never previews.
- **An app's type declarations stay "untrusted" until it has run once**, and
  an untrusted declaration's icon is ignored. The script launches the app and
  quits it, then checks `lsregister -dump` and says so if the declaration did
  not take.

If the icons do not appear immediately, log out and back in. Finder's icon
cache is the slowest part of this and nothing else forces it.

---

## Measured results

Sizes are what they are, so here they are. Ember files include the schema,
provenance chain and summary tier; the legacy files include none of that.

| Input | Legacy | `gzip -9` | Ember | vs raw | vs gzip |
|---|---:|---:|---:|---:|---:|
| 719 B of prose | 719 B | — | 901 B | **+25%** | — |
| 52 KB of prose | 52,042 | — | 7,270 | −86% | — |
| 1.1 MB of prose | 1,097,399 | 107,467 | 131,328 | −88% | +22% |
| 322 B of JSON config | 322 B | — | 725 B | **+125%** | — |
| 13 KB of JSON config | 13,156 | 1,386 | 3,271 | −75% | +136% |
| 542 KB CSV, 20k rows | 542,335 | 168,875 | 107,059 | −80% | **−37%** |
| 11 KB PNG swatch | 11,276 | — | 2,217 | **−80%** | — |
| 1.8 MB PNG photograph | 1,827,670 | — | 1,859,322 | +2% | — |

The two image rows replace a single "956 KB synthetic photo" measured before
tiles were delta-filtered, whose input is not in the repository and so could
not be re-run. The swatch is [`examples/swatch.png`](examples/swatch.png), so
that row can be.

### Where this costs you

**Small files get bigger.** A Wick file carries a schema, a provenance chain
and a summary tier, and those have a floor of roughly 600 bytes. Under about
2 KB that floor dominates and the file grows. A 322-byte config becomes 725
bytes. If you have a directory of tiny files and no use for any of the five
properties, this format is not for you.

**Config loses to gzip.** `.emc` stores each leaf value with its full dotted
path, which is verbose — and is exactly what makes the diff readable. That is
the trade, taken deliberately.

**Images are about the size of the PNG, either way.** `.emi` stores pixels,
delta-filters each tile and lets the container compress it, which lands
anywhere from 80% smaller on flat artwork to a few percent larger on a
photograph. What it never gets is the cross-image context a single PNG stream
has, because a tile is compressed alone — which is exactly what buys
tile-level patching and a diff that says *where*.

**Tables genuinely win.** Column-major row groups give zstd a column of similar
values to work with, which is why `.emx` beats `gzip -9` by 37% on a real
table.

Nothing here is a compression project. Every format that gets smaller does so
because a general-purpose compressor was pointed at better-organised bytes.

---

## Project layout

```
crates/libwick/         the spine — the whole of Wick
  src/header.rs         the 60-byte header
  src/chunks.rs         TLV records, compression, per-chunk encryption
  src/provenance.rs     hash-linked, Ed25519-signed history
  src/schema.rs         embedded validation rules
  src/caps.rs           capability declarations and enforcement primitives
  src/migrate.rs        the declarative migration engine
  src/diff.rs           structural diff over the chunk tree
  src/crypto.rs         XChaCha20-Poly1305, Argon2id, Ed25519
  src/plugin.rs         the interface a format implements
  src/file.rs           whole-file, partial and Peek reads; atomic writes

crates/wick-emt/        .emt <-> .txt, .md
crates/wick-emd/        .emd <-> .pdf, .md, .txt   (+ a PDF writer and reader)
crates/wick-emi/        .emi <-> .png
crates/wick-emc/        .emc <-> .json, .yaml, .toml
crates/wick-emx/        .emx <-> .csv, .json
  src/units.rs          units as dimension vectors
  src/expr.rs           computed-column expressions, checked symbolically

crates/hearth/          the `hearth` binary
  src/registry.rs       which plugin handles which file
  src/preview.rs        the one-page preview Quick Look renders
  tests/cli.rs          end-to-end tests against the real binary

macos/build-app.sh      Hearth.app: file types, icons, window, Quick Look
  app/main.swift        the window that views and edits a file
  quicklook/            the preview extension, ~100 lines of Swift
assets/make_art.py      every icon, as editable text
docs/WICK.md            the container specification
docs/FORMATS.md         the five payload schemas
docs/AGENTS.md          the contract for scripts and agents
examples/               one input per format
```

### Tests

```sh
cargo test --workspace     # 253 tests
```

The unit tests check each layer. `crates/hearth/tests/cli.rs` checks the thing
a user actually runs — process in, files out, exit status — because round-trip
fidelity is only meaningful measured through the whole pipeline. It also
converts every file in `examples/`, so a broken example fails the build.

### Icons

Every mark is pixel art on a 16×16 grid, defined as plain text in
[`assets/make_art.py`](assets/make_art.py). Edit a row, re-run the script, and
every PNG, SVG and icon size regenerates:

```sh
python3 assets/make_art.py
```

Two candidates were drawn for each of the seven subjects. They are all in
`assets/candidates/`, with a side-by-side comparison per subject; change a
`chosen` in the script to swap one. The 16×16 grid is not decoration — it is
the same grid a favicon and a macOS icon use, so every exported size is a
whole-number multiple with no resampling anywhere. That is what makes
`hearth.iconset` and one `.iconset` per format fall out of the same script
that draws the README's artwork.

The palette is EMBR's ember colours plus one accent hue per format. The accent
tells you which format at a glance; the fire tells you whose.

---

## Decisions made along the way

**One container, five payloads.** The alternative — five formats that each
grow their own versioning and integrity — is how the current situation
happened. The test of the design is whether adding a format is cheap, and it
is: implement one trait, add one line to the registry.

**Structure is an annotation, not a replacement.** `.emt` stores the exact
bytes that separated each block, so `txt → emt → txt` is a byte-for-byte
identity. A format nobody trusts to give their data back is a format nobody
adopts, and "trust us" is not an argument. Where a format genuinely cannot make
that promise — `.emd`, which reflows — it says so instead of pretending.

**Compression is per chunk, and where it happens depends on size.** Small
children are compressed together, because a 400-byte paragraph alone gives a
compressor nothing to work with; large children are compressed individually and
stay independently decodable. Getting this wrong the first time cost 3× on
prose, which is why the rule is now measured rather than assumed.

**Editing is a round trip, not a second writer.** `hearth edit` exports to a
legacy format, opens that, and re-imports it. The alternative — a payload
editor per format — would be five more implementations able to write a file,
and the fifth would eventually write one the reader disagrees with. The cost
is that a format can only be edited through a dialect it both exports *and*
imports, and that the plugin has to choose which: a `.emt` that came from
Markdown goes back out as Markdown, and a `.emd` refuses to make the round
trip through PDF even though it can write one.

**Creating goes through the importer too.** `hearth create` asks the plugin
for starter content in a legacy format and converts it, rather than
assembling chunks directly. One code path builds every file, so a created
file cannot differ from a converted one in a detail nobody thought to
compare. Where a format cannot describe a new document on its own — a table
has no columns, an image has no size — it says so instead of guessing.

**A locked chunk is not an error.** Reading the public half of a split-trust
file is the ordinary case, not a failure. Sealed chunks the reader cannot open
pass through byte-for-byte on the next write.

**Unsigned is reported as unsigned.** A tool that treats a missing signature as
a pass has removed the only thing signing was for.

**Unknown units are their own dimension.** `USD`, `requests` and `bushels` are
perfectly good units to check arithmetic with. Refusing them would make the
feature useless for the data people actually have, and it correctly makes `USD`
and `EUR` incompatible.

**Rules are data, never code.** A migration rule set renames, drops and inserts.
It cannot loop, branch or read the filesystem, because opening an untrusted
file should not be interesting.

**Rust.** Best-in-class zstd, BLAKE3 and RustCrypto bindings, one static
binary, and it compiles to WebAssembly — which a browser-based viewer will
need.

---

## Roadmap

Ordered by payoff ÷ effort.

**Next**

- [ ] **`.embr` interop.** `AR` is reserved for it, but `.embr` predates this
      spec and has not been retrofitted onto Wick. Decide: retrofit for full
      ecosystem consistency, or keep it standalone and teach Hearth both.
- [x] **Partial reads within `DATA`** — `hearth info` reports the sub-chunk
      index and `hearth view --limit` decodes only the row groups it will
      show. On three million rows that is 4 MB of memory instead of 871 MB.
      See [Reading part of a big file](#reading-part-of-a-big-file). Writing
      the file back is refused rather than silently dropping the rest.
- [ ] **A shared verification library with ChainTrace.** The provenance chain
      is the piece that overlaps. Whether the primitives already match is
      unresolved.
- [ ] **WASM viewer.** A `.emt` you email someone is a brick unless they can
      open it. Same Rust, `wasm-pack`, a static page. A format nobody can open
      dies.

**Then**

- [ ] **A Hearth GUI** beyond the macOS window — the grid a `.emx` gets is
      what the other four formats still want.
- [x] **macOS integration** — registered file types, Finder icons and Quick
      Look previews driven by `SUMM`. See
      [macOS integration](#macos-integration).
- [ ] **Richer `.emd` layout** — tables, images, embedded fonts.
- [ ] **`.emi` vector rasterisation**, so a `VECT` layer renders rather than
      merely surviving.
- [ ] **Streaming writes**, so a large table need not be built in memory first.

**Known limitations**

- Small files pay a fixed metadata overhead; see
  [Where this costs you](#where-this-costs-you).
- PDF import is best-effort text extraction and says so in the file it
  produces. Scanned PDFs yield nothing.
- `.emi` reads PNG only. JPEG is declined deliberately, not pending.
- `.emd` markdown export is regenerated rather than reproduced.
- Reading is partial, writing is not. `hearth view`, `hearth preview` and
  `hearth info` decode only what they display; every command that produces a
  file still builds the whole payload in memory first. See **Streaming
  writes** above.
- **v1 is unstable.** The format may change without a compatibility path until
  it is declared frozen.

---

## Naming

Three names, for three things that can be adopted separately.

**Hearth** is the project, and the command you type. It is the front door, so
it is what the whole thing is called.

**Wick** is the container specification — the thin core everything else burns
on. It has its own name because it could have its own second implementation:
someone writing a reader in another language implements Wick, not Hearth.
[docs/WICK.md](docs/WICK.md) is normative and is written to be implementable
without reading any of this code.

**Ember** is the family of formats built on Wick, and the `em` in every
extension. A sixth format would join the family without changing either of
the other two names.

## License

MIT
