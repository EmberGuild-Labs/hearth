#!/usr/bin/env python3
"""Generate the pixel art for the Ember File Ecosystem.

Seven subjects — the Wick spine, the Hearth app, and the five formats — each
drawn twice so there is a real choice to make rather than a fait accompli.
Everything is a 16x16 grid, the same grid a favicon and a Finder icon use, so
every exported size is a whole-number multiple with no resampling anywhere.
This follows the approach already used in the EMBR repository, deliberately:
two projects in one family should be drawn the same way.

Each design is written three times:

  * SVG — one <rect> per run of pixels, `shape-rendering="crispEdges"`, so it
    stays sharp at any size. This is the master.
  * PNG — nearest-neighbour upscale, for previewing and for anything that
    will not take an SVG.
  * a contact sheet — the two candidates side by side on a dark ground, plus
    one family sheet showing every chosen mark together.

Edit a grid below and re-run:

    python3 assets/make_art.py

Nothing here needs a dependency. The PNG writer is forty lines because pixel
art wants nearest-neighbour scaling anyway, and a build step that needs Pillow
installed is a build step nobody runs.
"""

from __future__ import annotations

import struct
import zlib
from pathlib import Path

OUT = Path(__file__).resolve().parent
SIZE = 16          # every grid is SIZE x SIZE
SCALE = 32         # PNG pixel size, so 16 * 32 = 512px

# --------------------------------------------------------------------------
# Palette
#
# The ember colours are shared with EMBR so the family reads as one thing.
# On top of that each format gets a single accent hue, which is what lets you
# tell a .emx from a .emi at 16 pixels without reading anything. Every mark
# still carries ember somewhere: the accent says which format, the fire says
# whose.
# --------------------------------------------------------------------------
PALETTE = {
    ".": None,              # transparent

    # Ember — the shared core.
    "R": "#B3200A",         # deep red, outer edge
    "O": "#EF5F10",         # orange
    "A": "#FF9A1F",         # amber
    "Y": "#FFD23B",         # yellow
    "W": "#FFF4C9",         # white-hot core

    # Neutrals.
    "K": "#2A2230",         # ink
    "G": "#7C7480",         # grey, for body text
    "P": "#F3EAD9",         # paper

    # Format accents, each with a shade for depth.
    "C": "#2E93A0",         # teal        .emi
    "c": "#1B5F6B",
    "B": "#3D6FA8",         # steel blue  .emc
    "b": "#27476B",
    "N": "#4E9A51",         # green       .emx
    "n": "#2E6633",
    "V": "#8360A8",         # violet      .emd
    "v": "#55396E",
}

# ==========================================================================
# Wick — the spine specification.
#
# Wick is the thin shared core everything else burns on, so both candidates
# are about *thinness* and *sharing*, not about fire in general. EMBR already
# owns the plain flame; if this looked like that, the two would be confusable
# at icon size, which is exactly the failure to avoid.
# ==========================================================================

# 1. Braid — a flame on a twisted cord. The cord is deliberately three pixels
#    wide: the spine is the smallest part of the system and the whole point
#    is that it is small.
WICK_BRAID = [
    "................",
    ".......WW.......",
    "......YWWY......",
    "......AWWA......",
    ".....AOOOOA.....",
    ".....ROOOOR.....",
    "......ROOR......",
    "......RRRR......",
    "......PPGG......",
    "......GPPG......",
    "......GGPP......",
    "......PGGP......",
    "......PPGG......",
    "......GPPG......",
    "......GGPP......",
    "......PGGP......",
]

# 2. Chunks — a flame above a stack of TLV records, each with its bright
#    four-byte type marker and its body. This is literally the format: the
#    header, then a flat list of typed chunks. The most explanatory of the
#    two, and the least likely to be mistaken for a generic flame.
WICK_CHUNKS = [
    ".......RR.......",
    "......ROOR......",
    ".....ROAAOR.....",
    ".....ROYYOR.....",
    "......RAAR......",
    ".......RR.......",
    "................",
    ".YYRRRRRRRRRRRR.",
    "................",
    ".YYRRRRRRRRRRRR.",
    "................",
    ".YYRRRRRRRRRRRR.",
    "................",
    ".YYRRRRRRRRRRRR.",
    "................",
    ".YYRRRRRRRRRRRR.",
]

# ==========================================================================
# Hearth — the hub application.
# ==========================================================================

# 1. Arch — a fireplace with a fire in it. Literal, warm, and instantly
#    readable as "the place where the fire is kept", which is what a hub
#    application is.
HEARTH_ARCH = [
    "GGGGGGGGGGGGGGGG",
    ".GGGGGGGGGGGGGG.",
    ".GGG........GGG.",
    ".GG..........GG.",
    ".GG..........GG.",
    ".GG....RR....GG.",
    ".GG...ROOR...GG.",
    ".GG...ROOR...GG.",
    ".GG..ROAAOR..GG.",
    ".GG..ROAAOR..GG.",
    ".GG.ROAYYAOR.GG.",
    ".GG.ROAWWAOR.GG.",
    ".GG.ROAYYAOR.GG.",
    ".GG.RROAAORR.GG.",
    ".GG.RROOOORR.GG.",
    "GGGGGGGGGGGGGGGG",
]

# 2. Converge — one fire with five sparks rising off it, one in each format's
#    accent colour. Says what Hearth actually is (the one place all five
#    formats meet) rather than what it is named after, and it is the only
#    mark in the set that shows the whole family at once.
HEARTH_CONVERGE = [
    "..C..........N..",
    "................",
    "....B......V....",
    "................",
    ".......P........",
    ".......RR.......",
    "......ROOR......",
    "......ROOR......",
    ".....ROAAOR.....",
    ".....ROAAOR.....",
    "....ROAYYAOR....",
    "...ROAYWWYAOR...",
    "...ROAYWWYAOR...",
    "...ROAAYYAAOR...",
    "....ROOAAOOR....",
    ".....RROORR.....",
]

# ==========================================================================
# .emt — text.
# ==========================================================================

# 1. Lines — a small flame over ragged-right text. The simplest possible
#    statement of "this is a text file", and the ragged right edge is what
#    makes it read as prose rather than as a table.
EMT_LINES = [
    ".......RR.......",
    "......ROOR......",
    ".....ROAAOR.....",
    ".....ROYYOR.....",
    ".....RAYYAR.....",
    "......RAAR......",
    "................",
    "..PPPPPPPPPPPP..",
    "................",
    "..PPPPPPPPPPPP..",
    "................",
    "..PPPPPPPPPP....",
    "................",
    "..PPPPPPPPPPPP..",
    "................",
    "..PPPPPP........",
]

# 2. Sections — a heading, a paragraph, a code block and a bulleted list,
#    each visibly a different kind of thing. This is the actual difference
#    between .emt and .txt: the structure is stored rather than guessed, and
#    the mark says so.
EMT_SECTIONS = [
    "................",
    "..AAAAAAAA......",
    "................",
    "..PPPPPPPPPPPP..",
    "..PPPPPPPPPP....",
    "................",
    "..GGGGGGGGGGGG..",
    "..GGGGGGGGGGGG..",
    "..GGGGGGGGGGGG..",
    "................",
    "..O.PPPPPPPP....",
    "................",
    "..O.PPPPPPPPP...",
    "................",
    "..O.PPPPPP......",
    "................",
]

# ==========================================================================
# .emd — documents.
# ==========================================================================

# 1. Pinned — a page held by a pin. The one thing .emd does that nothing else
#    does is let you freeze a layout on purpose, and a pin is the clearest
#    two-pixel way to say it.
EMD_PINNED = [
    "....RRRRRRRR....",
    "....ROOOOOOR....",
    "....RRRRRRRR....",
    ".......AA.......",
    ".......AA.......",
    "..PPPPPAAPPPPP..",
    "..PPPPPPPPPPPP..",
    "..PPVVVVVVVVPP..",
    "..PPPPPPPPPPPP..",
    "..PPVVVVVVVVPP..",
    "..PPPPPPPPPPPP..",
    "..PPVVVVVVPPPP..",
    "..PPPPPPPPPPPP..",
    "..PPVVVVVVVVPP..",
    "..PPPPPPPPPPPP..",
    "..PPVVVVPPPPPP..",
]

# 2. Reflow — the same document at two widths, with an ember arrow between.
#    Says the other half of the format: one structure, many renderings. The
#    pin states the exception; this states the rule.
EMD_REFLOW = [
    "..PPPPPPPPPPPP..",
    "..PVVVVVVVVVVP..",
    "..PVVVVVVVPPPP..",
    "..PPPPPPPPPPPP..",
    "................",
    "...AAAAAAAAAA...",
    "....AAAAAAAA....",
    ".....AAAAAA.....",
    "......AAAA......",
    ".......AA.......",
    ".....PPPPPP.....",
    ".....PVVVVP.....",
    ".....PVVVVP.....",
    ".....PVVPPP.....",
    ".....PVVVVP.....",
    ".....PPPPPP.....",
]

# ==========================================================================
# .emi — images.
# ==========================================================================

# 1. Tiles — a four-by-four grid with exactly one tile lit. The whole reason
#    .emi exists is that an edit touches tiles rather than the file, and this
#    is that sentence as a picture.
EMI_TILES = [
    "................",
    ".CCC.CCC.CCC.CCC",
    ".CCC.CCC.CCC.CCC",
    ".CCC.CCC.CCC.CCC",
    "................",
    ".CCC.CCC.AAA.CCC",
    ".CCC.CCC.AYA.CCC",
    ".CCC.CCC.AAA.CCC",
    "................",
    ".CCC.CCC.CCC.CCC",
    ".CCC.CCC.CCC.CCC",
    ".CCC.CCC.CCC.CCC",
    "................",
    ".CCC.CCC.CCC.CCC",
    ".CCC.CCC.CCC.CCC",
    ".CCC.CCC.CCC.CCC",
]

# 2. Scene — a framed picture: an ember sun over hills. Reads as "image" to
#    anyone, instantly, with no explanation. The safer of the two and the
#    less specific.
EMI_SCENE = [
    "cCCCCCCCCCCCCCCc",
    "C..............C",
    "C.....AAA......C",
    "C....AYYYA.....C",
    "C....AYWYA.....C",
    "C.....AAA......C",
    "C..............C",
    "C..............C",
    "C.......c......C",
    "C......ccc.....C",
    "C.....ccccc....C",
    "C...c.ccccc.c..C",
    "C..ccccccccccc.C",
    "C.cccccccccccccC",
    "Cccccccccccccc.C",
    "cCCCCCCCCCCCCCCc",
]

# ==========================================================================
# .emc — configuration.
# ==========================================================================

# 1. Keyhole — a flame with a keyhole knocked out of it. Split-trust
#    encryption is the format's most distinctive feature, and negative space
#    is how EMBR's own monogram works, so the family idiom is already there.
EMC_KEYHOLE = [
    "................",
    ".......RR.......",
    "......ROOR......",
    ".....ROAAOR.....",
    "....ROAAAAOR....",
    "...ROAA..AAOR...",
    "..ROAA....AAOR..",
    "..ROAA....AAOR..",
    "..ROAAA..AAAOR..",
    "..ROAAA..AAAOR..",
    "..ROAAA..AAAOR..",
    "..ROAAAAAAAAOR..",
    "..ROAAAAAAAAOR..",
    "...ROAAAAAAOR...",
    "....ROAAAAOR....",
    ".....RROORR.....",
]

# 2. Seal — a shield with an ember check in it. Says the other half of the
#    format: a config that validates itself and declares what it is allowed
#    to touch. More corporate, more legible, less interesting.
EMC_SEAL = [
    "....BBBBBBBB....",
    "...BBBBBBBBBB...",
    "..BBBBBBBBBBBB..",
    "..BbbbbbbbbbBB..",
    "..BbBBBBBBBbBB..",
    "..BbBBBBBBAbBB..",
    "..BbBBBBBAAbBB..",
    "..BbBBBBAAABBB..",
    "..BbBAABAAABBB..",
    "..BbBAAAAABbBB..",
    "..BbBBAAABBbBB..",
    "...BbBBABBBbB...",
    "...BBbbbbbbBB...",
    "....BBBBBBBB....",
    ".....BBBBBB.....",
    "......BBBB......",
]

# ==========================================================================
# .emx — tables.
# ==========================================================================

# 1. Columns — a table with a header row and one column in ember: the typed,
#    unit-carrying, computed column that the format is for. Distinct from the
#    .emi tile grid because the cells are wide rather than square, which is
#    what a table looks like.
EMX_COLUMNS = [
    "................",
    ".AAAA.AAAA.AAAA.",
    ".AAAA.AAAA.AAAA.",
    "................",
    ".NNNN.NNNN.OOOO.",
    ".NNNN.NNNN.OOOO.",
    "................",
    ".NNNN.NNNN.OOOO.",
    ".NNNN.NNNN.OOOO.",
    "................",
    ".NNNN.NNNN.OOOO.",
    ".NNNN.NNNN.OOOO.",
    "................",
    ".NNNN.NNNN.OOOO.",
    ".NNNN.NNNN.OOOO.",
    "................",
]

# 2. Balance — a two-pan balance with an ember at the fulcrum. This is the
#    unit algebra: metres on one side, seconds on the other, and the file
#    refusing to add them. The most conceptual mark in the set.
EMX_BALANCE = [
    "................",
    ".......YY.......",
    "......YAAY......",
    ".......AA.......",
    ".NNNNNNNNNNNNNN.",
    "...N........N...",
    "...N........N...",
    ".NNNNN....NNNNN.",
    "..nnn......nnn..",
    "................",
    ".......NN.......",
    ".......NN.......",
    ".......NN.......",
    ".......NN.......",
    "....NNNNNNNN....",
    "...nnnnnnnnnn...",
]

# --------------------------------------------------------------------------
# Subjects, their candidates, and which one ships.
#
# Change a `chosen` and re-run to swap that mark everywhere it appears.
# --------------------------------------------------------------------------
SUBJECTS = {
    "wick": {
        "title": "Wick — the container spine",
        "chosen": "chunks",
        "candidates": {
            "braid": (WICK_BRAID, "Flame on a twisted cord. The spine as the thin shared core."),
            "chunks": (WICK_CHUNKS, "Flame over a stack of TLV records. Literally the format."),
        },
    },
    "hearth": {
        "title": "Hearth — the hub application",
        "chosen": "arch",
        "candidates": {
            "arch": (HEARTH_ARCH, "A fireplace with a fire in it. Warm, literal, unmistakable."),
            "converge": (HEARTH_CONVERGE, "One fire, five sparks — one per format. Shows the family."),
        },
    },
    "emt": {
        "title": ".emt — text",
        "chosen": "sections",
        "candidates": {
            "lines": (EMT_LINES, "Flame over ragged-right prose. Simplest reading of 'text'."),
            "sections": (EMT_SECTIONS, "Heading, paragraph, code, list. The structure .txt lacks."),
        },
    },
    "emd": {
        "title": ".emd — documents",
        "chosen": "reflow",
        "candidates": {
            "pinned": (EMD_PINNED, "A page held by a pin. Layout frozen on purpose."),
            "reflow": (EMD_REFLOW, "The same document at two widths. One structure, many renderings."),
        },
    },
    "emi": {
        "title": ".emi — images",
        "chosen": "scene",
        "candidates": {
            "tiles": (EMI_TILES, "A tile grid with one tile lit. An edit touches tiles, not the file."),
            "scene": (EMI_SCENE, "A framed sun over hills. Reads as 'image' to anyone, instantly."),
        },
    },
    "emc": {
        "title": ".emc — configuration",
        "chosen": "keyhole",
        "candidates": {
            "keyhole": (EMC_KEYHOLE, "A keyhole knocked out of a flame. Split-trust secrets."),
            "seal": (EMC_SEAL, "A shield with a check. Config that validates and scopes itself."),
        },
    },
    "emx": {
        "title": ".emx — tables",
        "chosen": "columns",
        "candidates": {
            "columns": (EMX_COLUMNS, "A table with one typed, unit-carrying column lit."),
            "balance": (EMX_BALANCE, "A balance. Metres on one side, seconds on the other."),
        },
    },
}

# Sizes macOS wants in an .iconset. Every one is an exact whole-number
# multiple of the 16x16 grid, so each is pixel-perfect with no resampling —
# which is the entire reason to draw on a 16px grid in the first place.
ICONSET = [
    ("icon_16x16.png", 1),
    ("icon_16x16@2x.png", 2),
    ("icon_32x32.png", 2),
    ("icon_32x32@2x.png", 4),
    ("icon_128x128.png", 8),
    ("icon_128x128@2x.png", 16),
    ("icon_256x256.png", 16),
    ("icon_256x256@2x.png", 32),
    ("icon_512x512.png", 32),
    ("icon_512x512@2x.png", 64),
]

SHEET_BG = (24, 20, 26)


def check(grid: list[str], name: str) -> None:
    """Pixel art is unforgiving about alignment, so verify the grid is square
    and uses only known colours before rendering anything."""
    if len(grid) != SIZE:
        raise SystemExit(f"{name}: {len(grid)} rows, expected {SIZE}")
    for y, row in enumerate(grid):
        if len(row) != SIZE:
            raise SystemExit(f"{name} row {y}: {len(row)} cols, expected {SIZE}")
        for ch in row:
            if ch not in PALETTE:
                raise SystemExit(f"{name} row {y}: unknown colour '{ch}'")


def hex_to_rgb(h: str) -> tuple[int, int, int]:
    return tuple(int(h[i:i + 2], 16) for i in (1, 3, 5))  # type: ignore[return-value]


def png_chunk(tag: bytes, data: bytes) -> bytes:
    return (struct.pack(">I", len(data)) + tag + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF))


def write_rgba_png(path: Path, width: int, height: int, rows: list[bytes]) -> None:
    raw = bytearray()
    for row in rows:
        raw.append(0)  # filter type 0 (None) for each scanline
        raw += row
    png = (b"\x89PNG\r\n\x1a\n"
           + png_chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
           + png_chunk(b"IDAT", zlib.compress(bytes(raw), 9))
           + png_chunk(b"IEND", b""))
    path.write_bytes(png)


def write_png(path: Path, grid: list[str], scale: int) -> None:
    """Minimal RGBA PNG writer — no dependencies, and nearest-neighbour
    upscaling is exactly what pixel art wants anyway."""
    w = h = SIZE * scale
    rows = []
    for y in range(h):
        row = bytearray()
        src = grid[y // scale]
        for x in range(w):
            colour = PALETTE[src[x // scale]]
            row += bytes((0, 0, 0, 0)) if colour is None else bytes(hex_to_rgb(colour)) + b"\xff"
        rows.append(bytes(row))
    write_rgba_png(path, w, h, rows)


def write_svg(path: Path, grid: list[str]) -> None:
    """One rect per run of same-coloured pixels. Runs are merged horizontally
    so the file stays small and stays hand-editable."""
    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {SIZE} {SIZE}" '
        f'width="512" height="512" shape-rendering="crispEdges">'
    ]
    for y, row in enumerate(grid):
        x = 0
        while x < SIZE:
            ch = row[x]
            run = 1
            while x + run < SIZE and row[x + run] == ch:
                run += 1
            if PALETTE[ch] is not None:
                parts.append(
                    f'<rect x="{x}" y="{y}" width="{run}" height="1" fill="{PALETTE[ch]}"/>'
                )
            x += run
    parts.append("</svg>")
    path.write_text("\n".join(parts) + "\n")


def write_sheet(path: Path, grids: list[list[str]], scale: int = 12, gap: int = 3) -> None:
    """Several marks side by side on a dark ground, for comparing at a glance."""
    cell = SIZE + gap * 2
    w = cell * len(grids) * scale
    h = cell * scale
    canvas = [[SHEET_BG for _ in range(w)] for _ in range(h)]

    for i, grid in enumerate(grids):
        ox = (i * cell + gap) * scale
        oy = gap * scale
        for gy in range(SIZE):
            for gx in range(SIZE):
                colour = PALETTE[grid[gy][gx]]
                if colour is None:
                    continue
                rgb = hex_to_rgb(colour)
                for sy in range(scale):
                    for sx in range(scale):
                        canvas[oy + gy * scale + sy][ox + gx * scale + sx] = rgb

    rows = [bytes(b for px in row for b in (*px, 255)) for row in canvas]
    write_rgba_png(path, w, h, rows)


def write_iconset(directory: Path, grid: list[str]) -> None:
    directory.mkdir(parents=True, exist_ok=True)
    for filename, scale in ICONSET:
        write_png(directory / filename, grid, scale)


def main() -> None:
    candidates_dir = OUT / "candidates"
    candidates_dir.mkdir(parents=True, exist_ok=True)

    chosen_grids = []
    for key, subject in SUBJECTS.items():
        print(f"\n{subject['title']}")
        grids = []
        for name, (grid, description) in subject["candidates"].items():
            check(grid, f"{key}-{name}")
            grids.append(grid)
            stem = f"{key}-{name}"
            write_png(candidates_dir / f"{stem}.png", grid, SCALE)
            write_png(candidates_dir / f"{stem}-32.png", grid, 2)   # favicon scale
            write_svg(candidates_dir / f"{stem}.svg", grid)
            mark = "  <-- chosen" if name == subject["chosen"] else ""
            print(f"  {name:<10} {description}{mark}")

        write_sheet(candidates_dir / f"{key}-compare.png", grids, scale=16)

        # The chosen mark goes one level up, where everything else links to it.
        grid = subject["candidates"][subject["chosen"]][0]
        chosen_grids.append(grid)
        write_svg(OUT / f"{key}.svg", grid)
        write_png(OUT / f"{key}.png", grid, 32)        # 512px
        write_png(OUT / f"{key}-1024.png", grid, 64)   # README, stores
        write_png(OUT / f"{key}-32.png", grid, 2)      # favicon

    # Hearth is the application and the five formats are document types, and
    # macOS wants an .iconset for each: the app icon in the Dock, and one per
    # exported type declaration so a .emx looks like a .emx in Finder. The
    # 16px grid pays for itself here — every size in an .iconset is a
    # whole-number multiple of it, so nothing is ever resampled.
    for key in ["hearth", "emt", "emd", "emi", "emc", "emx"]:
        subject = SUBJECTS[key]
        write_iconset(OUT / f"{key}.iconset", subject["candidates"][subject["chosen"]][0])

    write_sheet(OUT / "family.png", chosen_grids, scale=14)
    print(f"\ncandidates/*-compare.png  — two candidates per subject, side by side")
    print(f"family.png                — every chosen mark together")
    print(f"*.iconset/                — macOS icon sizes: the app, and one per format")


if __name__ == "__main__":
    main()
