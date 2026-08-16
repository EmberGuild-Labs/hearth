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
//! Pixels are stored as RGBA8 and compressed by the chunk layer, rather than
//! as a PNG per tile: the container already compresses, and a tile stored as
//! pixels is a tile that can be patched without a decode-modify-re-encode
//! cycle.
//!
//! Each tile's bytes are delta-filtered first — PNG's Paeth predictor, chosen
//! per tile against storing the pixels as they are, whichever comes out
//! smaller. Without it a photograph cost 40% more as `.emi` than as the PNG
//! it came from, because a compressor matches repeated bytes and a
//! photograph does not repeat, it *drifts*. With it the same photograph lands
//! within a few percent of its PNG. See [`encode_tile`].
//!
//! It does not always beat PNG, and does not try to: a tile is compressed
//! alone, so there is none of the cross-image context a single PNG stream
//! gets. That is the price of a payload that can be patched and diffed a
//! region at a time, and it is stated rather than hidden.
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

/// Longest edge of the `SUMM` thumbnail, for an image large enough to want
/// one. See [`thumb_edge`] for what happens to a small one.
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

    /// Downsample by averaging whole source rectangles, in linear light.
    ///
    /// The obvious implementation takes the nearest source pixel for each
    /// destination pixel. That was here first, and it is why thumbnails
    /// looked worse than the images they came from: shrinking 1024px to
    /// 128px keeps one pixel in sixty-four and discards the other
    /// sixty-three, so text and fine detail come back as aliased speckle,
    /// which whatever displays it then smooths into blur. Averaging the
    /// whole rectangle keeps that detail as tone.
    ///
    /// Two details are easy to get wrong and visible when they are:
    ///
    /// * The average is taken in linear light. sRGB bytes lie along a curve,
    ///   so averaging them directly darkens every edge — black and white in
    ///   equal measure average to 128, a good deal darker than the grey a
    ///   squint actually sees.
    /// * Colour is weighted by alpha, so a fully transparent pixel cannot
    ///   drag whatever colour it happens to carry into the average and leave
    ///   a fringe around a cut-out.
    ///
    /// Source rectangles are laid out in integer arithmetic, so they tile the
    /// image exactly at any ratio: every pixel is counted once.
    pub fn thumbnail(&self, max_edge: u32) -> Image {
        let long = self.width.max(self.height);
        if max_edge == 0 || long <= max_edge {
            // Not a downsample. Copying is not merely the faster path, it is
            // the only one that guarantees the bytes are unchanged: a round
            // trip out to linear light and back moves some of them by one.
            return self.clone();
        }
        let span = |i: u32, n_out: u32, n_in: u32| -> (u32, u32) {
            let lo = (i as u64 * n_in as u64 / n_out as u64) as u32;
            let hi = (((i as u64 + 1) * n_in as u64 / n_out as u64) as u32).max(lo + 1);
            (lo, hi)
        };

        let w = ((self.width as u64 * max_edge as u64 / long as u64) as u32).max(1);
        let h = ((self.height as u64 * max_edge as u64 / long as u64) as u32).max(1);
        let linear = srgb_to_linear();
        let mut out = Image::new(w, h);

        for y in 0..h {
            let (y0, y1) = span(y, h, self.height);
            for x in 0..w {
                let (x0, x1) = span(x, w, self.width);
                let (mut r, mut g, mut b, mut a) = (0f32, 0f32, 0f32, 0f32);
                for sy in y0..y1 {
                    let row = (sy as usize) * (self.width as usize);
                    for sx in x0..x1 {
                        let i = (row + sx as usize) * 4;
                        let p = &self.pixels[i..i + 4];
                        let weight = p[3] as f32 / 255.0;
                        r += linear[p[0] as usize] * weight;
                        g += linear[p[1] as usize] * weight;
                        b += linear[p[2] as usize] * weight;
                        a += weight;
                    }
                }
                let count = ((x1 - x0) as f32) * ((y1 - y0) as f32);
                let px = if a > 0.0 {
                    [
                        linear_to_srgb(r / a),
                        linear_to_srgb(g / a),
                        linear_to_srgb(b / a),
                        ((a / count) * 255.0).round() as u8,
                    ]
                } else {
                    // Every contributing pixel was transparent, so there is
                    // no colour to keep and nothing to average.
                    [0, 0, 0, 0]
                };
                out.set_pixel(x, y, px);
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

/// sRGB byte to linear intensity, one entry per possible byte. A table
/// because the alternative is a `powf` per channel per source pixel, and a
/// thumbnail of a large image reads every one of them.
fn srgb_to_linear() -> &'static [f32; 256] {
    static TABLE: std::sync::OnceLock<[f32; 256]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [0f32; 256];
        for (i, v) in t.iter_mut().enumerate() {
            let c = i as f32 / 255.0;
            *v = if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            };
        }
        t
    })
}

fn linear_to_srgb(v: f32) -> u8 {
    let c = v.clamp(0.0, 1.0);
    let s = if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0).round() as u8
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

/// Stored verbatim: the bytes are the pixels.
const FILTER_NONE: u8 = 0;
/// Each byte is stored as its difference from PNG's Paeth predictor.
const FILTER_PAETH: u8 = 1;

/// PNG's Paeth predictor: of the pixel to the left, the one above and the one
/// diagonally up-left, pick whichever is closest to their linear estimate.
///
/// Byte-for-byte the function from the PNG specification, and for the same
/// reason: it is the cheapest predictor that handles both a horizontal edge
/// and a vertical one, which is most of what an image is made of.
fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i16 + b as i16 - c as i16;
    let (pa, pb, pc) = (
        (p - a as i16).abs(),
        (p - b as i16).abs(),
        (p - c as i16).abs(),
    );
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// The three neighbours of byte `i`, treating anything off the top or left
/// edge of the tile as zero. `stride` is one row of the tile in bytes.
fn neighbours(bytes: &[u8], i: usize, stride: usize) -> (u8, u8, u8) {
    let left = if i % stride >= 4 { bytes[i - 4] } else { 0 };
    let up = if i >= stride { bytes[i - stride] } else { 0 };
    let up_left = if i >= stride && i % stride >= 4 {
        bytes[i - stride - 4]
    } else {
        0
    };
    (left, up, up_left)
}

fn filter_paeth(pixels: &[u8], stride: usize) -> Vec<u8> {
    let mut out = vec![0u8; pixels.len()];
    for i in 0..pixels.len() {
        let (a, b, c) = neighbours(pixels, i, stride);
        out[i] = pixels[i].wrapping_sub(paeth(a, b, c));
    }
    out
}

fn unfilter_paeth(deltas: &[u8], stride: usize) -> Vec<u8> {
    let mut out = vec![0u8; deltas.len()];
    // In place of the source, because each prediction is made from pixels
    // already reconstructed — the same order the filter ran in.
    for i in 0..deltas.len() {
        let (a, b, c) = neighbours(&out, i, stride);
        out[i] = deltas[i].wrapping_add(paeth(a, b, c));
    }
    out
}

/// `[tx u16][ty u16][w u16][h u16][filter u8][rows]`
///
/// The filter byte is what makes `.emi` competitive with PNG on photographs.
/// Raw RGBA handed to the chunk layer was costing 40% against the PNG a file
/// was converted from, because a compressor matches repeated byte strings
/// while a photograph's structure is *gradual*: each pixel resembles its
/// neighbours without repeating them. Subtracting a prediction turns that
/// resemblance into small numbers near zero, which is the thing a compressor
/// is good at. On a 1.7 MB photograph the payload drops by a quarter.
///
/// It is chosen per tile rather than applied everywhere, because the two
/// cases genuinely disagree. Flat artwork — an icon, a screenshot of a solid
/// background — is *already* long runs of identical bytes, which is the best
/// case there is; predicting it produces runs of zeros that compress no
/// better, and sometimes slightly worse. So each tile is stored both ways and
/// the smaller one wins, asked of [`libwick::chunks::stored_size`] so that
/// the answer comes from the layer that will actually do the compressing.
///
/// **Tiles written before the filter byte existed have no filter byte**, so
/// they are one byte shorter than the same tile is now. That is how
/// [`decode_tile`] tells the two apart, and why a `.emi` written by an
/// earlier build still opens.
fn encode_tile(img: &Image, tx: u32, ty: u32, tile: u32) -> Chunk {
    let x0 = tx * tile;
    let y0 = ty * tile;
    let w = tile.min(img.width - x0);
    let h = tile.min(img.height - y0);

    let mut pixels = Vec::with_capacity((w * h * 4) as usize);
    for y in y0..y0 + h {
        let start = ((y as usize) * (img.width as usize) + x0 as usize) * 4;
        pixels.extend_from_slice(&img.pixels[start..start + (w as usize) * 4]);
    }
    tile_chunk(tx, ty, w, h, &pixels)
}

/// Assemble a tile chunk from a rectangle of pixels, stored whichever way
/// comes out smaller once the chunk layer has compressed it.
fn tile_chunk(tx: u32, ty: u32, w: u32, h: u32, pixels: &[u8]) -> Chunk {
    let deltas = filter_paeth(pixels, (w as usize) * 4);
    let (filter, body) =
        if libwick::chunks::stored_size(&deltas) < libwick::chunks::stored_size(pixels) {
            (FILTER_PAETH, &deltas)
        } else {
            (FILTER_NONE, &pixels.to_vec())
        };

    let mut v = Vec::with_capacity(9 + body.len());
    v.extend_from_slice(&(tx as u16).to_le_bytes());
    v.extend_from_slice(&(ty as u16).to_le_bytes());
    v.extend_from_slice(&(w as u16).to_le_bytes());
    v.extend_from_slice(&(h as u16).to_le_bytes());
    v.push(filter);
    v.extend_from_slice(body);
    Chunk::new(TILE, v)
}

fn tile_coords(c: &Chunk) -> Result<(u32, u32, u32, u32)> {
    if c.value.len() < 8 {
        return Err(Error::Truncated("TILE header"));
    }
    let n = |i: usize| u16::from_le_bytes([c.value[i], c.value[i + 1]]) as u32;
    Ok((n(0), n(2), n(4), n(6)))
}

/// A tile's coordinates and its pixels, whichever way it was stored.
fn decode_tile(c: &Chunk) -> Result<(u32, u32, u32, u32, Vec<u8>)> {
    let (tx, ty, w, h) = tile_coords(c)?;
    let n = (w as usize) * (h as usize) * 4;
    let stride = (w as usize) * 4;

    let pixels = match c.value.len() {
        // Written before the filter byte existed.
        len if len == 8 + n => c.value[8..].to_vec(),
        len if len == 9 + n => match c.value[8] {
            FILTER_NONE => c.value[9..].to_vec(),
            FILTER_PAETH => unfilter_paeth(&c.value[9..], stride),
            other => {
                return Err(Error::Other(format!(
                    "tile ({tx},{ty}) uses filter {other}, which this build does not know. \
                     The file was written by a newer Hearth"
                )))
            }
        },
        len => {
            return Err(Error::Other(format!(
                "tile ({tx},{ty}) holds {len} bytes, expected {} or {}",
                8 + n,
                9 + n
            )))
        }
    };
    Ok((tx, ty, w, h, pixels))
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
        let (tx, ty, w, h, pixels) = decode_tile(c)?;
        let (x0, y0) = (tx * header.tile, ty * header.tile);
        if x0 + w > img.width || y0 + h > img.height {
            return Err(Error::Other(format!(
                "tile ({tx},{ty}) claims pixels outside a {}x{} image",
                img.width, img.height
            )));
        }
        for row in 0..h {
            let src = (row as usize) * (w as usize) * 4;
            let dst = (((y0 + row) as usize) * (img.width as usize) + x0 as usize) * 4;
            img.pixels[dst..dst + (w as usize) * 4]
                .copy_from_slice(&pixels[src..src + (w as usize) * 4]);
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

        // Overlapping tiles are decoded, painted and re-encoded. Editing the
        // stored bytes in place stopped being possible when they became
        // deltas: one changed pixel changes the prediction for the next.
        let (_, _, _, _, before) = decode_tile(c)?;
        let mut pixels = before.clone();
        for row in 0..th {
            for col in 0..tw {
                let (px, py) = (x0 + col, y0 + row);
                if px < x || py < y || px >= x + patch.width || py >= y + patch.height {
                    continue;
                }
                let src = patch.pixel(px - x, py - y);
                let dst = ((row as usize) * (tw as usize) + col as usize) * 4;
                pixels[dst..dst + 4].copy_from_slice(&src);
            }
        }
        if pixels != before {
            touched.push((tx, ty));
            out.push(tile_chunk(tx, ty, tw, th, &pixels));
        } else {
            // Unchanged pixels keep their existing bytes, so a patch that
            // paints a tile the colour it already was leaves the file alone —
            // including a tile still stored in the older unfiltered form.
            out.push(c.clone());
        }
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

        // Hash the pixels, not the stored bytes. Two tiles holding the same
        // picture must compare equal even when one of them was written before
        // tiles were delta-filtered, or a change of encoding would report
        // itself as a change of image.
        let index = |d: &ChunkList| -> Result<std::collections::HashMap<(u32, u32), [u8; 32]>> {
            d.all(TILE)
                .map(|c| {
                    let (tx, ty, _, _, pixels) = decode_tile(c)?;
                    Ok(((tx, ty), *blake3::hash(&pixels).as_bytes()))
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
    summ.push(Chunk::stored(
        THMB,
        to_png(&img.thumbnail(thumb_edge(img.width.max(img.height))))?,
    ));
    Ok(summ)
}

/// How large a thumbnail to keep for an image whose long edge is `long`.
///
/// [`THUMB_MAX`] alone is the wrong answer for a small image, and it was
/// costing real bytes: a 160×84 photograph got a 128×67 thumbnail, which is
/// the same picture again. Stored as PNG it came to 16 KB beside a 27 KB
/// payload — more than half the file spent summarising something already
/// small enough to read. A summary that size is not a summary.
///
/// So the thumbnail is also capped at a third of the source, which changes
/// nothing above 384px — every photograph and screenshot still gets the full
/// 128 — and stops a small image from carrying a second copy of itself.
/// Below about 48px there is nothing left to divide, and the whole image is
/// a few hundred bytes anyway, so it is kept as it is.
fn thumb_edge(long: u32) -> u32 {
    let third = long / 3;
    if third < 16 {
        long
    } else {
        THUMB_MAX.min(third)
    }
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

/// The image's dimensions as recorded in `SUMM`, without decoding `DATA`.
///
/// Enough for a caller to decide whether decoding the payload is worth it
/// before it commits to doing so, which is the whole point of the tier.
pub fn summary_size(file: &WickFile) -> Result<Option<(u32, u32)>> {
    let Some(summ) = file.summary()? else {
        return Ok(None);
    };
    let Some(stat) = summ.get(STAT) else {
        return Ok(None);
    };
    let v: serde_json::Value = stat.as_json()?;
    match (v["width"].as_u64(), v["height"].as_u64()) {
        (Some(w), Some(h)) => Ok(Some((w as u32, h as u32))),
        _ => Ok(None),
    }
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

    /// Content a compressor cannot do anything with, so that a test about
    /// the *ratio* between two parts of a file is not really a test about
    /// how well one of them happened to compress.
    fn noise(w: u32, h: u32) -> Image {
        let mut img = Image::new(w, h);
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        };
        for y in 0..h {
            for x in 0..w {
                img.set_pixel(x, y, [next(), next(), next(), 255]);
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
        // Noise rather than a gradient: since tiles are delta-filtered, a
        // gradient's payload compresses to almost nothing, and comparing a
        // thumbnail against it would measure the gradient, not the tier.
        let f = build(&noise(600, 400));
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

    /// A tile as it was stored before the filter byte existed: the same
    /// header, then the pixels themselves.
    fn legacy_tile(img: &Image, tx: u32, ty: u32, tile: u32) -> Chunk {
        let (x0, y0) = (tx * tile, ty * tile);
        let w = tile.min(img.width - x0);
        let h = tile.min(img.height - y0);
        let mut v = Vec::new();
        for (i, n) in [tx, ty, w, h].iter().enumerate() {
            let _ = i;
            v.extend_from_slice(&(*n as u16).to_le_bytes());
        }
        for y in y0..y0 + h {
            let start = ((y as usize) * (img.width as usize) + x0 as usize) * 4;
            v.extend_from_slice(&img.pixels[start..start + (w as usize) * 4]);
        }
        Chunk::new(TILE, v)
    }

    #[test]
    fn a_filtered_tile_gives_back_the_pixels_it_was_given() {
        // Including the awkward shapes: a tile narrower than the grid, one
        // row, one column, one pixel. The filter reads a row back and a
        // pixel left, so an edge is where it goes wrong if it is wrong.
        for (w, h) in [(64, 64), (65, 3), (1, 40), (40, 1), (1, 1), (150, 90)] {
            let img = gradient(w, h);
            let data = encode(
                &img,
                &ImageHeader {
                    width: w,
                    height: h,
                    tile: TILE_SIZE,
                    source: None,
                    opaque: true,
                },
            )
            .unwrap();
            assert_eq!(decode(&data).unwrap().0, img, "{w}x{h}");
        }
    }

    #[test]
    fn a_tile_written_before_filtering_still_reads() {
        // A .emi from an earlier build has tiles one byte shorter, with no
        // filter byte at all. Length is what tells them apart, and an old
        // file must keep opening.
        let img = gradient(100, 70);
        let mut data = ChunkList::new();
        data.push(Chunk::new(
            IMHD,
            serde_json::to_vec(&ImageHeader {
                width: 100,
                height: 70,
                tile: TILE_SIZE,
                source: None,
                opaque: true,
            })
            .unwrap(),
        ));
        for ty in 0..img.tiles_down(TILE_SIZE) {
            for tx in 0..img.tiles_across(TILE_SIZE) {
                data.push(legacy_tile(&img, tx, ty, TILE_SIZE));
            }
        }
        assert_eq!(decode(&data).unwrap().0, img);
    }

    #[test]
    fn the_same_picture_stored_two_ways_diffs_to_nothing() {
        // The reason the diff hashes pixels rather than stored bytes: a file
        // written before filtering and one written after hold different
        // bytes for an identical image, and reporting that as an edit would
        // make the first diff after an upgrade useless.
        let img = gradient(100, 70);
        let header = ImageHeader {
            width: 100,
            height: 70,
            tile: TILE_SIZE,
            source: None,
            opaque: true,
        };
        let mut old = ChunkList::new();
        old.push(Chunk::new(IMHD, serde_json::to_vec(&header).unwrap()));
        for ty in 0..img.tiles_down(TILE_SIZE) {
            for tx in 0..img.tiles_across(TILE_SIZE) {
                old.push(legacy_tile(&img, tx, ty, TILE_SIZE));
            }
        }
        let mut a = WickFile::new(TAG);
        a.set_data(&old).unwrap();
        let b = build(&img);
        assert!(
            Emi.diff(&a, &b, &KeyRing::default()).unwrap().is_empty(),
            "the same image, stored two ways, reported as changed"
        );
    }

    #[test]
    fn an_unknown_filter_is_refused_rather_than_guessed() {
        let img = gradient(20, 20);
        let mut v = encode_tile(&img, 0, 0, TILE_SIZE).value;
        v[8] = 99;
        let err = decode_tile(&Chunk::new(TILE, v)).unwrap_err().to_string();
        assert!(err.contains("filter 99"), "{err}");
        assert!(err.contains("newer Hearth"), "{err}");
    }

    #[test]
    fn filtering_earns_its_place() {
        // The whole reason the filter byte exists, measured through the code
        // that actually stores the bytes rather than a stand-in for it. Noise
        // is the case a predictor cannot help with, so this is the case it
        // can: an image whose pixels resemble their neighbours without
        // repeating them, which is what a photograph is.
        let img = gradient(256, 256);
        let header = ImageHeader {
            width: 256,
            height: 256,
            tile: TILE_SIZE,
            source: None,
            opaque: true,
        };
        let mut raw = ChunkList::new();
        for ty in 0..img.tiles_down(TILE_SIZE) {
            for tx in 0..img.tiles_across(TILE_SIZE) {
                raw.push(legacy_tile(&img, tx, ty, TILE_SIZE));
            }
        }
        let keys = KeyRing::default();
        let stored_raw = raw.encode(&keys).unwrap().len();
        let mut filtered = ChunkList::new();
        for c in encode(&img, &header).unwrap().all(TILE) {
            filtered.push(c.clone());
        }
        let stored_filtered = filtered.encode(&keys).unwrap().len();
        assert!(
            stored_filtered * 2 < stored_raw,
            "filtered {stored_filtered} vs raw {stored_raw} — \
             the filter is not paying for itself"
        );
    }

    #[test]
    fn a_thumbnail_averages_its_pixels_instead_of_picking_one() {
        // A checkerboard is the case nearest-neighbour gets worst: whichever
        // pixel it lands on, the answer is pure black or pure white and the
        // other half of the image is gone.
        let mut img = Image::new(64, 64);
        for y in 0..64 {
            for x in 0..64 {
                let v = if (x + y) % 2 == 0 { 255 } else { 0 };
                img.set_pixel(x, y, [v, v, v, 255]);
            }
        }
        let t = img.thumbnail(1);
        let [r, g, b, a] = t.pixel(0, 0);
        assert_eq!((t.width, t.height, a), (1, 1, 255));
        assert_eq!([r, g, b], [r, r, r]);
        // Averaged in linear light, half black and half white is sRGB 188,
        // not the 128 that averaging the bytes directly would give. 128 here
        // would mean the gamma correction had been dropped.
        assert!((186..=190).contains(&r), "half-and-half grey came out {r}");
    }

    #[test]
    fn a_thumbnail_no_smaller_than_the_image_is_the_image() {
        let img = gradient(40, 30);
        assert_eq!(img.thumbnail(40), img);
        assert_eq!(img.thumbnail(400), img);
    }

    #[test]
    fn transparent_pixels_do_not_tint_the_ones_beside_them() {
        // Two pixels: opaque blue, and red that is not there at all. The red
        // is invisible, so the average is blue at half alpha. Weighting the
        // colours by alpha is what keeps the red out; without it the result
        // is a muddy purple, which is the fringe you see around a badly
        // scaled cut-out.
        let mut img = Image::new(2, 1);
        img.set_pixel(0, 0, [0, 0, 255, 255]);
        img.set_pixel(1, 0, [255, 0, 0, 0]);
        let [r, g, b, a] = img.thumbnail(1).pixel(0, 0);
        assert_eq!((r, g, b), (0, 0, 255));
        assert!((126..=129).contains(&a), "alpha came out {a}");
    }

    #[test]
    fn a_small_image_does_not_carry_a_second_copy_of_itself() {
        // The thumbnail of a 150px image at the full 128 would be the same
        // picture again, and on detailed content that doubled the file.
        let f = build(&noise(150, 100));
        let thumb = thumbnail_png(&f).unwrap().unwrap();
        let data = f.chunks.get(ChunkType::DATA).unwrap().value.len();
        assert!(
            thumb.len() * 4 < data,
            "thumb {} vs data {data}",
            thumb.len()
        );
        assert_eq!(from_png(&thumb).unwrap().width, 50);
        // And a large one is unaffected: the cap only bites below 384px.
        assert_eq!(thumb_edge(1600), THUMB_MAX);
        assert_eq!(thumb_edge(384), THUMB_MAX);
        // Nothing left to divide: a 30px image is kept whole.
        assert_eq!(thumb_edge(30), 30);
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
