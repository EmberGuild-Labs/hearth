//! `.emt` — plain text, sectioned.
//!
//! The simplest payload in the family, and deliberately the first one built:
//! it exercises the chunk table, the provenance chain and the migration
//! engine without also needing an image codec or a layout engine.
//!
//! ## What it adds over `.txt`
//!
//! **Enforced UTF-8.** A `.txt` file has no encoding. It has whatever
//! encoding the writer happened to use, and every reader guesses. Import
//! fails loudly on bytes that are not UTF-8 rather than producing mojibake
//! three tools downstream.
//!
//! **Semantic sectioning without Markdown's ambiguity.** Each block of text
//! is stored as its own chunk, tagged with what it is — heading, paragraph,
//! code, list item, quotation. Markdown expresses the same structure but only
//! as a *convention over characters*: whether `*text*` is emphasis or a
//! literal asterisk depends on which of a dozen implementations reads it, and
//! whether an indented line is code depends on what came before it. Here the
//! structure is decided once, at import, and stored. Every later reader gets
//! the same answer.
//!
//! **Section-level diffing.** Editing one paragraph of a long document
//! changes one chunk, so `hearth diff` says which paragraph.
//!
//! ## Exact round-trips
//!
//! A section stores its text *and the exact bytes that separated it from the
//! next one*. Concatenating every section reproduces the source file byte for
//! byte — trailing whitespace, mixed line endings, final newline or its
//! absence, all of it. Structure is an annotation layered over the original
//! bytes, never a replacement for them, so `txt -> emt -> txt` is an
//! identity and the format can be adopted without a leap of faith.

use libwick::chunks::{Chunk, ChunkList, ChunkType};
use libwick::error::{Error, Result};
use libwick::plugin::{Payload, Plugin, RenderOpts, Source, Starter};
use libwick::schema::{FieldRule, Issue, Schema};
use libwick::{ChangeKind, Tag, WickFile};

pub const TAG: Tag = Tag::new(b"MT");
pub const SCHEMA_VERSION: u32 = 1;

const SECT: ChunkType = ChunkType::new(b"SECT");
const META: ChunkType = ChunkType::new(b"META");
/// Sub-chunks of `SUMM`.
const OUTL: ChunkType = ChunkType::new(b"OUTL");
const STAT: ChunkType = ChunkType::new(b"STAT");

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Paragraph = 0,
    Heading = 1,
    Code = 2,
    List = 3,
    Quote = 4,
    /// A horizontal rule or other divider.
    Rule = 5,
}

impl Kind {
    fn from_u8(b: u8) -> Result<Kind> {
        Ok(match b {
            0 => Kind::Paragraph,
            1 => Kind::Heading,
            2 => Kind::Code,
            3 => Kind::List,
            4 => Kind::Quote,
            5 => Kind::Rule,
            other => return Err(Error::Other(format!("unknown .emt section kind {other}"))),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Paragraph => "paragraph",
            Kind::Heading => "heading",
            Kind::Code => "code",
            Kind::List => "list",
            Kind::Quote => "quote",
            Kind::Rule => "rule",
        }
    }
}

/// One block of text plus what it is and what followed it.
#[derive(Clone, Debug, PartialEq)]
pub struct Section {
    pub kind: Kind,
    /// Heading depth, or list nesting. Zero for everything else.
    pub level: u8,
    /// Language of a fenced code block, if it declared one.
    pub lang: String,
    /// The block's own bytes, exactly as they appeared.
    pub text: String,
    /// The bytes between this block and the next, exactly as they appeared.
    /// Storing this is what makes the round-trip an identity.
    pub sep: String,
}

impl Section {
    pub fn new(kind: Kind, text: &str, sep: &str) -> Self {
        Section {
            kind,
            level: 0,
            lang: String::new(),
            text: text.to_string(),
            sep: sep.to_string(),
        }
    }

    /// `[kind u8][level u8][lang len u16][sep len u16][lang][sep][text]`
    fn encode(&self) -> Chunk {
        let mut v = Vec::with_capacity(self.text.len() + self.sep.len() + self.lang.len() + 6);
        v.push(self.kind as u8);
        v.push(self.level);
        v.extend_from_slice(&(self.lang.len() as u16).to_le_bytes());
        v.extend_from_slice(&(self.sep.len() as u16).to_le_bytes());
        v.extend_from_slice(self.lang.as_bytes());
        v.extend_from_slice(self.sep.as_bytes());
        v.extend_from_slice(self.text.as_bytes());
        Chunk::new(SECT, v)
    }

    fn decode(c: &Chunk) -> Result<Section> {
        let v = &c.value;
        if v.len() < 6 {
            return Err(Error::Truncated("SECT header"));
        }
        let lang_len = u16::from_le_bytes([v[2], v[3]]) as usize;
        let sep_len = u16::from_le_bytes([v[4], v[5]]) as usize;
        let lang_end = 6 + lang_len;
        let sep_end = lang_end + sep_len;
        if sep_end > v.len() {
            return Err(Error::Truncated("SECT body"));
        }
        let utf8 = |b: &[u8]| -> Result<String> {
            String::from_utf8(b.to_vec())
                .map_err(|_| Error::Other("a .emt section is not valid UTF-8".into()))
        };
        Ok(Section {
            kind: Kind::from_u8(v[0])?,
            level: v[1],
            lang: utf8(&v[6..lang_end])?,
            sep: utf8(&v[lang_end..sep_end])?,
            text: utf8(&v[sep_end..])?,
        })
    }

    pub fn words(&self) -> usize {
        self.text.split_whitespace().count()
    }

    /// First line, trimmed and elided — what a diff or an outline shows.
    pub fn preview(&self, width: usize) -> String {
        let line = self.text.lines().next().unwrap_or("").trim();
        if line.chars().count() > width {
            let head: String = line.chars().take(width - 1).collect();
            format!("{head}…")
        } else {
            line.to_string()
        }
    }
}

/// Read every section out of a file's `DATA`.
pub fn sections(file: &WickFile) -> Result<Vec<Section>> {
    file.data()?.all(SECT).map(Section::decode).collect()
}

/// Reassemble the original bytes.
pub fn to_text(sections: &[Section]) -> String {
    let mut s = String::new();
    for sec in sections {
        s.push_str(&sec.text);
        s.push_str(&sec.sep);
    }
    s
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Split a source file into sections.
///
/// `structured` turns on Markdown-shaped recognition — headings, fences,
/// quotes, lists. With it off (a `.txt` or a `.log`), every blank-line-
/// separated block is a paragraph, which is the only structure plain text
/// actually asserts.
pub fn parse(src: &str, structured: bool) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    // Keep the line terminators so the separator bytes survive verbatim.
    let lines = split_keeping_endings(src);
    let mut i = 0;

    while i < lines.len() {
        // Anything blank belongs to the preceding section's separator.
        if lines[i].trim().is_empty() {
            let blank = lines[i];
            match out.last_mut() {
                Some(prev) => prev.sep.push_str(blank),
                // Leading blank lines have nothing to attach to, so they
                // become a paragraph with no text of their own.
                None => out.push(Section::new(Kind::Paragraph, "", blank)),
            }
            i += 1;
            continue;
        }

        let (section, consumed) = if structured {
            parse_block(&lines, i)
        } else {
            parse_plain_block(&lines, i)
        };
        out.push(section);
        i += consumed;
    }
    out
}

/// Lines with their terminators attached, so nothing is lost.
fn split_keeping_endings(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, c) in s.char_indices() {
        if c == '\n' {
            out.push(&s[start..=i]);
            start = i + 1;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// Split a run of lines into the section text and its trailing whitespace.
fn split_trailing(block: &str) -> (String, String) {
    let end = block.trim_end_matches(['\n', '\r']).len();
    (block[..end].to_string(), block[end..].to_string())
}

fn parse_plain_block(lines: &[&str], start: usize) -> (Section, usize) {
    let mut n = 0;
    while start + n < lines.len() && !lines[start + n].trim().is_empty() {
        n += 1;
    }
    let block: String = lines[start..start + n].concat();
    let (text, sep) = split_trailing(&block);
    (Section::new(Kind::Paragraph, &text, &sep), n)
}

fn parse_block(lines: &[&str], start: usize) -> (Section, usize) {
    let first = lines[start];
    let t = first.trim_start();

    // Fenced code: everything to the closing fence is opaque, which is the
    // whole point of a fence and the thing Markdown parsers most often
    // disagree about.
    if let Some(rest) = t.strip_prefix("```") {
        let lang = rest.trim().to_string();
        let mut n = 1;
        while start + n < lines.len() {
            let done = lines[start + n].trim_start().starts_with("```");
            n += 1;
            if done {
                break;
            }
        }
        let block: String = lines[start..start + n].concat();
        let (text, sep) = split_trailing(&block);
        let mut s = Section::new(Kind::Code, &text, &sep);
        s.lang = lang;
        return (s, n);
    }

    // ATX heading: always exactly one line.
    if t.starts_with('#') {
        let level = t.bytes().take_while(|c| *c == b'#').count();
        if level <= 6 && t.as_bytes().get(level) == Some(&b' ') {
            let (text, sep) = split_trailing(first);
            let mut s = Section::new(Kind::Heading, &text, &sep);
            s.level = level as u8;
            return (s, 1);
        }
    }

    // A rule is one line of three or more of the same marker.
    let bare = t.trim_end();
    if bare.len() >= 3
        && (bare.bytes().all(|c| c == b'-')
            || bare.bytes().all(|c| c == b'=')
            || bare.bytes().all(|c| c == b'*'))
    {
        let (text, sep) = split_trailing(first);
        return (Section::new(Kind::Rule, &text, &sep), 1);
    }

    let kind = if t.starts_with("> ") || t == ">" {
        Kind::Quote
    } else if is_list_marker(t) {
        Kind::List
    } else {
        Kind::Paragraph
    };

    // Consume the contiguous run of lines that keep the same kind. A list
    // item's continuation lines are indented, so they stay with it.
    let mut n = 1;
    while start + n < lines.len() {
        let next = lines[start + n];
        if next.trim().is_empty() {
            break;
        }
        let nt = next.trim_start();
        let still_same = match kind {
            Kind::Quote => nt.starts_with('>'),
            // A new marker starts a new item; anything else continues this one.
            Kind::List => !is_list_marker(nt),
            _ => {
                !nt.starts_with('#')
                    && !nt.starts_with("```")
                    && !is_list_marker(nt)
                    && !nt.starts_with('>')
            }
        };
        if !still_same {
            break;
        }
        n += 1;
    }

    let block: String = lines[start..start + n].concat();
    let (text, sep) = split_trailing(&block);
    let mut s = Section::new(kind, &text, &sep);
    if kind == Kind::List {
        // Indentation depth, two spaces per level, which is the only list
        // nesting convention every Markdown dialect agrees on.
        s.level = (first.len() - first.trim_start().len()).min(12) as u8 / 2;
    }
    (s, n)
}

fn is_list_marker(t: &str) -> bool {
    if let Some(rest) = t.strip_prefix(['-', '*', '+']) {
        return rest.starts_with(' ');
    }
    let digits = t.bytes().take_while(|c| c.is_ascii_digit()).count();
    digits > 0
        && matches!(t.as_bytes().get(digits), Some(b'.') | Some(b')'))
        && t.as_bytes().get(digits + 1) == Some(&b' ')
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

pub struct Emt;

impl Plugin for Emt {
    fn tag(&self) -> Tag {
        TAG
    }
    fn ext(&self) -> &'static str {
        "emt"
    }
    fn name(&self) -> &'static str {
        "text"
    }
    fn about(&self) -> &'static str {
        "plain text with enforced UTF-8 and unambiguous sectioning (replaces .txt)"
    }
    fn imports(&self) -> &'static [&'static str] {
        &["txt", "text", "log", "md", "markdown", "rst"]
    }
    fn exports(&self) -> &'static [&'static str] {
        &["txt", "md"]
    }
    fn schema_version(&self) -> u32 {
        SCHEMA_VERSION
    }

    fn import(&self, src: &Source) -> Result<Payload> {
        // The UTF-8 check is the format's first promise, so it happens
        // before anything else and reports where the file went wrong.
        let text = src.text()?;
        let structured = matches!(src.ext, "md" | "markdown" | "rst");
        let sections = parse(text, structured);

        let mut data = ChunkList::new();
        data.push(Chunk::json(
            META,
            &serde_json::json!({
                "source": src.name,
                "structured": structured,
                "sections": sections.len(),
            }),
        )?);
        for s in &sections {
            data.push(s.encode());
        }

        let mut schema = Schema::new("text");
        schema.version = SCHEMA_VERSION;
        schema.fields = vec![
            FieldRule::new("section.kind", "string").required(),
            FieldRule::new("section.text", "string").required(),
        ];

        Ok(Payload {
            summary: Some(summarize(&sections)),
            data,
            schema: Some(schema),
            caps: None,
            migrations: None,
        })
    }

    /// An empty `.emt`, or one holding a single heading. Markdown is the
    /// starter dialect because it is the one `.emt` imports *and* exports,
    /// so whatever `hearth create` writes here is exactly what `hearth edit`
    /// will hand back to the editor.
    fn starter(&self, spec: &Starter) -> Result<(&'static str, Vec<u8>)> {
        spec.only("emt", &["title"])?;
        let text = match spec.title {
            Some(t) => format!("# {t}\n\n"),
            None => String::new(),
        };
        Ok(("md", text.into_bytes()))
    }

    /// Markdown when the file was imported as Markdown, plain text
    /// otherwise. `.emt` round-trips both exactly, but only the dialect the
    /// sections were *recognised* in can express them again: hand a
    /// structured file back as `.txt` and every heading returns as a
    /// paragraph.
    fn edit_ext(&self, file: &WickFile) -> Result<&'static str> {
        let structured = match file.data()?.get(META) {
            Some(c) => c
                .as_json()?
                .get("structured")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            None => false,
        };
        Ok(if structured { "md" } else { "txt" })
    }

    fn export(&self, file: &WickFile, to: &str) -> Result<Vec<u8>> {
        match to {
            // Both targets reproduce the source exactly: the structure is an
            // annotation over the original bytes, not a re-rendering of them,
            // so there is nothing to reformat on the way out.
            "txt" | "md" => Ok(to_text(&sections(file)?).into_bytes()),
            other => Err(Error::Other(format!(".emt cannot export to .{other}"))),
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
        let sections = sections(file)?;
        let limit = opts.limit.unwrap_or(usize::MAX);
        for (i, s) in sections.iter().enumerate().take(limit) {
            let tag = match s.kind {
                Kind::Heading => format!("h{}", s.level),
                Kind::Code if !s.lang.is_empty() => format!("code:{}", s.lang),
                k => k.name().to_string(),
            };
            writeln!(
                out,
                "\x1b[2m{i:>4}  {tag:<12}\x1b[0m{}",
                s.text.replace('\n', "\n            ")
            )?;
        }
        if sections.len() > limit {
            writeln!(
                out,
                "\x1b[2m      … {} more sections\x1b[0m",
                sections.len() - limit
            )?;
        }
        Ok(())
    }

    fn validate(&self, file: &WickFile) -> Result<Vec<Issue>> {
        let mut issues = Vec::new();
        let sections = sections(file)?;
        if sections.is_empty() {
            issues.push(Issue::warning("", "file contains no sections"));
        }
        for (i, s) in sections.iter().enumerate() {
            if s.kind == Kind::Heading && s.text.lines().count() > 1 {
                issues.push(Issue::error(
                    format!("section[{i}]"),
                    "a heading spans more than one line",
                ));
            }
            if s.kind == Kind::Code && s.lang.contains(char::is_whitespace) {
                issues.push(Issue::warning(
                    format!("section[{i}]"),
                    format!("code fence language {:?} contains whitespace", s.lang),
                ));
            }
        }
        Ok(issues)
    }

    fn diff(
        &self,
        a: &WickFile,
        b: &WickFile,
        keys: &libwick::KeyRing,
    ) -> Result<Vec<libwick::Change>> {
        let (sa, sb) = (sections(a)?, sections(b)?);
        let structural = libwick::diff::structural(&a.chunks, &b.chunks, keys);

        // Re-describe each structural change in terms the reader recognises:
        // "section 4 (heading)" rather than "DATA[1]/SECT[5]".
        Ok(structural
            .into_iter()
            .map(|mut c| {
                if c.ty != SECT {
                    return c;
                }
                let idx = c
                    .path
                    .rsplit('[')
                    .next()
                    .and_then(|s| s.trim_end_matches(']').parse::<usize>().ok());
                // DATA holds META at index 0, so the section number is one less.
                let sec = idx.and_then(|i| i.checked_sub(1));
                let side = if c.kind == ChangeKind::Removed {
                    &sa
                } else {
                    &sb
                };
                if let Some(s) = sec.and_then(|i| side.get(i)) {
                    c.path = format!("section {}", sec.unwrap());
                    c.note = match c.kind {
                        ChangeKind::Modified => {
                            let old = sec.and_then(|i| sa.get(i));
                            match old {
                                Some(o) if o.words() != s.words() => format!(
                                    "{}: {} -> {} words  {}",
                                    s.kind.name(),
                                    o.words(),
                                    s.words(),
                                    s.preview(48)
                                ),
                                _ => format!("{}: {}", s.kind.name(), s.preview(56)),
                            }
                        }
                        _ => format!("{}: {}", s.kind.name(), s.preview(56)),
                    };
                }
                c
            })
            .collect())
    }

    fn summarize(&self, data: &ChunkList) -> Result<Option<ChunkList>> {
        let sections: Vec<Section> = data.all(SECT).map(Section::decode).collect::<Result<_>>()?;
        Ok(Some(summarize(&sections)))
    }
}

/// The cheap tier: an outline and a word count. Enough to answer "what is
/// this document" without decompressing the text.
fn summarize(sections: &[Section]) -> ChunkList {
    let mut summ = ChunkList::new();
    let words: usize = sections.iter().map(|s| s.words()).sum();
    let chars: usize = sections.iter().map(|s| s.text.chars().count()).sum();

    let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
    for s in sections {
        *counts.entry(s.kind.name()).or_default() += 1;
    }

    let stat = serde_json::json!({
        "sections": sections.len(),
        "words": words,
        "characters": chars,
        // 238 wpm is the median silent-reading rate for adults on prose
        // (Brysbaert 2019, a meta-analysis of 190 studies).
        "reading_minutes": (words as f64 / 238.0 * 10.0).round() / 10.0,
        "kinds": counts,
        "opening": sections
            .iter()
            .find(|s| s.kind == Kind::Paragraph && !s.text.trim().is_empty())
            .map(|s| s.preview(200))
            .unwrap_or_default(),
    });
    summ.push(Chunk::new(
        STAT,
        serde_json::to_vec(&stat).unwrap_or_default(),
    ));

    for s in sections.iter().filter(|s| s.kind == Kind::Heading) {
        let mut v = vec![s.level];
        v.extend_from_slice(s.text.trim_start_matches(['#', ' ']).trim().as_bytes());
        summ.push(Chunk::new(OUTL, v));
    }
    summ
}

fn render_summary(file: &WickFile, out: &mut dyn std::io::Write) -> Result<()> {
    let Some(summ) = file.summary()? else {
        return Err(Error::MissingChunk("SUMM"));
    };
    if let Some(stat) = summ.get(STAT) {
        let v: serde_json::Value = stat.as_json()?;
        writeln!(
            out,
            "{} sections, {} words, about {} min to read",
            v["sections"], v["words"], v["reading_minutes"]
        )?;
        let opening = v["opening"].as_str().unwrap_or("");
        if !opening.is_empty() {
            writeln!(out, "\n{opening}")?;
        }
    }
    let headings: Vec<_> = summ.all(OUTL).collect();
    if !headings.is_empty() {
        writeln!(out, "\noutline:")?;
        for h in headings {
            let level = *h.value.first().unwrap_or(&1) as usize;
            let text = std::str::from_utf8(&h.value[1..]).unwrap_or("");
            writeln!(out, "{}{}", "  ".repeat(level.max(1)), text)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROSE: &str =
        "Title line\n\nA first paragraph that runs\nacross two lines.\n\nA second one.\n";

    const MD: &str = "\
# Heading

Some prose with *asterisks* that stay literal.

## Sub-heading

- first item
- second item
  continued here

> a quotation
> across two lines

```rust
fn main() {}
```

Closing paragraph.
";

    fn build(src: &str, ext: &str) -> WickFile {
        let mut f = WickFile::new(TAG);
        let payload = Emt
            .import(&Source::new(src.as_bytes(), "sample", ext))
            .unwrap();
        f.set_data(&payload.data).unwrap();
        if let Some(s) = payload.summary {
            f.set_summary(&s).unwrap();
        }
        if let Some(s) = payload.schema {
            f.set_schema(&s).unwrap();
        }
        f
    }

    #[test]
    fn plain_text_round_trips_byte_for_byte() {
        for src in [
            PROSE,
            "no trailing newline",
            "\n\n\nleading blanks\n",
            "windows\r\nline\r\nendings\r\n",
            "trailing spaces   \n\nand a tab\there\n",
            "",
        ] {
            let f = build(src, "txt");
            let out = Emt.export(&f, "txt").unwrap();
            assert_eq!(
                String::from_utf8(out).unwrap(),
                src,
                "round trip changed {src:?}"
            );
        }
    }

    #[test]
    fn markdown_round_trips_byte_for_byte_too() {
        let f = build(MD, "md");
        assert_eq!(
            String::from_utf8(Emt.export(&f, "md").unwrap()).unwrap(),
            MD
        );
    }

    #[test]
    fn markdown_structure_is_recognised() {
        let secs = parse(MD, true);
        let kinds: Vec<_> = secs.iter().map(|s| s.kind).collect();
        assert_eq!(kinds[0], Kind::Heading);
        assert_eq!(secs[0].level, 1);
        assert_eq!(kinds[1], Kind::Paragraph);
        assert_eq!(secs[2].level, 2);
        assert!(kinds.contains(&Kind::List));
        assert!(kinds.contains(&Kind::Quote));
        let code = secs.iter().find(|s| s.kind == Kind::Code).unwrap();
        assert_eq!(code.lang, "rust");
        assert!(code.text.contains("fn main"));
    }

    #[test]
    fn plain_text_asserts_no_structure_it_does_not_have() {
        // The same content read as .txt is prose, not headings: a hash at the
        // start of a line in a log file is not a heading, and guessing is
        // exactly the Markdown ambiguity this format refuses.
        let secs = parse(MD, false);
        assert!(secs.iter().all(|s| s.kind == Kind::Paragraph));
    }

    #[test]
    fn non_utf8_input_is_refused_with_a_position() {
        let bad = [b'h', b'i', 0xFF, 0xFE];
        let err = match Emt.import(&Source::new(&bad, "broken.txt", "txt")) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("invalid UTF-8 was accepted"),
        };
        assert!(err.contains("not valid UTF-8"), "{err}");
        assert!(err.contains("byte 2"), "{err}");
    }

    #[test]
    fn sections_survive_a_chunk_round_trip() {
        let original = parse(MD, true);
        let f = build(MD, "md");
        assert_eq!(sections(&f).unwrap(), original);
    }

    #[test]
    fn the_summary_is_a_fraction_of_the_payload() {
        let long = PROSE.repeat(400);
        let mut f = build(&long, "txt");
        let data = f.chunks.get(ChunkType::DATA).unwrap().value.len();
        let summ = f.chunks.get(ChunkType::SUMM).unwrap().value.len();
        assert!(summ * 20 < data, "summary {summ} vs data {data}");

        f.set_data(&ChunkList::new()).unwrap(); // prove render never reads DATA
        let mut buf = Vec::new();
        Emt.render(
            &f,
            &RenderOpts {
                summary: true,
                ..Default::default()
            },
            &mut buf,
        )
        .unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("words"));
        assert!(text.contains("min to read"));
    }

    #[test]
    fn the_summary_outline_lists_headings() {
        let f = build(MD, "md");
        let mut buf = Vec::new();
        Emt.render(
            &f,
            &RenderOpts {
                summary: true,
                ..Default::default()
            },
            &mut buf,
        )
        .unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("Heading"));
        assert!(text.contains("Sub-heading"));
    }

    #[test]
    fn editing_one_paragraph_reports_one_section() {
        let a = build(PROSE, "txt");
        let b = build(
            &PROSE.replace("A second one.", "A second one, revised."),
            "txt",
        );
        let d = Emt.diff(&a, &b, &libwick::KeyRing::empty()).unwrap();
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(d[0].path.starts_with("section "));
        assert!(d[0].note.contains("paragraph"));
    }

    #[test]
    fn an_unchanged_file_diffs_to_nothing() {
        let a = build(MD, "md");
        let b = build(MD, "md");
        assert!(Emt
            .diff(&a, &b, &libwick::KeyRing::empty())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn validation_catches_a_hand_edited_payload() {
        let mut f = build(PROSE, "txt");
        let mut data = f.data().unwrap();
        let mut bad = Section::new(Kind::Heading, "line one\nline two", "\n");
        bad.level = 1;
        data.push(bad.encode());
        f.set_data(&data).unwrap();

        let issues = Emt.validate(&f).unwrap();
        assert!(issues
            .iter()
            .any(|i| i.message.contains("more than one line")));
    }
}
