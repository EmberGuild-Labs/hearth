//! A self-contained preview of any Wick file.
//!
//! This exists for Quick Look. macOS asks an extension "what is in this
//! file" and expects an answer in milliseconds, for a file it may never
//! open; that is the summary tier's job description, written by somebody
//! else. So the preview reads `SUMM` and renders from it, and only decodes
//! the payload when asked to (`--full`), when a file carries no summary at
//! all, or — for an image — to draw the picture itself, which is the one
//! answer to "what is in this file" that a summary cannot give at the size
//! a pane wants it. See [`picture`].
//!
//! The HTML is one file with no external references — no fonts, no scripts,
//! no images fetched over the network — because a Quick Look extension is
//! sandboxed and a preview that needs the network is a preview that shows a
//! blank pane on an aeroplane.

use libwick::chunks::ChunkType;
use libwick::plugin::{Plugin, RenderOpts};
use libwick::{Peek, WickFile};
use std::fmt::Write as _;
use std::path::Path;

pub struct Opts {
    /// Decode the payload instead of rendering from the summary tier.
    pub full: bool,
    /// Wrapping width for the plugin's renderer.
    pub width: usize,
}

/// Longest edge, in pixels, of the picture embedded in an HTML preview.
///
/// A Quick Look pane is around 800pt wide, which is 1600 device pixels on
/// every Mac sold this decade. 1600 is therefore the point past which more
/// pixels buy nothing but base64.
const DISPLAY_MAX: u32 = 1600;

/// How large an image the preview will decode to show it properly. Past
/// this, the summary thumbnail is used and the page says so — a preview is
/// supposed to be the cheap way to look at a file, and holding 100 megapixels
/// of RGBA to draw a two-inch pane is not that.
const DECODE_BUDGET_PIXELS: u64 = 40_000_000;

/// The picture an image preview shows, and where it came from.
struct Picture {
    png: Vec<u8>,
    /// Natural size of `png`, which is what it is displayed at.
    width: u32,
    height: u32,
    /// Integer factor to draw it at. Only ever above 1 for images small
    /// enough that native size would be a postage stamp, and integer so that
    /// enlarging a 16px icon stays crisp instead of being interpolated.
    zoom: u32,
    /// Said under the picture, because "this is a 128px thumbnail of a 4000px
    /// photograph" is the difference between a blurry preview and an honest
    /// one.
    caption: String,
    /// True when the pixels came from `DATA` rather than from `SUMM`.
    decoded: bool,
}

/// What a preview is made of, gathered once and then written out as either
/// HTML or text.
struct Preview {
    name: String,
    ext: String,
    format: String,
    tag: String,
    spec: String,
    size: String,
    flags: Vec<String>,
    chunks: Vec<(String, String)>,
    /// True when the body came from `SUMM` rather than from `DATA`.
    tiered: bool,
    body: String,
    /// The picture, for formats that have one to show for themselves.
    image: Option<Picture>,
    provenance: Option<String>,
    /// Why the body is not what was asked for, when it is not.
    note: Option<String>,
}

/// Pick the picture for an `.emi` preview: the raster itself where that is
/// affordable, the summary thumbnail where it is not.
///
/// The summary thumbnail alone was what this used to embed, and it is why a
/// converted image looked worse than the PNG it came from: 128 pixels
/// stretched across an 800-point pane is 128 pixels stretched across an
/// 800-point pane, however good the thumbnail is. So the payload is decoded
/// when it is cheap enough to decode — an image preview showing the image is
/// worth one decode — and the page says which of the two it got.
fn picture(file: &WickFile, path: &Path) -> anyhow::Result<Option<Picture>> {
    let summary_size = wick_emi::summary_size(file)?;
    let affordable = match summary_size {
        Some((w, h)) => (w as u64) * (h as u64) <= DECODE_BUDGET_PIXELS,
        // No summary tier to ask, so the payload is being read anyway.
        None => true,
    };

    if affordable {
        let raster = wick_emi::image(file).or_else(|_| {
            // A tiered read stops before DATA on purpose. This is the one
            // caller that wants it, so ask for the whole file.
            WickFile::read(path).and_then(|f| wick_emi::image(&f))
        });
        if let Ok((img, _)) = raster {
            let shown = img.thumbnail(DISPLAY_MAX);
            let scaled = shown.width != img.width;
            let caption = if scaled {
                format!(
                    "{}×{}, drawn from the payload at {}×{}",
                    img.width, img.height, shown.width, shown.height
                )
            } else {
                format!("{}×{}, drawn from the payload", img.width, img.height)
            };
            return Ok(Some(Picture {
                zoom: zoom_for(shown.width.max(shown.height)),
                width: shown.width,
                height: shown.height,
                png: wick_emi::to_png(&shown)?,
                caption,
                decoded: true,
            }));
        }
    }

    // Sealed, truncated, or simply too large to be worth decoding. The
    // summary tier is exactly the fallback it was designed to be.
    let Some(png) = wick_emi::thumbnail_png(file)? else {
        return Ok(None);
    };
    let thumb = wick_emi::from_png(&png)?;
    let caption = match summary_size {
        Some((w, h)) => format!(
            "summary thumbnail, {}×{} of {w}×{h} — the payload was not decoded",
            thumb.width, thumb.height
        ),
        None => format!("summary thumbnail, {}×{}", thumb.width, thumb.height),
    };
    Ok(Some(Picture {
        zoom: zoom_for(thumb.width.max(thumb.height)),
        width: thumb.width,
        height: thumb.height,
        png,
        caption,
        decoded: false,
    }))
}

/// How many times to enlarge a small picture so it is visible in a pane
/// built for a page of text. Whole numbers only: doubling a 16px icon to
/// 32px keeps every pixel a square, where 1.7× would smear it.
fn zoom_for(long_edge: u32) -> u32 {
    match long_edge {
        0 => 1,
        n => (256 / n.max(1)).clamp(1, 8),
    }
}

/// `picture` is false for the text preview, which has nowhere to put an
/// image and should not pay for decoding one.
fn gather(
    plugin: &dyn Plugin,
    path: &Path,
    opts: &Opts,
    want_picture: bool,
) -> anyhow::Result<Preview> {
    let peek = Peek::open(path)?;

    let mut note = None;
    let tiered = if opts.full {
        false
    } else if peek.has(ChunkType::SUMM) {
        true
    } else {
        // A preview that silently showed something other than what it
        // claims to show would make the tier impossible to trust. Say so.
        note = Some("no summary tier; read from the payload".to_string());
        false
    };

    let render = RenderOpts {
        summary: tiered,
        width: opts.width,
        limit: None,
        color: false,
    };
    // Read only what the render will use. This matters most in exactly the
    // case Quick Look cares about: previewing a million-row table from its
    // summary tier should not first decompress the table.
    let file = WickFile::read_partial(path, |taken| plugin.enough(taken, &render))?;

    let mut body = Vec::new();
    plugin.render(&file, &render, &mut body)?;

    let provenance = {
        let chain = file.chain()?;
        let report = chain.verify();
        chain.entries().last().map(|last| {
            let state = match (&report.broken_at, report.signed) {
                (Some(_), _) => "chain broken".to_string(),
                (None, 0) => format!("{} entries, unsigned", report.entries),
                (None, n) => format!("{} entries, {n} signed", report.entries),
            };
            format!("{}  {}  ({state})", last.timestamp, last.action)
        })
    };

    // The image formats have an actual picture to show, and a preview pane
    // that showed a paragraph about an image instead of the image would be a
    // strange thing to build.
    let image = if want_picture && plugin.tag() == wick_emi::TAG {
        picture(&file, path)?
    } else {
        None
    };

    Ok(Preview {
        name: path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string(),
        ext: plugin.ext().to_string(),
        format: plugin.name().to_string(),
        tag: peek.tag().to_string(),
        spec: format!("Wick v{}", peek.version()),
        size: crate::human(peek.file_len as usize),
        flags: peek
            .header
            .flags
            .names()
            .iter()
            .map(|s| s.to_string())
            .collect(),
        chunks: peek
            .chunks
            .iter()
            .map(|c| (c.ty.to_string(), crate::human(c.stored as usize)))
            .collect(),
        tiered,
        body: String::from_utf8_lossy(&body).into_owned(),
        image,
        provenance,
        note,
    })
}

pub fn text(plugin: &dyn Plugin, path: &Path, opts: &Opts) -> anyhow::Result<String> {
    let p = gather(plugin, path, opts, false)?;
    let mut s = String::new();
    writeln!(s, "{}  —  .{} ({}), {}", p.name, p.ext, p.format, p.size)?;
    writeln!(
        s,
        "{}{}",
        p.spec,
        if p.flags.is_empty() {
            String::new()
        } else {
            format!("  ·  {}", p.flags.join(", "))
        }
    )?;
    if let Some(n) = &p.note {
        writeln!(s, "({n})")?;
    }
    writeln!(s)?;
    s.push_str(p.body.trim_end());
    s.push('\n');
    if let Some(prov) = &p.provenance {
        writeln!(s, "\n{prov}")?;
    }
    Ok(s)
}

pub fn html(plugin: &dyn Plugin, path: &Path, opts: &Opts) -> anyhow::Result<String> {
    let p = gather(plugin, path, opts, true)?;
    let accent = accent(&p.ext);
    let mut s = String::new();

    writeln!(s, "<!doctype html>")?;
    writeln!(s, "<html lang=\"en\"><head><meta charset=\"utf-8\">")?;
    writeln!(
        s,
        "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">"
    )?;
    writeln!(s, "<title>{}</title>", esc(&p.name))?;
    writeln!(s, "<style>{}</style>", css(accent))?;
    writeln!(s, "</head><body>")?;

    writeln!(s, "<header>")?;
    writeln!(s, "  <div class=\"tag\">{}</div>", esc(&p.tag))?;
    writeln!(s, "  <div class=\"names\">")?;
    writeln!(s, "    <h1>{}</h1>", esc(&p.name))?;
    writeln!(
        s,
        "    <p class=\"sub\">.{} · {} · {} · {}</p>",
        esc(&p.ext),
        esc(&p.format),
        esc(&p.size),
        esc(&p.spec)
    )?;
    writeln!(s, "  </div>")?;
    writeln!(s, "</header>")?;

    if !p.flags.is_empty() {
        writeln!(s, "<ul class=\"flags\">")?;
        for f in &p.flags {
            writeln!(s, "  <li>{}</li>", esc(f))?;
        }
        writeln!(s, "</ul>")?;
    }

    // Two sources, so two sentences where they disagree. "Read from the
    // summary tier" stops being true the moment the picture below it came
    // out of DATA, and a claim about cost that is only sometimes right is
    // worse than no claim at all.
    let decoded_picture = p.image.as_ref().is_some_and(|i| i.decoded);
    writeln!(
        s,
        "<p class=\"tier\">{}</p>",
        match (p.tiered, decoded_picture) {
            (true, false) => "read from the summary tier — the payload was never decoded",
            (true, true) =>
                "details read from the summary tier; the picture decoded from the payload",
            (false, _) => "read from the payload",
        }
    )?;
    if let Some(n) = &p.note {
        writeln!(s, "<p class=\"note\">{}</p>", esc(n))?;
    }

    if let Some(pic) = &p.image {
        // width and height are the picture's own, so the pane draws it at
        // one image pixel per pixel and never stretches it. `max-width` in
        // the stylesheet still shrinks it to fit a narrow pane, which loses
        // nothing: scaling down is the direction that keeps detail.
        writeln!(
            s,
            "<figure><img alt=\"preview\" width=\"{}\" height=\"{}\"{} src=\"data:image/png;base64,{}\">",
            pic.width * pic.zoom,
            pic.height * pic.zoom,
            if pic.zoom > 1 { " class=\"zoomed\"" } else { "" },
            base64(&pic.png)
        )?;
        writeln!(s, "  <figcaption>{}</figcaption>", esc(&pic.caption))?;
        writeln!(s, "</figure>")?;
    }

    writeln!(s, "<pre>{}</pre>", esc(p.body.trim_end()))?;

    writeln!(s, "<footer>")?;
    writeln!(s, "  <ul class=\"chunks\">")?;
    for (ty, len) in &p.chunks {
        writeln!(
            s,
            "    <li><b>{}</b> <span>{}</span></li>",
            esc(ty),
            esc(len)
        )?;
    }
    writeln!(s, "  </ul>")?;
    if let Some(prov) = &p.provenance {
        writeln!(s, "  <p class=\"prov\">{}</p>", esc(prov))?;
    }
    writeln!(s, "</footer>")?;
    writeln!(s, "</body></html>")?;
    Ok(s)
}

/// One hue per format, the same one the icon uses, so a preview pane and a
/// Finder icon agree about what kind of file this is.
fn accent(ext: &str) -> &'static str {
    match ext {
        "emd" => "#8360A8",
        "emi" => "#2E93A0",
        "emc" => "#3D6FA8",
        "emx" => "#4E9A51",
        _ => "#EF5F10",
    }
}

fn css(accent: &str) -> String {
    format!(
        "
:root {{
  --accent: {accent};
  --bg: #F3EAD9; --panel: #FFFFFF; --ink: #2A2230; --dim: #7C7480;
  --line: rgba(42,34,48,.14);
  color-scheme: light dark;
}}
@media (prefers-color-scheme: dark) {{
  :root {{ --bg: #191420; --panel: #221B29; --ink: #F3EAD9; --dim: #9A92A0;
           --line: rgba(243,234,217,.14); }}
}}
* {{ box-sizing: border-box; }}
body {{
  margin: 0; padding: 20px 22px 26px;
  background: var(--bg); color: var(--ink);
  font: 13px/1.55 -apple-system, BlinkMacSystemFont, 'SF Pro Text', system-ui, sans-serif;
}}
header {{ display: flex; align-items: center; gap: 12px; }}
.tag {{
  flex: none; width: 38px; height: 38px; border-radius: 9px;
  background: var(--accent); color: #fff;
  display: flex; align-items: center; justify-content: center;
  font: 600 14px/1 ui-monospace, SFMono-Regular, Menlo, monospace;
  letter-spacing: .5px;
}}
h1 {{ margin: 0; font-size: 15px; font-weight: 600; word-break: break-all; }}
.sub {{ margin: 2px 0 0; color: var(--dim); font-size: 11.5px; }}
.flags {{ list-style: none; display: flex; flex-wrap: wrap; gap: 6px; margin: 12px 0 0; padding: 0; }}
.flags li {{
  border: 1px solid var(--line); border-radius: 999px;
  padding: 1px 9px; font-size: 11px; color: var(--dim);
}}
.tier {{ margin: 12px 0 0; font-size: 11.5px; color: var(--accent); }}
.note {{ margin: 3px 0 0; font-size: 11.5px; color: var(--dim); }}
figure {{ margin: 14px 0 0; }}
figure img {{
  max-width: 100%; height: auto;
  border: 1px solid var(--line); border-radius: 6px;
  background: var(--panel);
}}
/* Only an enlargement gets nearest-neighbour. At natural size it makes no
   difference, and on the way down it is the thing that adds the aliasing. */
figure img.zoomed {{ image-rendering: pixelated; }}
figcaption {{ margin: 6px 0 0; font-size: 11px; color: var(--dim); }}
pre {{
  margin: 14px 0 0; padding: 14px 16px; overflow-x: auto;
  background: var(--panel); border: 1px solid var(--line); border-radius: 10px;
  font: 12px/1.5 ui-monospace, SFMono-Regular, Menlo, monospace;
  white-space: pre-wrap; word-break: break-word;
}}
footer {{ margin-top: 16px; border-top: 1px solid var(--line); padding-top: 10px; }}
.chunks {{ list-style: none; display: flex; flex-wrap: wrap; gap: 14px; margin: 0; padding: 0; }}
.chunks li {{ font-size: 11px; color: var(--dim); }}
.chunks b {{ font: 600 11px ui-monospace, SFMono-Regular, Menlo, monospace; color: var(--ink); }}
.prov {{ margin: 8px 0 0; font-size: 11px; color: var(--dim); }}
"
    )
}

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Standard base64, for the one data URI a preview needs. A dependency for
/// twenty lines of table lookup would be a poor trade.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let b = [
            group[0],
            *group.get(1).unwrap_or(&0),
            *group.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        for i in 0..4 {
            if i <= group.len() {
                out.push(ALPHABET[(n >> (18 - i * 6)) as usize & 0x3F] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc_examples() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn markup_in_a_file_cannot_escape_into_the_page() {
        assert_eq!(
            esc("<script>alert('x')</script>"),
            "&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;"
        );
    }
}
