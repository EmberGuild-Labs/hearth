# Hearth for programs

Notes for a script, a CI job, or a coding agent driving `hearth`. Everything
here is a property the tool is tested to have, not a convention it happens to
follow today.

## The contract

**Nothing ever waits for a human.** Every command runs to completion without a
terminal. Where a passphrase is needed, `--passphrase-env VAR` supplies it, and
standard input is read when it is a pipe. `hearth edit` only ever prompts when
stdin is a terminal; with a pipe it takes `--from` or waits for the editor
process, never for a keypress.

**Data on stdout, everything else on stderr.** Advisory notes, warnings and
progress go to stderr. A `--json` run writes exactly one JSON document to
stdout and nothing else, so `hearth info x.emt --json | jq .` works without
`--quiet`.

**Exit status is the verdict.**

| Status | Meaning |
|---|---|
| `0` | it worked |
| `1` | it did not — the reason is on stderr |
| `1` from `diff` | the files differ (this is the conventional diff behaviour, not an error) |
| `1` from `validate` | the file does not satisfy its own rules |

`--json` changes the shape of the answer, never the verdict.

**Writes are atomic.** Every write goes to a temporary file in the same
directory and is renamed over the target, so an interrupted run leaves the
original intact rather than a half-written container.

## Machine-readable output

```sh
hearth info    <file> --json      # container, chunk table, provenance summary
hearth validate <file> --json     # {"ok": bool, "errors": n, "issues": [...]}
hearth diff  <a> <b> --json       # {"identical": bool, "changes": [...]}
hearth formats       --json       # what this build reads and writes
hearth get <file> <path> --json   # one config value, typed
```

`hearth formats --json` is the capability check: it lists every format this
build handles with its imports and exports, so a caller can decide what to do
with a file without trying and catching failures.

## Reading part of a big file

`hearth info` and `hearth view --limit` both cost roughly the same on a
100-row table and a three-million-row one, and neither loads the payload.

`hearth info --json` reports a `payload` object: where each sub-chunk of
`DATA` lies, so a caller can fetch one row group or one tile rather than a
file.

```sh
hearth info big.emx --json | jq '.payload.children | length'      # 5861
hearth info big.emx --json | jq '.payload.children[1]'
# {"type": "RGRP", "offset": 322, "length": 3566}
```

`payload` is `null` when the file has no `DATA`, and `{"addressable": false,
"why": "compressed" | "sealed"}` when its sub-chunks cannot be read
individually. That is a fact about the layout rather than a failure: a payload
of many small parts is stored as one compressed stream because that is how it
compresses, and it is small enough that seeking into it would save nothing.

`hearth view <file> --limit N` uses the same index, decoding only the leading
sub-chunks the view will show. On a three-million-row `.emx`, `--limit 5`
takes 0.03s and 3.9 MB of memory against 1.04s and 871 MB for a whole-file
read. The row count in the footer still comes from the file, not from what was
read.

The content hash is verified either way — it covers the whole chunk table, so
checking it costs a pass over the file. What a partial read saves is
decompression and memory, not I/O.

## Editing without an editor

Three ways, smallest first.

**One value** (`.emc`):

```sh
hearth get service.emc database.host          # db.internal
hearth set service.emc database.port 5433     # typed: 5433 is a number
hearth set service.emc name "ridge" --string  # forced to a string
hearth unset service.emc features             # a prefix takes the subtree
```

`set` reads its value as JSON when it parses as JSON and as a string when it
does not, so `true`, `8080` and `null` mean what they look like. It writes one
node and leaves every other chunk — sealed groups included — untouched. If the
schema declares a different type, the write still happens and the mismatch is
reported on stderr; `hearth validate` will report it too.

**A whole payload**, for any format:

```sh
dialect=$(hearth edit report.emd --export /tmp/draft)   # prints: md
$EDITOR /tmp/draft                                       # or generate it
hearth edit report.emd --from /tmp/draft                 # put it back
```

This is the round trip `hearth edit` takes interactively, split in two. The
container is preserved: provenance gains a link, capabilities and migration
rules survive, and the summary tier is rebuilt from the new payload.

**In a pipeline**, with no files at all:

```sh
cat notes.md | hearth -q convert - --src md -o - > notes.emt
hearth -q convert notes.emt --to md -o - | grep '^#'
```

`-` is stdin as the input and stdout as the output. Bytes on stdin carry no
name, so `--src <ext>` says what they are; guessing would make the format
depend on what a sniffer happened to think.

## What refuses, and why

These are deliberate. A tool that did them anyway would produce a file that
lies.

| Situation | What happens |
|---|---|
| `edit` on a file with sealed values | refused — re-importing cannot restore a secret it never saw. `hearth unseal` first |
| `edit` on a file at another schema version | refused — an edit must not migrate a file as a side effect. `hearth migrate` first |
| `set` a *new* path on a file with sealed values | refused unless `--force`, since the path may already exist inside the sealed half |
| export a `.emc` whose secrets are sealed | refused — a config quietly missing its password is how a deployment breaks |
| `encrypt` a file that already has an encrypted slot | refused — a file needing two passphrases with no way to say which is which. `decrypt` first, or use `seal` to add a second slot deliberately |
| `decrypt` a file with more than one slot and no `--slot` | refused — guessing which half of a split-trust file to open is exactly the wrong guess |
| any passphrase shorter than 8 characters | refused before anything is written |
| `create` a `.emx` without `--columns`, or a `.emi` without `--size` | refused — the format cannot invent them |
| an output name that contradicts `--to`/`--as` | refused rather than resolved by precedence |

## A worked example

Bump a port across a directory of configs, and prove nothing else moved:

```sh
for f in config/*.emc; do
    before=$(mktemp)
    hearth -q convert "$f" --to json -o - > "$before"
    hearth -q set "$f" listen.port 9090
    hearth validate "$f" --json | jq -e '.ok' > /dev/null || echo "$f now fails its own rules"
    hearth -q convert "$f" --to json -o - | diff "$before" - | head -5
done
```

Every one of those edits is in each file's provenance chain afterwards:

```sh
hearth verify-chain config/service.emc
hearth info config/service.emc --json | jq '.provenance.latest.action'
```

## When not to use these formats

An Ember file is worth it when at least one of the five properties is — a
schema that travels with the data, a semantic diff, a provenance chain, a
cheap summary tier, or a migration path. For source code, a README, or
anything a human reads in an editor and a tool greps, plain text remains the
right answer: every other program in the world already speaks it, and
`git diff` on a binary container tells you nothing.

The formats earn their place at the edges of a system — configuration with
secrets in it, tables whose units matter, documents that need to prove where
they came from — not as a replacement for text everywhere.
