# Working in this repository

Hearth: five file formats (`.emt`, `.emd`, `.emi`, `.emc`, `.emx`) that share
one container (Wick), plus `hearth`, the tool that reads all of them. Hearth
is the project, Wick is the container spec, Ember is the format family — see
the Naming section of README.md. Rust workspace, no runtime dependencies
beyond the crates in `Cargo.toml`.

Start with [README.md](README.md) for what the project is,
[docs/WICK.md](docs/WICK.md) for the container spec, and
[docs/FORMATS.md](docs/FORMATS.md) for what each format puts in `DATA`.

## Driving hearth from a script or an agent

Read **[docs/AGENTS.md](docs/AGENTS.md)** first. It documents the machine-facing
contract: `--json` on every reporting command, data on stdout and everything
else on stderr, exit statuses, `-` for stdin and stdout, and the surgical
edit commands (`get`, `set`, `unset`) that avoid a whole-file round trip.

## Which files should be Ember files

Use them where one of the five properties pays for itself:

* **`.emc`** for configuration that has secrets in it — `hearth seal` keeps
  them in the same file as the public half, and an export refuses rather than
  writing a config quietly missing a password.
* **`.emx`** for tabular data with units — the arithmetic is checked
  symbolically, before any rows exist.
* **`.emd`/`.emt`** for documents that need to prove their own history.

**Keep source code, READMEs and anything a person edits in a plain-text
editor as plain text.** Every other tool in the world already reads it, and
`git diff` on a container says nothing useful. A format that is not pulling
its weight is just an extra step between you and the data.

## Conventions to follow when changing this code

* **The spine owns the container.** Header, chunk table, hashing, provenance,
  encryption and migration live in `crates/libwick`. A format plugin describes
  its own payload and nothing else. If a change needs a plugin to know about
  the header, the change is in the wrong place.
* **One implementation.** The macOS app and the Quick Look extension shell out
  to the `hearth` binary rather than reimplementing anything. Keep it that way:
  a second opinion about what an Ember file is will drift.
* **Refuse rather than guess.** Where the tool cannot know something — a new
  table's columns, what bytes on stdin are, which format an ambiguous name
  wants — it says so. Adding a plausible default is how a file ends up quietly
  wrong.
* **Say what is true.** "Sealed" is not "missing"; "unsigned" is not "valid".
  Several error messages exist specifically to keep those apart.
* **Comments explain the decision, not the syntax.** The interesting comments
  in this codebase say why something is the way it is, usually because the
  obvious alternative was tried and failed.

## Building and checking

```sh
cargo build --workspace
cargo test --workspace          # 253 tests; crates/hearth/tests/cli.rs is end-to-end
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
python3 assets/make_art.py      # icons are generated; CI checks they match
```

`crates/hearth/tests/cli.rs` runs the real binary in a scratch directory with
its own signing key. Anything user-visible — a new command, a changed message
somebody might depend on — belongs there.

## macOS

`macos/build-app.sh` installs `Hearth.app`: the five file types with their
icons, a window that edits a file, and a Quick Look extension driven by the
summary tier. `macos/README.md` documents the parts of that which are not
obvious, each of which was learned by watching it fail silently.

A `.emx` opens as an editable grid instead of CSV text. Its model lives in
`macos/app/Grid.swift`, kept free of AppKit so `macos/test-grid.sh` can check
the rules — which typed cells a column accepts, and whether a saved table is
the table that was loaded — without a window server. Run it alongside
`cargo test` when touching either.
