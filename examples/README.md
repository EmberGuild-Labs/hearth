# Examples

One input per format, plus the supporting files a few commands take. Every one
of these is converted by the integration test suite, so a broken example fails
the build rather than failing a newcomer.

Run these from this directory.

| File | Becomes | Shows |
|---|---|---|
| `notes.md` | `.emt` (or `.emd` with `--as emd`) | headings, lists, quotations, a fenced code block |
| `service.json` | `.emc` | nested config, mixed types, an empty list, a secret |
| `service-v2.json` | `.emc` | the same config, edited — for `hearth diff` |
| `readings.csv` | `.emx` | units in the header, an empty cell, an identifier with leading zeros |
| `journey.csv` | `.emx` | a computed column whose units agree |
| `journey-broken.csv` | `.emx` | a computed column whose units do not |
| `swatch.png` | `.emi` | a gradient with a flat block, so tiles and the palette both have something to show |
| `caps.json` | — | a capability declaration to embed with `--caps` |
| `policy.json` | — | a policy to check that declaration against |
| `rules.json` | — | a migration rule set to attach with `hearth rules set` |

## A tour

Convert everything:

```sh
hearth convert notes.md
hearth convert service.json
hearth convert readings.csv
hearth convert journey.csv
hearth convert swatch.png
```

**Nothing to convert from.** Start a file from scratch and open it in your
editor. The second command hands `$EDITOR` a `.csv`, and whatever comes back
becomes the payload of the same file:

```sh
hearth create scratch.emt --title "Field Notes" --edit
hearth create sightings.emx --columns "station, count, distance (m)" --edit
```

**The container, without reading the payload:**

```sh
hearth info notes.emt
hearth view notes.emt --summary
```

**Semantic diff.** Both files are the same config with a port changed and a key
renamed:

```sh
hearth convert service-v2.json
hearth diff service.emc service-v2.emc
```

**Units that are actually checked.** The first computes; the second refuses:

```sh
hearth recompute journey.emx && hearth view journey.emx
hearth convert journey-broken.csv && hearth validate journey-broken.emx
```

**Capabilities.** Embed a declaration, then check it against a policy the
declaration does not fit:

```sh
hearth convert service.json --caps caps.json -o scoped.emc
hearth validate scoped.emc --policy policy.json
```

**Split-trust secrets.** The password becomes unreadable without the
passphrase; everything else stays open:

```sh
hearth seal service.emc database.password --slot 1 --label prod
hearth view service.emc
hearth view service.emc --unlock 1
```

**Migration.** Attach the rules, then let the file upgrade itself:

```sh
hearth rules set service.emc rules.json
hearth rules show service.emc
hearth migrate service.emc --dry-run
hearth migrate service.emc
```

**Documents.** Pin the layout and confirm two renders are identical:

```sh
hearth convert notes.md --as emd -o report.emd
hearth pin report.emd
hearth convert report.emd --to pdf -o one.pdf
hearth convert report.emd --to pdf -o two.pdf --force
cmp one.pdf two.pdf && echo "identical"
```

**Images.** The thumbnail comes from the summary tier, so `DATA` is never
decoded:

```sh
hearth thumbnail swatch.emi
hearth view swatch.emi --summary
hearth view swatch.emi          # a terminal preview, if yours does truecolour
```

**Round-trips.** `.emt` and `.emx` are byte-for-byte:

```sh
hearth convert notes.emt --to md -o back.md && diff notes.md back.md && echo "identical"
hearth convert readings.emx --to csv -o back.csv && diff readings.csv back.csv && echo "identical"
```

## Cleaning up

Generated files are gitignored, but to be sure:

```sh
rm -f *.emt *.emd *.emi *.emc *.emx *.pdf back.* one.pdf two.pdf *.thumb.png
```
