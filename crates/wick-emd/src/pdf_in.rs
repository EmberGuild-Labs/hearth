//! Best-effort text extraction from a PDF.
//!
//! This is the one importer in the ecosystem that cannot promise fidelity,
//! and saying so plainly is better than pretending otherwise. A PDF is a
//! description of marks on a page, not a document: there is no reliable
//! notion of a paragraph in it, headings are "text that happens to be larger",
//! and reading order is whatever order the generator emitted drawing
//! operators in. Two-column layouts, tables and figure captions come out
//! interleaved. Anything scanned comes out empty, because there is no text in
//! it to find.
//!
//! What this does: walk the content streams, inflating those with
//! `/FlateDecode`, and collect the strings passed to the text-showing
//! operators (`Tj`, `TJ`, `'`, `"`), using the text-positioning operators to
//! decide where a line ends. Blocks are then split on blank lines, and a
//! short line in isolation is guessed to be a heading.
//!
//! [`extract`] returns the text and a list of caveats, which the importer
//! records in the file's provenance so the guesswork is visible to whoever
//! reads the result later rather than being quietly forgotten.

use flate2::read::ZlibDecoder;
use std::io::Read;

pub struct Extracted {
    pub text: String,
    /// What this extraction had to guess at or could not do.
    pub caveats: Vec<String>,
}

pub fn extract(bytes: &[u8]) -> Extracted {
    let mut caveats = Vec::new();
    if !bytes.starts_with(b"%PDF") {
        caveats.push("file does not start with %PDF".into());
    }

    let streams = find_streams(bytes, &mut caveats);
    if streams.is_empty() {
        caveats.push(
            "no readable content streams: the PDF may be encrypted, may use an object stream \
             layout this extractor does not parse, or may be a scan with no text in it"
                .into(),
        );
    }

    let mut text = String::new();
    for s in &streams {
        text.push_str(&read_stream_text(s));
    }
    if !text.trim().is_empty() {
        caveats.push(
            "paragraph and heading structure was inferred from line breaks and length; \
             a PDF does not record it"
                .into(),
        );
    }
    Extracted {
        text: tidy(&text),
        caveats,
    }
}

/// Every `stream ... endstream` body, inflated where it is Flate-compressed.
///
/// Scanning for the keyword rather than parsing the object graph is a
/// deliberate simplification: it handles the great majority of real PDFs, is
/// a few dozen lines instead of a few thousand, and fails visibly rather than
/// subtly when it does not work.
fn find_streams(bytes: &[u8], caveats: &mut Vec<String>) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut compressed_failures = 0;

    while let Some(rel) = find(&bytes[i..], b"stream") {
        let dict_start = bytes[..i + rel].rfind_seq(b"<<").unwrap_or(0);
        let dict = String::from_utf8_lossy(&bytes[dict_start..i + rel]).to_string();

        // The body starts after the EOL that must follow the keyword.
        let mut start = i + rel + 6;
        if bytes.get(start) == Some(&b'\r') {
            start += 1;
        }
        if bytes.get(start) == Some(&b'\n') {
            start += 1;
        }
        let Some(end_rel) = find(&bytes[start..], b"endstream") else {
            caveats.push("a stream was never closed; the file may be truncated".into());
            break;
        };
        let body = &bytes[start..start + end_rel];
        i = start + end_rel + 9;

        if dict.contains("/FlateDecode") {
            let mut inflated = Vec::new();
            match ZlibDecoder::new(body).read_to_end(&mut inflated) {
                Ok(_) => out.push(inflated),
                Err(_) => compressed_failures += 1,
            }
        } else if !dict.contains("/DCTDecode") && !dict.contains("/Image") {
            out.push(body.to_vec());
        }
    }
    if compressed_failures > 0 {
        caveats.push(format!(
            "{compressed_failures} compressed stream(s) could not be inflated and were skipped"
        ));
    }
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

trait RFind {
    fn rfind_seq(&self, needle: &[u8]) -> Option<usize>;
}

impl RFind for [u8] {
    fn rfind_seq(&self, needle: &[u8]) -> Option<usize> {
        self.windows(needle.len()).rposition(|w| w == needle)
    }
}

/// Pull the shown strings out of one content stream.
fn read_stream_text(stream: &[u8]) -> String {
    let s = String::from_utf8_lossy(stream);
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0usize;
    // Text collected since the last positioning operator, which is what a
    // line amounts to in a content stream.
    let mut line = String::new();

    let flush = |line: &mut String, out: &mut String| {
        if !line.trim().is_empty() {
            out.push_str(line.trim_end());
            out.push('\n');
        }
        line.clear();
    };

    while i < chars.len() {
        match chars[i] {
            '(' => {
                let (text, next) = read_literal(&chars, i);
                line.push_str(&text);
                i = next;
            }
            '<' if chars.get(i + 1) != Some(&'<') => {
                let (text, next) = read_hex(&chars, i);
                line.push_str(&text);
                i = next;
            }
            // Td, TD, T* and ' all move to a new line.
            'T' if matches!(chars.get(i + 1), Some('d') | Some('D') | Some('*')) => {
                flush(&mut line, &mut out);
                i += 2;
            }
            '\'' | '"' => {
                flush(&mut line, &mut out);
                i += 1;
            }
            'E' if chars.get(i + 1) == Some(&'T') => {
                flush(&mut line, &mut out);
                i += 2;
            }
            _ => i += 1,
        }
    }
    flush(&mut line, &mut out);
    out
}

/// A `( ... )` literal string, honouring backslash escapes and nesting.
fn read_literal(chars: &[char], start: usize) -> (String, usize) {
    let mut out = String::new();
    let mut depth = 1;
    let mut i = start + 1;
    while i < chars.len() {
        match chars[i] {
            '\\' => {
                i += 1;
                match chars.get(i) {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => {}
                    Some(d) if d.is_ascii_digit() => {
                        // Up to three octal digits.
                        let mut code = 0u32;
                        let mut n = 0;
                        while n < 3 {
                            match chars.get(i).and_then(|c| c.to_digit(8)) {
                                Some(d) => {
                                    code = code * 8 + d;
                                    i += 1;
                                    n += 1;
                                }
                                None => break,
                            }
                        }
                        i -= 1;
                        if let Some(c) = char::from_u32(code) {
                            out.push(c);
                        }
                    }
                    Some(c) => out.push(*c),
                    None => break,
                }
                i += 1;
            }
            '(' => {
                depth += 1;
                out.push('(');
                i += 1;
            }
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return (out, i + 1);
                }
                out.push(')');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    (out, chars.len())
}

/// A `<48656C6C6F>` hex string.
fn read_hex(chars: &[char], start: usize) -> (String, usize) {
    let mut digits = String::new();
    let mut i = start + 1;
    while i < chars.len() && chars[i] != '>' {
        if chars[i].is_ascii_hexdigit() {
            digits.push(chars[i]);
        }
        i += 1;
    }
    if digits.len() % 2 == 1 {
        digits.push('0');
    }
    let bytes: Vec<u8> = digits
        .as_bytes()
        .chunks(2)
        .filter_map(|p| u8::from_str_radix(std::str::from_utf8(p).ok()?, 16).ok())
        .collect();
    // Two-byte encodings are common in PDFs that embed subset fonts; the
    // high byte being zero throughout is the usual giveaway.
    let text = if bytes.len() >= 4 && bytes.iter().step_by(2).all(|b| *b == 0) {
        bytes
            .chunks(2)
            .filter_map(|p| char::from_u32(p[1] as u32))
            .collect()
    } else {
        bytes.iter().map(|b| *b as char).collect()
    };
    (text, i + 1)
}

/// Collapse the runs of blank lines that fall out of per-operator flushing,
/// and normalise whitespace.
fn tidy(s: &str) -> String {
    let mut out = String::new();
    let mut blanks = 0;
    for line in s.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            blanks += 1;
            continue;
        }
        if blanks > 0 && !out.is_empty() {
            out.push_str("\n\n");
        } else if !out.is_empty() {
            out.push('\n');
        }
        blanks = 0;
        out.push_str(line);
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An uncompressed PDF with two lines of text, as a generator would emit.
    const SIMPLE: &[u8] = b"%PDF-1.4
1 0 obj
<< /Length 90 >>
stream
BT /F1 12 Tf 50 700 Td (Hello \\(world\\)) Tj 0 -20 Td (Second line) Tj ET
endstream
endobj
%%EOF
";

    #[test]
    fn literal_strings_are_extracted_and_unescaped() {
        let e = extract(SIMPLE);
        assert!(e.text.contains("Hello (world)"), "{:?}", e.text);
        assert!(e.text.contains("Second line"));
    }

    #[test]
    fn positioning_operators_separate_lines() {
        let e = extract(SIMPLE);
        let lines: Vec<&str> = e.text.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 2, "{:?}", e.text);
    }

    #[test]
    fn hex_strings_are_decoded() {
        let pdf = b"%PDF-1.4\n1 0 obj\n<< /Length 40 >>\nstream\nBT <48656C6C6F> Tj ET\nendstream\nendobj\n";
        assert!(extract(pdf).text.contains("Hello"));
    }

    #[test]
    fn flate_compressed_streams_are_inflated() {
        use flate2::write::ZlibEncoder;
        use std::io::Write;
        let content = b"BT /F1 12 Tf (Compressed text) Tj ET";
        let mut enc = ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(content).unwrap();
        let deflated = enc.finish().unwrap();

        let mut pdf =
            b"%PDF-1.4\n1 0 obj\n<< /Length 99 /Filter /FlateDecode >>\nstream\n".to_vec();
        pdf.extend_from_slice(&deflated);
        pdf.extend_from_slice(b"\nendstream\nendobj\n%%EOF\n");

        assert!(extract(&pdf).text.contains("Compressed text"));
    }

    #[test]
    fn a_scan_with_no_text_says_so_rather_than_returning_nothing_quietly() {
        let pdf = b"%PDF-1.4\n1 0 obj\n<< /Subtype /Image /Filter /DCTDecode /Length 4 >>\nstream\n\xFF\xD8\xFF\xD9\nendstream\nendobj\n";
        let e = extract(pdf);
        assert!(e.text.trim().is_empty());
        assert!(
            e.caveats.iter().any(|c| c.contains("scan with no text")),
            "{:?}",
            e.caveats
        );
    }

    #[test]
    fn structure_guessing_is_declared_as_a_caveat() {
        let e = extract(SIMPLE);
        assert!(
            e.caveats.iter().any(|c| c.contains("inferred")),
            "{:?}",
            e.caveats
        );
    }

    #[test]
    fn a_non_pdf_is_flagged() {
        let e = extract(b"just some text");
        assert!(e.caveats.iter().any(|c| c.contains("%PDF")));
    }
}
