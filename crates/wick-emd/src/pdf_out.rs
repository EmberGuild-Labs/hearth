//! A small PDF writer.
//!
//! `.emd` stores a document as reflowable blocks, so producing a PDF means
//! laying it out — choosing where each line of text lands on each page. That
//! layout is done here, once, and can be *recorded back into the file* as a
//! `PINL` chunk. A pinned document then renders identically forever, because
//! the positions are data rather than the output of whatever layout engine
//! happens to be reading it. That is the "no per-viewer rendering drift"
//! claim, made concrete: reflow when you want it, pin when you need it.
//!
//! The output uses the base-14 fonts every PDF viewer is required to have, so
//! nothing is embedded and the files stay small. Line breaking needs the
//! font's character widths, so the Helvetica and Helvetica-Bold tables are
//! below; Courier is metrically fixed at 600 units.

use crate::{Block, BlockKind, Pin};

/// PDF user-space units are 1/72 inch, so these are points.
pub struct PageSetup {
    pub width: f64,
    pub height: f64,
    pub margin: f64,
    pub body_size: f64,
    pub leading: f64,
}

impl Default for PageSetup {
    fn default() -> Self {
        // A4. Chosen over US Letter because it is the majority of the world
        // and because the difference only matters for pinned documents,
        // which record their own page size anyway.
        PageSetup {
            width: 595.28,
            height: 841.89,
            margin: 56.0,
            body_size: 11.0,
            leading: 1.45,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Font {
    Regular,
    Bold,
    Mono,
}

impl Font {
    pub fn resource(self) -> &'static str {
        match self {
            Font::Regular => "F1",
            Font::Bold => "F2",
            Font::Mono => "F3",
        }
    }

    pub fn base_name(self) -> &'static str {
        match self {
            Font::Regular => "Helvetica",
            Font::Bold => "Helvetica-Bold",
            Font::Mono => "Courier",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Font::Regular => "regular",
            Font::Bold => "bold",
            Font::Mono => "mono",
        }
    }

    pub fn parse(s: &str) -> Font {
        match s {
            "bold" => Font::Bold,
            "mono" => Font::Mono,
            _ => Font::Regular,
        }
    }

    /// Width of one character in 1/1000 em.
    fn char_width(self, c: char) -> f64 {
        if self == Font::Mono {
            return 600.0;
        }
        let table = match self {
            Font::Bold => &HELVETICA_BOLD,
            _ => &HELVETICA,
        };
        match c {
            '\u{20}'..='\u{7E}' => table[(c as usize) - 32] as f64,
            // The handful of non-ASCII characters the layout itself emits.
            // Getting the em dash right matters: it is what a horizontal
            // rule is made of, and measuring it as an average letter would
            // run the rule past the margin.
            '\u{2014}' => 1000.0,
            '\u{2013}' => 556.0,
            '\u{2022}' => 350.0,
            '\u{2018}' | '\u{2019}' => {
                if self == Font::Bold {
                    238.0
                } else {
                    222.0
                }
            }
            '\u{201C}' | '\u{201D}' => {
                if self == Font::Bold {
                    500.0
                } else {
                    333.0
                }
            }
            // Anything else is measured as an average lowercase letter,
            // which keeps a line of accented or non-Latin text close to the
            // right length rather than wildly off. It is not exact.
            _ => 556.0,
        }
    }

    pub fn text_width(self, s: &str, size: f64) -> f64 {
        s.chars().map(|c| self.char_width(c)).sum::<f64>() * size / 1000.0
    }
}

/// Adobe's published Helvetica widths for ASCII 32–126, in 1/1000 em.
#[rustfmt::skip]
const HELVETICA: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278,
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 278, 278, 584, 584, 584, 556,
    1015, 667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833, 722, 778,
    667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 278, 278, 278, 469, 556,
    333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556,
    556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584,
];

#[rustfmt::skip]
const HELVETICA_BOLD: [u16; 95] = [
    278, 333, 474, 556, 556, 889, 722, 238, 333, 333, 389, 584, 278, 333, 278, 278,
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 333, 333, 584, 584, 584, 611,
    975, 722, 722, 722, 722, 667, 611, 778, 722, 278, 556, 722, 611, 833, 722, 778,
    667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 333, 278, 333, 584, 556,
    333, 556, 611, 556, 611, 556, 333, 611, 611, 278, 278, 556, 278, 889, 611, 611,
    611, 611, 389, 556, 333, 611, 556, 778, 556, 556, 500, 389, 280, 389, 584,
];

/// Style a block is drawn in.
fn style(b: &Block, setup: &PageSetup) -> (Font, f64, f64, f64) {
    // (font, size, indent, space before)
    match b.kind {
        BlockKind::Heading => {
            let scale = match b.level {
                0 | 1 => 1.85,
                2 => 1.45,
                3 => 1.2,
                _ => 1.05,
            };
            (
                Font::Bold,
                setup.body_size * scale,
                0.0,
                setup.body_size * 1.2,
            )
        }
        BlockKind::Code => (
            Font::Mono,
            setup.body_size * 0.88,
            14.0,
            setup.body_size * 0.7,
        ),
        // A quotation is set in from both margins. There is no italic in
        // the base-14 set worth relying on, and a drawn bar would need a
        // second graphics path; indentation says the same thing and pins
        // cleanly.
        BlockKind::Quote => (
            Font::Regular,
            setup.body_size * 0.97,
            26.0,
            setup.body_size * 0.8,
        ),
        BlockKind::List => (
            Font::Regular,
            setup.body_size,
            12.0 + 12.0 * b.level as f64,
            setup.body_size * 0.25,
        ),
        _ => (Font::Regular, setup.body_size, 0.0, setup.body_size * 0.7),
    }
}

/// Break text into lines that fit `width`, at a word boundary where one
/// exists. Code is never re-wrapped — its line breaks are meaningful.
fn wrap(text: &str, font: Font, size: f64, width: f64, hard: bool) -> Vec<String> {
    if hard {
        return text.lines().map(|l| l.to_string()).collect();
    }
    let mut out = Vec::new();
    for para in text.lines() {
        let mut line = String::new();
        for word in para.split_whitespace() {
            let candidate = if line.is_empty() {
                word.to_string()
            } else {
                format!("{line} {word}")
            };
            if font.text_width(&candidate, size) <= width || line.is_empty() {
                line = candidate;
            } else {
                out.push(std::mem::take(&mut line));
                line = word.to_string();
            }
        }
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// One positioned line of text. This is what a `PINL` chunk stores.
#[derive(Clone, Debug)]
pub struct Placed {
    pub block: usize,
    pub page: usize,
    pub x: f64,
    /// Baseline, measured from the bottom of the page as PDF does.
    pub y: f64,
    pub font: Font,
    pub size: f64,
    pub text: String,
}

/// Lay a document out, producing one entry per line of text.
pub fn layout(blocks: &[Block], setup: &PageSetup) -> Vec<Placed> {
    let usable = setup.width - setup.margin * 2.0;
    let mut out = Vec::new();
    let mut page = 0usize;
    let mut y = setup.height - setup.margin;

    for (i, b) in blocks.iter().enumerate() {
        if b.kind == BlockKind::PageBreak {
            page += 1;
            y = setup.height - setup.margin;
            continue;
        }
        let (font, size, indent, space_before) = style(b, setup);
        let line_height = size * setup.leading;

        if b.kind == BlockKind::Rule {
            y -= space_before + line_height;
            out.push(Placed {
                block: i,
                page,
                x: setup.margin,
                y,
                font,
                size,
                // Drawn as a run of em dashes rather than a graphics
                // operator: one text-drawing path is easier to pin, and a
                // rule is not worth a second one. The count comes from the
                // dash's real width so the rule stops at the margin.
                text: "\u{2014}"
                    .repeat((usable / font.text_width("\u{2014}", size)).floor() as usize),
            });
            continue;
        }

        let bullet = matches!(b.kind, BlockKind::List);
        let width = usable - indent - if bullet { 12.0 } else { 0.0 };
        let lines = wrap(&b.text, font, size, width, b.kind == BlockKind::Code);

        y -= space_before;
        for (n, line) in lines.iter().enumerate() {
            if y - line_height < setup.margin {
                page += 1;
                y = setup.height - setup.margin;
            }
            y -= line_height;
            let text = if bullet && n == 0 {
                format!("\u{2022}  {line}")
            } else if bullet {
                format!("   {line}")
            } else {
                line.clone()
            };
            out.push(Placed {
                block: i,
                page,
                x: setup.margin + indent,
                y,
                font,
                size,
                text,
            });
        }
    }
    out
}

/// Escape for a PDF literal string, and drop what WinAnsi cannot hold.
fn pdf_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '(' => out.push_str("\\("),
            ')' => out.push_str("\\)"),
            '\\' => out.push_str("\\\\"),
            // WinAnsiEncoding covers Latin-1 plus a handful of typographic
            // characters. The few used by the layout are mapped explicitly;
            // anything else outside it becomes '?' rather than a mangled
            // byte sequence.
            '\u{2022}' => out.push_str("\\225"),
            '\u{2014}' => out.push_str("\\227"),
            '\u{2018}' => out.push_str("\\221"),
            '\u{2019}' => out.push_str("\\222"),
            '\u{201C}' => out.push_str("\\223"),
            '\u{201D}' => out.push_str("\\224"),
            c if (c as u32) < 32 => out.push(' '),
            c if (c as u32) < 127 => out.push(c),
            c if (c as u32) <= 255 => out.push_str(&format!("\\{:03o}", c as u32)),
            _ => out.push('?'),
        }
    }
    out
}

/// Write a PDF from already-placed lines.
pub fn write(placed: &[Placed], setup: &PageSetup, title: &str) -> Vec<u8> {
    let page_count = placed
        .iter()
        .map(|p| p.page)
        .max()
        .map(|m| m + 1)
        .unwrap_or(1);

    // Build each page's content stream.
    let mut streams: Vec<String> = vec![String::new(); page_count];
    for p in placed {
        let s = &mut streams[p.page];
        s.push_str(&format!(
            "BT /{} {:.2} Tf 1 0 0 1 {:.2} {:.2} Tm ({}) Tj ET\n",
            p.font.resource(),
            p.size,
            p.x,
            p.y,
            pdf_string(&p.text)
        ));
    }

    // Object numbering: 1 catalog, 2 pages, 3-5 fonts, 6 info, then a page
    // and a content stream for each page.
    let first_page_obj = 7usize;
    let mut objects: Vec<String> = Vec::new();

    objects.push("<< /Type /Catalog /Pages 2 0 R >>".into());

    let kids: Vec<String> = (0..page_count)
        .map(|i| format!("{} 0 R", first_page_obj + i * 2))
        .collect();
    objects.push(format!(
        "<< /Type /Pages /Count {page_count} /Kids [{}] >>",
        kids.join(" ")
    ));

    for f in [Font::Regular, Font::Bold, Font::Mono] {
        objects.push(format!(
            "<< /Type /Font /Subtype /Type1 /BaseFont /{} /Encoding /WinAnsiEncoding >>",
            f.base_name()
        ));
    }
    objects.push(format!(
        "<< /Title ({}) /Producer (Hearth {}) >>",
        pdf_string(title),
        libwick::VERSION
    ));

    for (i, stream) in streams.iter().enumerate() {
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {:.2} {:.2}] \
             /Resources << /Font << /F1 3 0 R /F2 4 0 R /F3 5 0 R >> >> /Contents {} 0 R >>",
            setup.width,
            setup.height,
            first_page_obj + i * 2 + 1
        ));
        objects.push(format!(
            "<< /Length {} >>\nstream\n{stream}endstream",
            stream.len()
        ));
    }

    // Serialise, recording each object's byte offset for the xref table.
    let mut out: Vec<u8> = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }

    let xref_at = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R /Info 6 0 R >>\nstartxref\n{xref_at}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

/// Turn a layout into storable pins.
pub fn to_pins(placed: &[Placed], setup: &PageSetup) -> Vec<Pin> {
    placed
        .iter()
        .map(|p| Pin {
            block: p.block,
            page: p.page,
            x: p.x,
            y: p.y,
            font: p.font.name().to_string(),
            size: p.size,
            text: p.text.clone(),
        })
        .map(|mut pin| {
            // Round to a tenth of a point. Storing full float precision would
            // make two layouts of the same document differ in the last bit
            // and show up as a diff.
            pin.x = (pin.x * 10.0).round() / 10.0;
            pin.y = (pin.y * 10.0).round() / 10.0;
            let _ = setup;
            pin
        })
        .collect()
}

pub fn from_pins(pins: &[Pin]) -> Vec<Placed> {
    pins.iter()
        .map(|p| Placed {
            block: p.block,
            page: p.page,
            x: p.x,
            y: p.y,
            font: Font::parse(&p.font),
            size: p.size,
            text: p.text.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> Vec<Block> {
        vec![
            Block::new(BlockKind::Heading, "A Title").with_level(1),
            Block::new(BlockKind::Paragraph, &"word ".repeat(2000)),
            Block::new(BlockKind::Code, "fn main() {\n    println!(\"hi\");\n}"),
        ]
    }

    #[test]
    fn a_horizontal_rule_stops_at_the_margin() {
        let setup = PageSetup::default();
        let usable = setup.width - setup.margin * 2.0;
        let placed = layout(&[Block::new(BlockKind::Rule, "")], &setup);
        let drawn = &placed[0];
        let w = drawn.font.text_width(&drawn.text, drawn.size);
        assert!(
            w <= usable,
            "rule is {w:.1}pt wide, measure is {usable:.1}pt"
        );
        assert!(
            w > usable * 0.9,
            "rule only spans {w:.1}pt of {usable:.1}pt"
        );
    }

    #[test]
    fn widths_are_font_dependent_and_monotonic() {
        assert!(Font::Bold.text_width("mmm", 12.0) > Font::Regular.text_width("mmm", 12.0));
        assert!(Font::Regular.text_width("iiii", 12.0) < Font::Regular.text_width("mmmm", 12.0));
        // Courier is fixed-pitch by definition.
        assert_eq!(
            Font::Mono.text_width("iiii", 10.0),
            Font::Mono.text_width("mmmm", 10.0)
        );
    }

    #[test]
    fn wrapped_lines_fit_the_measure() {
        let setup = PageSetup::default();
        let width = setup.width - setup.margin * 2.0;
        for line in wrap(
            &"lorem ipsum dolor ".repeat(100),
            Font::Regular,
            11.0,
            width,
            false,
        ) {
            assert!(
                Font::Regular.text_width(&line, 11.0) <= width,
                "line overflows: {line:?}"
            );
        }
    }

    #[test]
    fn code_keeps_its_own_line_breaks() {
        let lines = wrap("a\nb\nc", Font::Mono, 10.0, 500.0, true);
        assert_eq!(lines, vec!["a", "b", "c"]);
    }

    #[test]
    fn long_documents_paginate() {
        let placed = layout(&doc(), &PageSetup::default());
        let pages = placed.iter().map(|p| p.page).max().unwrap();
        assert!(pages >= 1, "2000 words of body text should exceed one page");
        // Nothing is drawn into the bottom margin.
        let setup = PageSetup::default();
        assert!(placed.iter().all(|p| p.y >= setup.margin - 0.001));
    }

    #[test]
    fn the_pdf_is_structurally_well_formed() {
        let setup = PageSetup::default();
        let bytes = write(&layout(&doc(), &setup), &setup, "A Title");
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.starts_with("%PDF-1.4"));
        assert!(s.trim_end().ends_with("%%EOF"));
        assert!(s.contains("/Type /Catalog"));
        assert!(s.contains("/BaseFont /Helvetica-Bold"));

        // Every xref offset must point at its object header. Offsets are
        // byte offsets into the file, and the binary comment on line 2 is
        // not valid UTF-8, so this has to be checked against the bytes.
        let tail = s.rsplit("startxref").next().unwrap();
        let xref_at: usize = tail.trim().lines().next().unwrap().parse().unwrap();
        assert_eq!(&bytes[xref_at..xref_at + 4], b"xref");

        let table = String::from_utf8_lossy(&bytes[xref_at..]).to_string();
        // Skip "xref", the subsection header, and the free entry for object 0.
        for (i, line) in table.lines().skip(3).enumerate() {
            let Some(off) = line
                .split_whitespace()
                .next()
                .and_then(|o| o.parse::<usize>().ok())
            else {
                break;
            };
            let header = format!("{} 0 obj", i + 1);
            assert!(
                bytes[off..].starts_with(header.as_bytes()),
                "xref entry {i} points at byte {off}, which is not {header}"
            );
        }
    }

    #[test]
    fn parentheses_and_backslashes_are_escaped() {
        let setup = PageSetup::default();
        let blocks = vec![Block::new(BlockKind::Paragraph, "a (b) \\ c")];
        let s = String::from_utf8_lossy(&write(&layout(&blocks, &setup), &setup, "t")).to_string();
        assert!(s.contains("(a \\(b\\) \\\\ c) Tj"), "{s}");
    }

    #[test]
    fn pins_round_trip_through_storage() {
        let setup = PageSetup::default();
        let placed = layout(&doc(), &setup);
        let back = from_pins(&to_pins(&placed, &setup));
        assert_eq!(back.len(), placed.len());
        assert_eq!(back[0].text, placed[0].text);
        assert_eq!(back[0].font, placed[0].font);
        assert!((back[0].y - placed[0].y).abs() < 0.05);
    }
}
