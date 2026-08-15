//! `.emi` — raster images stored as tiles, with room for a vector layer and
//! an edit history in the same file.
//!
//! A PNG is one compressed stream. Change one pixel and the whole stream is
//! rewritten, so every version of an image is a completely new file and no
//! tool can tell you what changed without decoding both in full and comparing
//! pixels.
//!
//! `.emi` stores the raster as a grid of [`TILE_SIZE`]-pixel tiles, each its
//! own chunk with its own hash. Three things follow:
//!
//! * **Lossless region patching.** Repainting a corner rewrites the tiles it
//!   touches, not the file.
//! * **A diff that means something.** Two versions are compared by tile hash,
//!   so `hearth diff` reports *where* the image changed, in pixel
//!   coordinates, without decoding tiles that match.
//! * **A cheap preview.** The thumbnail and palette live in `SUMM`, so a
//!   browser showing a directory of images reads a few kilobytes per file
//!   rather than decoding megapixels.
//!
//! Pixels are stored as raw RGBA8 and compressed by the chunk layer. Raw
//! rather than PNG-per-tile because the container already compresses, and
//! because a tile that is stored decoded is a tile that can be patched
//! without a decode-modify-re-encode cycle. The cost is that `.emi` is
//! somewhat larger than an equivalent PNG on photographic content; the
//! benefit is everything above. That is a real trade and it is stated
//! rather than hidden.
//!
//! The vector layer (`VECT`) and edit history (`EDIT`) are stored and
//! preserved, but this build does not rasterise vectors — an exported PNG is
//! the raster layer alone.

use libwick::chunks::{Chunk, ChunkList, ChunkType};
use libwick::error::{Error, Result};
use libwick::plugin::{Payload, Plugin, RenderOpts, Source, Starter};
use libwick::schema::{Issue, Schema};
use libwick::{Change, ChangeKind, KeyRing, Tag, WickFile};
use serde::{Deserialize, Serialize};

pub const TAG: Tag = Tag::new(b"MI");
pub const SCHEMA_VERSION: u32 = 1;

/// Tile edge in pixels. 64 keeps an edited region small while leaving each
/// tile (16 KB of RGBA) big enough for zstd to find structure in.
pub const TILE_SIZE: u32 = 64;

/// Longest edge of the `SUMM` thumbnail.
pub const THUMB_MAX: u32 = 128;

const IMHD: ChunkType = ChunkType::new(b"IMHD");
const TILE: ChunkType = ChunkType::new(b"TILE");
const VECT: ChunkType = ChunkType::new(b"VECT");
const EDIT: ChunkType = ChunkType::new(b"EDIT");
const THMB: ChunkType = ChunkType::new(b"THMB");
const PALT: ChunkType = ChunkType::new(b"PALT");
const STAT: ChunkType = ChunkType::new(b"STAT");

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImageHeader {
    pub width: u32,
    pub height: u32,
    #[serde(default = "default_tile")]
    pub tile: u32,
    /// Where the pixels came from, for the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// True when the source had no meaningful alpha, so an exporter can
    /// write RGB and not pay for a constant alpha channel.
    #[serde(default)]
    pub opaque: bool,
}

fn default_tile() -> u32 {
    TILE_SIZE
}

/// An RGBA8 image in memory.
#[derive(Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes, row-major.
    pub pixels: Vec<u8>,
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Image({}x{})", self.width, self.height)
    }
}

impl Image {
    pub fn new(width: u32, height: u32) -> Self {
        Image {
            width,
            height,
            pixels: vec![0; (width as usize) * (height as usize) * 4],
        }
    }

    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y as usize) * (self.width as usize) + x as usize) * 4;
        [
            self.pixels[i],
            self.pixels[i + 1],
            self.pixels[i + 2],
            self.pixels[i + 3],
        ]
    }

    pub fn set_pixel(&mut self, x: u32, y: u32, rgba: [u8; 4]) {
        let i = ((y as usize) * (self.width as usize) + x as usize) * 4;
        self.pixels[i..i + 4].copy_from_slice(&rgba);
    }

    pub fn is_opaque(&self) -> bool {
        self.pixels.chunks_exact(4).all(|p| p[3] == 255)
    }

    pub fn tiles_across(&self, tile: u32) -> u32 {
        self.width.div_ceil(tile)
    }

    pub fn tiles_down(&self, tile: u32) -> u32 {
        self.height.div_ceil(tile)
    }

    /// Nearest-neighbour box downsample. Good enough for a thumbnail and
    /// dependency-free; nothing here is trying to be an image editor.
    pub fn thumbnail(&self, max_edge: u32) -> Image {
        let scale = (self.width.max(self.height) as f64 / max_edge as f64).max(1.0);
        let w = ((self.width as f64 / scale).round() as u32).max(1);
        let h = ((self.height as f64 / scale).round() as u32).max(1);
        let mut out = Image::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let sx = ((x as f64 + 0.5) * scale) as u32;
                let sy = ((y as f64 + 0.5) * scale) as u32;
                out.set_pixel(
                    x,
                    y,
                    self.pixel(sx.min(self.width - 1), sy.min(self.height - 1)),
                );
            }
        }
        out
    }

    /// The most common colours, most frequent first. Quantised to 4 bits per
    /// channel so that near-identical shades count as one.
    pub fn palette(&self, n: usize) -> Vec<([u8; 4], u32)> {
        let mut counts: std::collections::HashMap<[u8; 4], u32> = Default::default();
        for p in self.pixels.chunks_exact(4) {
            let key = [p[0] & 0xF0, p[1] & 0xF0, p[2] & 0xF0, p[3] & 0xF0];
            *counts.entry(key).or_default() += 1;
        }
        let mut v: Vec<_> = counts.into_iter().collect();
        // Sort by count, then by colour, so the palette is deterministic and
        // two runs over the same image produce byte-identical summaries.
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v.truncate(n);
        v
    }
}

// ---------------------------------------------------------------------------
// PNG
// ---------------------------------------------------------------------------

pub fn from_png(bytes: &[u8]) -> Result<Image> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder
        .read_info()
        .map_err(|e| Error::Other(format!("not a readable PNG: {e}")))?;

    let mut buf = vec![0; reader.output_buffer_size().unwrap_or(0)];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| Error::Other(format!("could not decode PNG pixels: {e}")))?;
    buf.truncate(info.buffer_size());

    let (w, h) = (info.width, info.height);
    let n = (w as usize) * (h as usize);
    let mut pixels = vec![0u8; n * 4];

    // normalize_to_color8 expands palettes and 16-bit samples, so only these
    // four channel layouts can reach here.
    match info.color_type {
        png::ColorType::Rgba => pixels.copy_from_slice(&buf[..n * 4]),
        png::ColorType::Rgb => {
            for (i, p) in buf[..n * 3].chunks_exact(3).enumerate() {
                pixels[i * 4..i * 4 + 3].copy_from_slice(p);
                pixels[i * 4 + 3] = 255;
            }
        }
        png::ColorType::Grayscale => {
            for (i, g) in buf[..n].iter().enumerate() {
                pixels[i * 4..i * 4 + 4].copy_from_slice(&[*g, *g, *g, 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for (i, p) in buf[..n * 2].chunks_exact(2).enumerate() {
                pixels[i * 4..i * 4 + 4].copy_from_slice(&[p[0], p[0], p[0], p[1]]);
            }
        }
        other => {
            return Err(Error::Other(format!(
                "PNG colour type {other:?} is not supported"
            )))
        }
    }
    Ok(Image {
        width: w,
        height: h,
        pixels,
    })
}

/// Encode as PNG. Drops the alpha channel when every pixel is opaque, which
/// is both smaller and what the source almost certainly was.
pub fn to_png(img: &Image) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let opaque = img.is_opaque();
        let mut enc = png::Encoder::new(&mut out, img.width, img.height);
        enc.set_color(if opaque {
            png::ColorType::Rgb
        } else {
            png::ColorType::Rgba
        });
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc
            .write_header()
            .map_err(|e| Error::Other(format!("could not write PNG header: {e}")))?;
        if opaque {
            let rgb: Vec<u8> = img
                .pixels
                .chunks_exact(4)
                .flat_map(|p| [p[0], p[1], p[2]])
                .collect();
            writer.write_image_data(&rgb)
        } else {
            writer.write_image_data(&img.pixels)
        }
        .map_err(|e| Error::Other(format!("could not write PNG pixels: {e}")))?;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tiling
// ---------------------------------------------------------------------------

/// `[tx u16][ty u16][w u16][h u16][RGBA rows]`
fn encode_tile(img: &Image, tx: u32, ty: u32, tile: u32) -> Chunk {
    let x0 = tx * tile;
    let y0 = ty * tile;
    let w = tile.min(img.width - x0);
    let h = tile.min(img.height - y0);

    let mut v = Vec::with_capacity(8 + (w * h * 4) as usize);
    v.extend_from_slice(&(tx as u16).to_le_bytes());
    v.extend_from_slice(&(ty as u16).to_le_bytes());
    v.extend_from_slice(&(w as u16).to_le_bytes());
    v.extend_from_slice(&(h as u16).to_le_bytes());
    for y in y0..y0 + h {
        let start = ((y as usize) * (img.width as usize) + x0 as usize) * 4;
        v.extend_from_slice(&img.pixels[start..start + (w as usize) * 4]);
    }
    Chunk::new(TILE, v)
}

fn tile_coords(c: &Chunk) -> Result<(u32, u32, u32, u32)> {
    if c.value.len() < 8 {
        return Err(Error::Truncated("TILE header"));
    }
    let n = |i: usize| u16::from_le_bytes([c.value[i], c.value[i + 1]]) as u32;
    Ok((n(0), n(2), n(4), n(6)))
}

pub fn encode(img: &Image, header: &ImageHeader) -> Result<ChunkList> {
    let mut data = ChunkList::new();
    data.push(Chunk::new(IMHD, serde_json::to_vec(header)?));
    for ty in 0..img.tiles_down(header.tile) {
        for tx in 0..img.tiles_across(header.tile) {
            data.push(encode_tile(img, tx, ty, header.tile));
        }
    }
    Ok(data)
}

pub fn decode(data: &ChunkList) -> Result<(Image, ImageHeader)> {
    let header: ImageHeader = serde_json::from_slice(&data.require(IMHD, "IMHD")?.value)?;
    let mut img = Image::new(header.width, header.height);

    for c in data.all(TILE) {
        let (tx, ty, w, h) = tile_coords(c)?;
        let (x0, y0) = (tx * header.tile, ty * header.tile);
        if x0 + w > img.width || y0 + h > img.height {
            return Err(Error::Other(format!(
                "tile ({tx},{ty}) claims pixels outside a {}x{} image",
                img.width, img.height
            )));
        }
        let expected = 8 + (w as usize) * (h as usize) * 4;
        if c.value.len() != expected {
            return Err(Error::Other(format!(
                "tile ({tx},{ty}) holds {} bytes, expected {expected}",
                c.value.len()
            )));
        }
        for row in 0..h {
            let src = 8 + (row as usize) * (w as usize) * 4;
            let dst = (((y0 + row) as usize) * (img.width as usize) + x0 as usize) * 4;
            img.pixels[dst..dst + (w as usize) * 4]
                .copy_from_slice(&c.value[src..src + (w as usize) * 4]);
        }
    }
    Ok((img, header))
}

pub fn image(file: &WickFile) -> Result<(Image, ImageHeader)> {
    decode(&file.data()?)
}

/// Replace a rectangle of pixels, rewriting only the tiles it overlaps.
///
/// This is the operation a whole-stream format cannot offer. Returns the
/// tiles that actually changed.
pub fn patch(file: &mut WickFile, x: u32, y: u32, patch: &Image) -> Result<Vec<(u32, u32)>> {
    let data = file.data()?;
    let header: ImageHeader = serde_json::from_slice(&data.require(IMHD, "IMHD")?.value)?;
    if x + patch.width > header.width || y + patch.height > header.height {
        return Err(Error::Other(
            "patch extends past the edge of the image".into(),
        ));
    }

    let mut out = ChunkList::new();
    let mut touched = Vec::new();
    for c in data.iter() {
        if c.ty != TILE {
            out.push(c.clone());
            continue;
        }
        let (tx, ty, tw, th) = tile_coords(c)?;
        let (x0, y0) = (tx * header.tile, ty * header.tile);
        let overlaps = x < x0 + tw && x0 < x + patch.width && y < y0 + th && y0 < y + patch.height;
        if !overlaps {
            out.push(c.clone());
            continue;
        }

        let mut v = c.value.clone();
        for row in 0..th {
            for col in 0..tw {
                let (px, py) = (x0 + col, y0 + row);
                if px < x || py < y || px >= x + patch.width || py >= y + patch.height {
                    continue;
                }
                let src = patch.pixel(px - x, py - y);
                let dst = 8 + ((row as usize) * (tw as usize) + col as usize) * 4;
                v[dst..dst + 4].copy_from_slice(&src);
            }
        }
        if v != c.value {
            touched.push((tx, ty));
        }
        out.push(Chunk::new(TILE, v));
    }
    file.set_data(&out)?;
    Ok(touched)
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

pub struct Emi;

impl Plugin for Emi {
    fn tag(&self) -> Tag {
        TAG
    }
    fn ext(&self) -> &'static str {
        "emi"
    }
    fn name(&self) -> &'static str {
        "image"
    }
    fn about(&self) -> &'static str {
        "tiled lossless raster with region patching and a preview tier (replaces .png)"
    }
    fn imports(&self) -> &'static [&'static str] {
        &["png"]
    }
    fn exports(&self) -> &'static [&'static str] {
        &["png"]
    }
    fn schema_version(&self) -> u32 {
        SCHEMA_VERSION
    }

    fn import(&self, src: &Source) -> Result<Payload> {
        let img = match src.ext {
            "png" => from_png(src.bytes)?,
            // JPEG is lossy, so importing it would mean either decoding it —
            // baking in the artefacts and growing the file several times over
            // — or storing the stream verbatim, which is a `.embr` archive
            // wearing an image format's extension. Neither is worth doing
            // badly, so it is declined rather than half-supported.
            "jpg" | "jpeg" => {
                return Err(Error::Other(
                    "JPEG import is not implemented: decoding a lossy source into a lossless \
                     container bakes in its artefacts and multiplies its size. Convert to PNG \
                     first if that trade is one you want"
                        .into(),
                ))
            }
            other => return Err(Error::Other(format!("no .emi importer for .{other}"))),
        };

        let header = ImageHeader {
            width: img.width,
            height: img.height,
            tile: TILE_SIZE,
            source: Some(src.name.to_string()),
            opaque: img.is_opaque(),
        };

        let mut schema = Schema::new("image");
        schema.version = SCHEMA_VERSION;
        schema.extra = Some(serde_json::json!({
            "pixel_format": "rgba8",
            "tile": TILE_SIZE,
        }));

        Ok(Payload {
            summary: Some(summarize(&img)?),
            data: encode(&img, &header)?,
            schema: Some(schema),
            caps: None,
            migrations: None,
        })
    }

    /// A transparent canvas of the requested size.
    ///
    /// The size has to be given. An image has no natural empty state the way
    /// a document does — every default would be somebody's wrong one — so a
    /// missing `--size` is an error rather than a guess.
    fn starter(&self, spec: &Starter) -> Result<(&'static str, Vec<u8>)> {
        spec.only("emi", &["size"])?;
        let Some((w, h)) = spec.size else {
            return Err(Error::Other(
                "a new .emi needs its dimensions: --size 640x480".into(),
            ));
        };
        if w == 0 || h == 0 {
            return Err(Error::Other("an image cannot have a zero edge".into()));
        }
        Ok(("png", to_png(&Image::new(w, h))?))
    }

    fn export(&self, file: &WickFile, to: &str) -> Result<Vec<u8>> {
        match to {
            "png" => to_png(&image(file)?.0),
            other => Err(Error::Other(format!(".emi cannot export to .{other}"))),
        }
    }

    fn render(
        &self,
        file: &WickFile,
        opts: &RenderOpts,
        out: &mut dyn std::io::Write,
    ) -> Result<()> {
        if opts.summary {
            return render_summary(file, out);
        }
        let (img, header) = image(file)?;
        writeln!(
            out,
            "{}x{} px, {} tiles of {}, {}",
            img.width,
            img.height,
            img.tiles_across(header.tile) * img.tiles_down(header.tile),
            header.tile,
            if header.opaque {
                "opaque"
            } else {
                "with alpha"
            }
        )?;
        if let Some(v) = file.data()?.get(VECT) {
            writeln!(
                out,
                "vector layer: {} bytes (not rasterised)",
                v.value.len()
            )?;
        }

        // A terminal preview using half-block characters: two pixel rows per
        // text row, foreground for the top and background for the bottom.
        let width = opts.width.clamp(16, 120) as u32;
        let thumb = img.thumbnail(width);
        writeln!(out)?;
        for y in (0..thumb.height).step_by(2) {
            let mut line = String::new();
            for x in 0..thumb.width {
                let top = thumb.pixel(x, y);
                let bottom = if y + 1 < thumb.height {
                    thumb.pixel(x, y + 1)
                } else {
                    [0, 0, 0, 0]
                };
                line.push_str(&format!(
                    "\x1b[38;2;{};{};{}m\x1b[48;2;{};{};{}m▀",
                    top[0], top[1], top[2], bottom[0], bottom[1], bottom[2]
                ));
            }
            writeln!(out, "{line}\x1b[0m")?;
        }
        Ok(())
    }

    fn validate(&self, file: &WickFile) -> Result<Vec<Issue>> {
        let data = file.data()?;
        let mut issues = Vec::new();
        let header: ImageHeader = match data.get(IMHD) {
            Some(c) => serde_json::from_slice(&c.value)?,
            None => {
                issues.push(Issue::error("IMHD", "image header chunk is missing"));
                return Ok(issues);
            }
        };
        if header.tile == 0 {
            issues.push(Issue::error("IMHD", "tile size is zero"));
            return Ok(issues);
        }

        let across = header.width.div_ceil(header.tile);
        let down = header.height.div_ceil(header.tile);
        let expected = (across as usize) * (down as usize);
        let found = data.all(TILE).count();
        if found != expected {
            issues.push(Issue::error(
                "TILE",
                format!(
                    "a {}x{} image needs {expected} tiles, found {found}",
                    header.width, header.height
                ),
            ));
        }

        // Every grid position covered exactly once. A duplicate or a gap
        // would decode to a plausible image with a stale or black region,
        // which is exactly the sort of quiet corruption to refuse.
        let mut seen = std::collections::HashSet::new();
        for c in data.all(TILE) {
            let (tx, ty, _, _) = tile_coords(c)?;
            if tx >= across || ty >= down {
                issues.push(Issue::error(
                    "TILE",
                    format!("tile ({tx},{ty}) is outside the grid"),
                ));
            } else if !seen.insert((tx, ty)) {
                issues.push(Issue::error(
                    "TILE",
                    format!("tile ({tx},{ty}) appears twice"),
                ));
            }
        }
        for ty in 0..down {
            for tx in 0..across {
                if !seen.contains(&(tx, ty)) {
                    issues.push(Issue::error("TILE", format!("tile ({tx},{ty}) is missing")));
                }
            }
        }

        if header.opaque {
            if let Ok((img, _)) = decode(&data) {
                if !img.is_opaque() {
                    issues.push(Issue::warning(
                        "IMHD",
                        "header says opaque but some pixels are transparent",
                    ));
                }
            }
        }
        Ok(issues)
    }

    fn diff(&self, a: &WickFile, b: &WickFile, _keys: &KeyRing) -> Result<Vec<Change>> {
        let (da, db) = (a.data()?, b.data()?);
        let ha: ImageHeader = serde_json::from_slice(&da.require(IMHD, "IMHD")?.value)?;
        let hb: ImageHeader = serde_json::from_slice(&db.require(IMHD, "IMHD")?.value)?;
        let mut out = Vec::new();

        if (ha.width, ha.height) != (hb.width, hb.height) {
            out.push(Change::new(
                ChangeKind::Modified,
                "size",
                IMHD,
                format!("{}x{} -> {}x{}", ha.width, ha.height, hb.width, hb.height),
            ));
            // Tile coordinates mean different things at different sizes, so
            // comparing them after a resize would be noise.
            return Ok(out);
        }

        let index = |d: &ChunkList| -> Result<std::collections::HashMap<(u32, u32), [u8; 32]>> {
            d.all(TILE)
                .map(|c| {
                    Ok((
                        {
                            let (tx, ty, _, _) = tile_coords(c)?;
                            (tx, ty)
                        },
                        *blake3::hash(&c.value).as_bytes(),
                    ))
                })
                .collect()
        };
        let (ia, ib) = (index(&da)?, index(&db)?);

        let mut changed: Vec<(u32, u32)> = ib
            .iter()
            .filter(|((tx, ty), h)| ia.get(&(*tx, *ty)) != Some(*h))
            .map(|(k, _)| *k)
            .collect();
        changed.sort_unstable_by_key(|(x, y)| (*y, *x));

        for (tx, ty) in changed.iter().take(64) {
            out.push(Change::new(
                ChangeKind::Modified,
                format!("tile ({tx},{ty})"),
                TILE,
                format!(
                    "pixels {},{} to {},{}",
                    tx * hb.tile,
                    ty * hb.tile,
                    ((tx + 1) * hb.tile).min(hb.width) - 1,
                    ((ty + 1) * hb.tile).min(hb.height) - 1
                ),
            ));
        }
        if changed.len() > 64 {
            out.push(Change::new(
                ChangeKind::Modified,
                "…",
                TILE,
                format!("{} more tiles changed", changed.len() - 64),
            ));
        }
        if !changed.is_empty() {
            let total = ((hb.width.div_ceil(hb.tile)) * (hb.height.div_ceil(hb.tile))) as f64;
            out.push(Change::new(
                ChangeKind::Modified,
                "summary",
                TILE,
                format!(
                    "{} of {} tiles ({:.1}% of the image)",
                    changed.len(),
                    total as u64,
                    changed.len() as f64 / total * 100.0
                ),
            ));
        }
        Ok(out)
    }

    fn summarize(&self, data: &ChunkList) -> Result<Option<ChunkList>> {
        Ok(Some(summarize(&decode(data)?.0)?))
    }
}

fn summarize(img: &Image) -> Result<ChunkList> {
    let mut summ = ChunkList::new();
    let palette = img.palette(8);
    let total = (img.width as u64) * (img.height as u64);

    let stat = serde_json::json!({
        "width": img.width,
        "height": img.height,
        "megapixels": (total as f64 / 1e6 * 100.0).round() / 100.0,
        "opaque": img.is_opaque(),
        "tiles": img.tiles_across(TILE_SIZE) * img.tiles_down(TILE_SIZE),
    });
    summ.push(Chunk::new(STAT, serde_json::to_vec(&stat)?));

    let colours: Vec<serde_json::Value> = palette
        .iter()
        .map(|([r, g, b, a], n)| {
            serde_json::json!({
                "hex": format!("#{r:02x}{g:02x}{b:02x}"),
                "alpha": a,
                "share": (*n as f64 / total.max(1) as f64 * 1000.0).round() / 10.0,
            })
        })
        .collect();
    summ.push(Chunk::new(PALT, serde_json::to_vec(&colours)?));

    // A PNG here rather than raw pixels: the thumbnail is the one part of
    // the file meant to be handed straight to something else to display.
    summ.push(Chunk::stored(THMB, to_png(&img.thumbnail(THUMB_MAX))?));
    Ok(summ)
}

fn render_summary(file: &WickFile, out: &mut dyn std::io::Write) -> Result<()> {
    let Some(summ) = file.summary()? else {
        return Err(Error::MissingChunk("SUMM"));
    };
    if let Some(stat) = summ.get(STAT) {
        let v: serde_json::Value = stat.as_json()?;
        writeln!(
            out,
            "{}x{} px ({} MP), {}",
            v["width"],
            v["height"],
            v["megapixels"],
            if v["opaque"].as_bool().unwrap_or(false) {
                "opaque"
            } else {
                "with alpha"
            }
        )?;
    }
    if let Some(p) = summ.get(PALT) {
        let colours: Vec<serde_json::Value> = serde_json::from_slice(&p.value)?;
        writeln!(out, "palette:")?;
        for c in colours.iter().take(6) {
            writeln!(
                out,
                "  {}  {}%",
                c["hex"].as_str().unwrap_or(""),
                c["share"]
            )?;
        }
    }
    if let Some(t) = summ.get(THMB) {
        writeln!(out, "thumbnail: {} bytes of PNG", t.value.len())?;
    }
    Ok(())
}

/// The `SUMM` thumbnail as PNG bytes, without decoding `DATA`.
pub fn thumbnail_png(file: &WickFile) -> Result<Option<Vec<u8>>> {
    Ok(file
        .summary()?
        .and_then(|s| s.get(THMB).map(|c| c.value.clone())))
}

/// Attach a vector overlay. Stored and preserved; not rasterised by this
/// build.
pub fn set_vector_layer(file: &mut WickFile, svg: &str) -> Result<()> {
    let mut data = file.data()?;
    data.set(Chunk::text(VECT, svg));
    file.set_data(&data)
}

/// Append an edit-history note to the payload. Distinct from `PROV`: this
/// records *what was drawn*, where provenance records what tool touched the
/// file and proves it.
pub fn record_edit(file: &mut WickFile, note: &str) -> Result<()> {
    let mut data = file.data()?;
    data.push(Chunk::json(
        EDIT,
        &serde_json::json!({"at": libwick::time::now_rfc3339(), "note": note}),
    )?);
    file.set_data(&data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(w: u32, h: u32) -> Image {
        let mut img = Image::new(w, h);
        for y in 0..h {
            for x in 0..w {
                img.set_pixel(
                    x,
                    y,
                    [(x * 3) as u8, (y * 5) as u8, ((x + y) * 2) as u8, 255],
                );
            }
        }
        img
    }

    fn build(img: &Image) -> WickFile {
        let png = to_png(img).unwrap();
        let mut f = WickFile::new(TAG);
        let p = Emi.import(&Source::new(&png, "sample.png", "png")).unwrap();
        f.set_data(&p.data).unwrap();
        if let Some(s) = p.summary {
            f.set_summary(&s).unwrap();
        }
        f
    }

    #[test]
    fn png_round_trips_pixel_for_pixel() {
        for (w, h) in [(1, 1), (64, 64), (150, 90), (65, 3)] {
            let img = gradient(w, h);
            let f = build(&img);
            let back = from_png(&Emi.export(&f, "png").unwrap()).unwrap();
            assert_eq!(back, img, "{w}x{h} did not survive");
        }
    }

    #[test]
    fn transparency_survives() {
        let mut img = gradient(32, 32);
        img.set_pixel(5, 5, [10, 20, 30, 128]);
        let f = build(&img);
        let back = from_png(&Emi.export(&f, "png").unwrap()).unwrap();
        assert_eq!(back.pixel(5, 5), [10, 20, 30, 128]);
        assert!(!back.is_opaque());
    }

    #[test]
    fn the_grid_covers_the_image_exactly() {
        let img = gradient(150, 90);
        let f = build(&img);
        // 150/64 -> 3 across, 90/64 -> 2 down.
        assert_eq!(f.data().unwrap().all(TILE).count(), 6);
        assert!(Emi.validate(&f).unwrap().is_empty());
    }

    #[test]
    fn patching_a_region_touches_only_its_tiles() {
        let img = gradient(200, 200);
        let mut f = build(&img);
        let before: Vec<Vec<u8>> = f
            .data()
            .unwrap()
            .all(TILE)
            .map(|c| c.value.clone())
            .collect();

        let mut red = Image::new(8, 8);
        for y in 0..8 {
            for x in 0..8 {
                red.set_pixel(x, y, [255, 0, 0, 255]);
            }
        }
        // Entirely inside tile (1,1).
        let touched = patch(&mut f, 70, 70, &red).unwrap();
        assert_eq!(touched, vec![(1, 1)]);

        let after: Vec<Vec<u8>> = f
            .data()
            .unwrap()
            .all(TILE)
            .map(|c| c.value.clone())
            .collect();
        let differing = before.iter().zip(&after).filter(|(a, b)| a != b).count();
        assert_eq!(differing, 1, "one tile should have changed");

        let (out, _) = image(&f).unwrap();
        assert_eq!(out.pixel(70, 70), [255, 0, 0, 255]);
        assert_eq!(out.pixel(69, 70), img.pixel(69, 70));
        assert_eq!(out.pixel(78, 70), img.pixel(78, 70));
    }

    #[test]
    fn a_patch_across_a_tile_boundary_updates_both() {
        let img = gradient(200, 200);
        let mut f = build(&img);
        let white = Image {
            width: 4,
            height: 4,
            pixels: vec![255; 64],
        };
        // Straddles x = 64, the boundary between tile column 0 and 1.
        let touched = patch(&mut f, 62, 10, &white).unwrap();
        assert_eq!(touched, vec![(0, 0), (1, 0)]);
        let (out, _) = image(&f).unwrap();
        assert_eq!(out.pixel(62, 10), [255, 255, 255, 255]);
        assert_eq!(out.pixel(65, 13), [255, 255, 255, 255]);
    }

    #[test]
    fn a_diff_reports_where_the_image_changed() {
        let img = gradient(200, 200);
        let a = build(&img);
        let mut b = build(&img);
        let dot = Image {
            width: 2,
            height: 2,
            pixels: vec![0; 16],
        };
        patch(&mut b, 130, 5, &dot).unwrap();

        let d = Emi.diff(&a, &b, &KeyRing::empty()).unwrap();
        assert_eq!(d.len(), 2, "{d:?}"); // one tile plus the summary line
        assert_eq!(d[0].path, "tile (2,0)");
        assert!(d[0].note.contains("pixels 128,0"), "{}", d[0].note);
        assert!(d[1].note.contains("1 of 16 tiles"), "{}", d[1].note);
    }

    #[test]
    fn identical_images_diff_to_nothing() {
        let img = gradient(100, 100);
        assert!(Emi
            .diff(&build(&img), &build(&img), &KeyRing::empty())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_resize_is_reported_without_tile_noise() {
        let d = Emi
            .diff(
                &build(&gradient(100, 100)),
                &build(&gradient(120, 100)),
                &KeyRing::empty(),
            )
            .unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].path, "size");
    }

    #[test]
    fn a_missing_tile_is_caught() {
        let mut f = build(&gradient(150, 90));
        let mut data = f.data().unwrap();
        let at = data.0.iter().position(|c| c.ty == TILE).unwrap();
        data.0.remove(at);
        f.set_data(&data).unwrap();
        let issues = Emi.validate(&f).unwrap();
        assert!(
            issues.iter().any(|i| i.message.contains("is missing")),
            "{issues:?}"
        );
    }

    #[test]
    fn the_thumbnail_is_a_tiny_fraction_of_the_payload() {
        let f = build(&gradient(600, 400));
        let thumb = thumbnail_png(&f).unwrap().unwrap();
        let data = f.chunks.get(ChunkType::DATA).unwrap().value.len();
        assert!(
            thumb.len() * 20 < data,
            "thumb {} vs data {data}",
            thumb.len()
        );
        // And it is a real PNG, decodable on its own.
        let decoded = from_png(&thumb).unwrap();
        assert!(decoded.width <= THUMB_MAX && decoded.height <= THUMB_MAX);
        assert_eq!(decoded.width, THUMB_MAX);
    }

    #[test]
    fn the_palette_is_deterministic() {
        let img = gradient(80, 80);
        assert_eq!(img.palette(8), img.palette(8));
    }

    #[test]
    fn jpeg_import_is_declined_with_a_reason() {
        let err = match Emi.import(&Source::new(b"\xFF\xD8\xFF", "photo.jpg", "jpg")) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("JPEG was accepted"),
        };
        assert!(err.contains("not implemented"), "{err}");
        assert!(err.contains("artefacts"), "{err}");
    }

    #[test]
    fn a_vector_layer_and_edits_are_preserved() {
        let mut f = build(&gradient(64, 64));
        set_vector_layer(&mut f, "<svg/>").unwrap();
        record_edit(&mut f, "cropped the top").unwrap();
        let data = f.data().unwrap();
        assert_eq!(data.get(VECT).unwrap().as_str().unwrap(), "<svg/>");
        assert_eq!(data.all(EDIT).count(), 1);
        // And the raster is untouched.
        assert_eq!(image(&f).unwrap().0, gradient(64, 64));
    }
}
