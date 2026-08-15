#!/bin/bash
#
# Check the .emx grid's model.
#
# The window is checked by opening one. This checks the part that cannot be:
# which typed cells are accepted, what they become, and whether saving a table
# gives back the table that was loaded. `app/Grid.swift` has no AppKit in it
# precisely so that this can run without a window server.
#
#   ./macos/test-grid.sh
#
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT="$(mktemp -d)"
trap 'rm -rf "$OUT"' EXIT

if ! command -v swiftc >/dev/null 2>&1; then
    echo "swiftc not found — install the Xcode command line tools" >&2
    exit 1
fi

swiftc -O -o "$OUT/gridtest" "$HERE/app/Grid.swift" "$HERE/tests/main.swift"
"$OUT/gridtest"
