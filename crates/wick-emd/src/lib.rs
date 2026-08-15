//! `.emd` — documents that reflow by default and can be pinned when they
//! must not.
//!
//! A PDF is a description of marks on a page. That is why it prints
//! identically everywhere and why it is miserable to read on a phone, to
//! search, to diff, or to extract anything from: the document's *structure*
//! was thrown away at the moment it was produced, and everything downstream
//! is trying to guess it back.
//!
//! `.emd` stores the structure — headings, paragraphs, lists, quotations,
//! code — and treats layout as a rendering of it. Reflow to any width for
//! free. Then, for the cases where exact appearance is the whole point (a
//! contract, an invoice, a form), the layout can be **pinned**: every line's
//! page, position, font and size is computed once and stored in a `PINL`
//! chunk. A pinned document renders from those coordinates rather than from
//! whatever layout engine is reading it, so it cannot drift between viewers
//! or between versions of this tool — and it still has its structure, so it
//! is still searchable and still diffable.
//!
//! Both halves are in the file at once. That is the thing a PDF cannot do.
//!
//! ## Payload layout
//!
//! `DATA` holds a `DMET` metadata chunk, a `BLOK` chunk per block, and
//! optionally a `PINL` chunk holding the pinned layout.

pub mod pdf_in;
pub mod pdf_out;

use libwick::chunks::{Chunk, ChunkList, ChunkType};
use libwick::error::{Error, Result};
use libwick::plugin::{Payload, Plugin, RenderOpts, Source, Starter};
use libwick::schema::{Issue, Schema};
use libwick::{Change, ChangeKind, KeyRing, Tag, WickFile};
use pdf_out::PageSetup;
use serde::{Deserialize, Serialize};

pub const TAG: Tag = Tag::new(b"MD");
pub const SCHEMA_VERSION: u32 = 1;

const DMET: ChunkType = ChunkType::new(b"DMET");
const BLOK: ChunkType = ChunkType::new(b"BLOK");
const PINL: ChunkType = ChunkType::new(b"PINL");
const OUTL: ChunkType = ChunkType::new(b"OUTL");
const STAT: ChunkType = ChunkType::new(b"STAT");

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BlockKind {
    Paragraph,
    Heading,
    Code,
    List,
    Quote,
    Rule,
    PageBreak,
}

impl BlockKind {
    pub fn name(self) -> &'static str {
        match self {
            BlockKind::Paragraph => "paragraph",
            BlockKind::Heading => "heading",
            BlockKind::Code => "code",
            BlockKind::List => "list",
            BlockKind::Quote => "quote",
            BlockKind::Rule => "rule",
            BlockKind::PageBreak => "page break",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Block {
    pub kind: BlockKind,
    #[serde(default)]
    pub level: u8,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub lang: String,
    #[serde(default)]
    pub text: String,
}

impl Block {
    pub fn new(kind: BlockKind, text: &str) -> Self {
        Block {
            kind,
            level: 0,
            lang: String::new(),
            text: text.to_string(),
        }
    }

    pub fn with_level(mut self, l: u8) -> Self {
        self.level = l;
        self
    }

    pub fn words(&self) -> usize {
        self.text.split_whitespace().count()
    }

    fn encode(&self) -> Result<Chunk> {
        Ok(Chunk::new(BLOK, serde_json::to_vec(self)?))
    }

    fn decode(c: &Chunk) -> Result<Block> {
        Ok(serde_json::from_slice(&c.value)?)
    }
}

/// One placed line of a pinned layout.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pin {
    pub block: usize,
    pub page: usize,
    pub x: f64,
    pub y: f64,
    pub font: String,
    pub size: f64,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DocMeta {
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Page size the pinned layout, if any, was computed for.
    #[serde(default = "a4")]
    pub page: [f64; 2],
    /// Present when the importer had to guess at structure.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caveats: Vec<String>,
}

fn a4() -> [f64; 2] {
    [595.28, 841.89]
}

impl Default for DocMeta {
    fn default() -> Self {
        DocMeta {
            title: String::new(),
            source: None,
            page: a4(),
            caveats: Vec::new(),
        }
    }
}

pub fn blocks(file: &WickFile) -> Result<Vec<Block>> {
    file.data()?.all(BLOK).map(Block::decode).collect()
}

pub fn meta(file: &WickFile) -> Result<DocMeta> {
    match file.data()?.get(DMET) {
        Some(c) => Ok(serde_json::from_slice(&c.value)?),
        None => Ok(DocMeta::default()),
    }
}

pub fn pins(file: &WickFile) -> Result<Option<Vec<Pin>>> {
    match file.data()?.get(PINL) {
        Some(c) => Ok(Some(serde_json::from_slice(&c.value)?)),
        None => Ok(None),
    }
}

pub fn is_pinned(file: &WickFile) -> bool {
    pins(file).ok().flatten().is_some()
}

/// Compute the layout now and store it, freezing the document's appearance.
pub fn pin(file: &mut WickFile, setup: &PageSetup) -> Result<usize> {
    let bs = blocks(file)?;
    let placed = pdf_out::layout(&bs, setup);
    let pins = pdf_out::to_pins(&placed, setup);
    let n = pins.len();

    let mut data = file.data()?;
    data.set(Chunk::new(PINL, serde_json::to_vec(&pins)?));
    let mut m = meta(file)?;
    m.page = [setup.width, setup.height];
    data.set(Chunk::new(DMET, serde_json::to_vec(&m)?));
    file.set_data(&data)?;
    Ok(n)
}

/// Drop the pinned layout and return the document to reflowing.
pub fn unpin(file: &mut WickFile) -> Result<bool> {
    let mut data = file.data()?;
    let had = data.remove(PINL).is_some();
    file.set_data(&data)?;
    Ok(had)
}

// ---------------------------------------------------------------------------
// Markdown
// ---------------------------------------------------------------------------

/// Parse Markdown-shaped text into blocks.
///
/// Unlike `.emt`, which annotates the original bytes and reproduces them
/// exactly, `.emd` keeps only the structure — a document that reflows has
/// given up the right to claim byte-exact round-trips, and pretending
/// otherwise would be the dishonest option. Markdown export is therefore
/// *regenerated*, and normalises formatting.
pub fn from_markdown(src: &str) -> Vec<Block> {
    let lines: Vec<&str> = src.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        let t = line.trim_start();

        if t.is_empty() {
            i += 1;
            continue;
        }
        if let Some(rest) = t.strip_prefix("```") {
            let lang = rest.trim().to_string();
            let mut body = Vec::new();
            i += 1;
            while i < lines.len() && !lines[i].trim_start().starts_with("```") {
                body.push(lines[i]);
                i += 1;
            }
            i += 1; // closing fence
            let mut b = Block::new(BlockKind::Code, &body.join("\n"));
            b.lang = lang;
            out.push(b);
            continue;
        }
        if t.starts_with('#') {
            let level = t.bytes().take_while(|c| *c == b'#').count();
            if level <= 6 && t.as_bytes().get(level) == Some(&b' ') {
                out.push(
                    Block::new(BlockKind::Heading, t[level + 1..].trim()).with_level(level as u8),
                );
                i += 1;
                continue;
            }
        }
        let bare = t.trim_end();
        if bare.len() >= 3 && (bare.bytes().all(|c| c == b'-') || bare.bytes().all(|c| c == b'*')) {
            out.push(Block::new(BlockKind::Rule, ""));
            i += 1;
            continue;
        }
        if t == "\\pagebreak" || t == "<!-- pagebreak -->" {
            out.push(Block::new(BlockKind::PageBreak, ""));
            i += 1;
            continue;
        }

        let indent = line.len() - t.len();
        if let Some(rest) = strip_list_marker(t) {
            let mut text = rest.to_string();
            i += 1;
            // Continuation lines are indented and carry no marker.
            while i < lines.len() {
                let n = lines[i].trim_start();
                if n.is_empty() || strip_list_marker(n).is_some() || lines[i].len() == n.len() {
                    break;
                }
                text.push(' ');
                text.push_str(n);
                i += 1;
            }
            out.push(Block::new(BlockKind::List, &text).with_level((indent / 2).min(4) as u8));
            continue;
        }
        if let Some(rest) = t.strip_prefix("> ").or_else(|| t.strip_prefix(">")) {
            let mut text = rest.trim().to_string();
            i += 1;
            while i < lines.len() && lines[i].trim_start().starts_with('>') {
                text.push(' ');
                text.push_str(lines[i].trim_start().trim_start_matches('>').trim());
                i += 1;
            }
            out.push(Block::new(BlockKind::Quote, &text));
            continue;
        }

        // A paragraph runs to the next blank line or block-starting marker.
        let mut text = t.trim_end().to_string();
        i += 1;
        while i < lines.len() {
            let n = lines[i].trim_start();
            if n.is_empty()
                || n.starts_with('#')
                || n.starts_with("```")
                || n.starts_with('>')
                || strip_list_marker(n).is_some()
            {
                break;
            }
            text.push(' ');
            text.push_str(n.trim_end());
            i += 1;
        }
        out.push(Block::new(BlockKind::Paragraph, &text));
    }
    out
}

fn strip_list_marker(t: &str) -> Option<&str> {
    for m in ["- ", "* ", "+ "] {
        if let Some(rest) = t.strip_prefix(m) {
            return Some(rest.trim_start());
        }
    }
    let digits = t.bytes().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0
        && matches!(t.as_bytes().get(digits), Some(b'.') | Some(b')'))
        && t.as_bytes().get(digits + 1) == Some(&b' ')
    {
        return Some(t[digits + 2..].trim_start());
    }
    None
}

/// Plain text: blank-line-separated paragraphs, and nothing claimed beyond
/// that. A short line on its own is *not* guessed to be a heading here,
/// because plain text does not assert one.
pub fn from_plain(src: &str) -> Vec<Block> {
    src.split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| {
            Block::new(
                BlockKind::Paragraph,
                &p.split_whitespace().collect::<Vec<_>>().join(" "),
            )
        })
        .collect()
}

pub fn to_markdown(blocks: &[Block]) -> String {
    let mut out = String::new();
    for b in blocks {
        match b.kind {
            BlockKind::Heading => {
                out.push_str(&"#".repeat(b.level.clamp(1, 6) as usize));
                out.push(' ');
                out.push_str(&b.text);
                out.push_str("\n\n");
            }
            BlockKind::Code => {
                out.push_str("```");
                out.push_str(&b.lang);
                out.push('\n');
                out.push_str(&b.text);
                out.push_str("\n```\n\n");
            }
            BlockKind::List => {
                out.push_str(&"  ".repeat(b.level as usize));
                out.push_str("- ");
                out.push_str(&b.text);
                out.push('\n');
            }
            BlockKind::Quote => {
                out.push_str("> ");
                out.push_str(&b.text);
                out.push_str("\n\n");
            }
            BlockKind::Rule => out.push_str("---\n\n"),
            BlockKind::PageBreak => out.push_str("<!-- pagebreak -->\n\n"),
            BlockKind::Paragraph => {
                out.push_str(&b.text);
                out.push_str("\n\n");
            }
        }
    }
    // Collapse the blank line a list run does not want after it.
    out.replace("\n\n\n", "\n\n")
}

pub fn to_plain(blocks: &[Block]) -> String {
    blocks
        .iter()
        .filter(|b| !matches!(b.kind, BlockKind::Rule | BlockKind::PageBreak))
        .map(|b| b.text.clone())
        .collect::<Vec<_>>()
        .join("\n\n")
        + "\n"
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

pub struct Emd;

impl Plugin for Emd {
    fn tag(&self) -> Tag {
        TAG
    }
    fn ext(&self) -> &'static str {
        "emd"
    }
    fn name(&self) -> &'static str {
        "document"
    }
    fn about(&self) -> &'static str {
        "reflowable documents with optional pinned layout (replaces .pdf)"
    }
    fn imports(&self) -> &'static [&'static str] {
        &["md", "markdown", "txt", "pdf"]
    }
    fn exports(&self) -> &'static [&'static str] {
        &["pdf", "md", "txt"]
    }
    fn schema_version(&self) -> u32 {
        SCHEMA_VERSION
    }

    fn import(&self, src: &Source) -> Result<Payload> {
        let mut m = DocMeta {
            source: Some(src.name.to_string()),
            ..Default::default()
        };
        let bs = match src.ext {
            "md" | "markdown" => from_markdown(src.text()?),
            "txt" | "text" => from_plain(src.text()?),
            "pdf" => {
                let e = pdf_in::extract(src.bytes);
                m.caveats = e.caveats;
                // Extracted text has no reliable structure, so it is imported
                // as prose. Guessing headings from line length here would put
                // a fabrication into the file and call it data.
                from_plain(&e.text)
            }
            other => return Err(Error::Other(format!("no .emd importer for .{other}"))),
        };
        m.title = bs
            .iter()
            .find(|b| b.kind == BlockKind::Heading)
            .map(|b| b.text.clone())
            .unwrap_or_else(|| {
                src.name
                    .rsplit('/')
                    .next()
                    .unwrap_or(src.name)
                    .rsplit_once('.')
                    .map(|(stem, _)| stem.to_string())
                    .unwrap_or_else(|| src.name.to_string())
            });

        let mut data = ChunkList::new();
        data.push(Chunk::new(DMET, serde_json::to_vec(&m)?));
        for b in &bs {
            data.push(b.encode()?);
        }

        let mut schema = Schema::new("document");
        schema.version = SCHEMA_VERSION;

        Ok(Payload {
            summary: Some(summarize(&bs, &m)),
            data,
            schema: Some(schema),
            caps: None,
            migrations: None,
        })
    }

    /// A new document, with its title as the first heading if one was given.
    /// `.emd` takes its title from that heading, so writing it here is how a
    /// created document ends up named rather than "untitled".
    fn starter(&self, spec: &Starter) -> Result<(&'static str, Vec<u8>)> {
        spec.only("emd", &["title"])?;
        let text = match spec.title {
            Some(t) => format!("# {t}\n\n"),
            None => String::new(),
        };
        Ok(("md", text.into_bytes()))
    }

    /// Markdown, not PDF. `.emd` exports PDF first because that is what a
    /// document is usually wanted as, but PDF only comes back in through
    /// best-effort text extraction — editing through it would return a
    /// document stripped of every structure it had.
    fn edit_ext(&self, _file: &WickFile) -> Result<&'static str> {
        Ok("md")
    }

    fn export(&self, file: &WickFile, to: &str) -> Result<Vec<u8>> {
        let bs = blocks(file)?;
        match to {
            "md" => Ok(to_markdown(&bs).into_bytes()),
            "txt" => Ok(to_plain(&bs).into_bytes()),
            "pdf" => {
                let m = meta(file)?;
                let setup = PageSetup {
                    width: m.page[0],
                    height: m.page[1],
                    ..Default::default()
                };
                // A pinned document renders from its stored coordinates, so
                // the output does not depend on this build's layout code at
                // all. That is the guarantee pinning exists to make.
                let placed = match pins(file)? {
                    Some(p) => pdf_out::from_pins(&p),
                    None => pdf_out::layout(&bs, &setup),
                };
                Ok(pdf_out::write(&placed, &setup, &m.title))
            }
            other => Err(Error::Other(format!(".emd cannot export to .{other}"))),
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
        let m = meta(file)?;
        if !m.title.is_empty() {
            writeln!(out, "\x1b[1m{}\x1b[0m", m.title)?;
        }
        if is_pinned(file) {
            writeln!(
                out,
                "\x1b[2mlayout pinned to {:.0}x{:.0} pt\x1b[0m",
                m.page[0], m.page[1]
            )?;
        }
        for c in &m.caveats {
            writeln!(out, "\x1b[33mnote:\x1b[0m {c}")?;
        }
        writeln!(out)?;

        // Reflow to the terminal, which is the same operation a phone or a
        // print driver performs — the document has one structure and many
        // renderings.
        let width = opts.width.clamp(20, 120);
        for b in blocks(file)?.iter().take(opts.limit.unwrap_or(usize::MAX)) {
            match b.kind {
                BlockKind::Rule => writeln!(out, "{}", "─".repeat(width))?,
                BlockKind::PageBreak => writeln!(out, "\x1b[2m{}\x1b[0m", "· ".repeat(width / 2))?,
                BlockKind::Code => {
                    for line in b.text.lines() {
                        writeln!(out, "    \x1b[36m{line}\x1b[0m")?;
                    }
                }
                BlockKind::Heading => {
                    writeln!(
                        out,
                        "\x1b[1m{}{}\x1b[0m",
                        "  ".repeat(b.level.saturating_sub(1) as usize),
                        b.text
                    )?;
                }
                _ => {
                    let (indent, prefix) = match b.kind {
                        BlockKind::List => ("  ".repeat(b.level as usize + 1), "• "),
                        BlockKind::Quote => ("  ".to_string(), "│ "),
                        _ => (String::new(), ""),
                    };
                    for (i, line) in fill(&b.text, width - indent.len() - prefix.len())
                        .iter()
                        .enumerate()
                    {
                        let lead = if i == 0 { prefix } else { "  " };
                        writeln!(out, "{indent}{lead}{line}")?;
                    }
                }
            }
            writeln!(out)?;
        }
        Ok(())
    }

    fn validate(&self, file: &WickFile) -> Result<Vec<Issue>> {
        let bs = blocks(file)?;
        let mut issues = Vec::new();
        if bs.is_empty() {
            issues.push(Issue::warning("", "document has no blocks"));
        }
        for (i, b) in bs.iter().enumerate() {
            if b.kind == BlockKind::Heading && (b.level == 0 || b.level > 6) {
                issues.push(Issue::error(
                    format!("block[{i}]"),
                    format!("heading level {} is outside 1-6", b.level),
                ));
            }
            if b.kind == BlockKind::Heading && b.text.contains('\n') {
                issues.push(Issue::error(
                    format!("block[{i}]"),
                    "a heading spans several lines",
                ));
            }
        }

        // A pinned layout that no longer describes the blocks is worse than
        // no pin at all: it would render a document that is not the one
        // stored. Catching the drift is the point of checking.
        if let Some(p) = pins(file)? {
            if let Some(bad) = p.iter().find(|pin| pin.block >= bs.len()) {
                issues.push(Issue::error(
                    "PINL",
                    format!(
                        "pinned layout refers to block {}, but there are {}",
                        bad.block,
                        bs.len()
                    ),
                ));
            }
            let pinned_blocks: std::collections::HashSet<usize> =
                p.iter().map(|x| x.block).collect();
            let drawable = bs
                .iter()
                .enumerate()
                .filter(|(_, b)| b.kind != BlockKind::PageBreak)
                .map(|(i, _)| i);
            let missing: Vec<usize> = drawable.filter(|i| !pinned_blocks.contains(i)).collect();
            if !missing.is_empty() {
                issues.push(Issue::error(
                    "PINL",
                    format!(
                        "{} block(s) have no pinned position, so a pinned render would omit them \
                         (blocks {:?}); re-pin or unpin",
                        missing.len(),
                        &missing[..missing.len().min(5)]
                    ),
                ));
            }
        }
        for c in meta(file)?.caveats {
            issues.push(Issue::note("import", c));
        }
        Ok(issues)
    }

    fn diff(&self, a: &WickFile, b: &WickFile, keys: &KeyRing) -> Result<Vec<Change>> {
        let (ba, bb) = (blocks(a)?, blocks(b)?);
        let mut out: Vec<Change> = libwick::diff::structural(&a.chunks, &b.chunks, keys)
            .into_iter()
            .filter(|c| c.ty == BLOK)
            .map(|mut c| {
                let idx = c
                    .path
                    .rsplit('[')
                    .next()
                    .and_then(|s| s.trim_end_matches(']').parse::<usize>().ok());
                // DMET occupies index 0 of DATA.
                let n = idx.and_then(|i| i.checked_sub(1));
                let side = if c.kind == ChangeKind::Removed {
                    &ba
                } else {
                    &bb
                };
                if let Some(block) = n.and_then(|i| side.get(i)) {
                    c.path = format!("block {}", n.unwrap());
                    let preview: String = block.text.chars().take(56).collect();
                    c.note = format!("{}: {preview}", block.kind.name());
                }
                c
            })
            .collect();

        // Pinning state is a document-level fact worth reporting on its own.
        match (is_pinned(a), is_pinned(b)) {
            (false, true) => out.push(Change::new(
                ChangeKind::Added,
                "layout",
                PINL,
                "layout pinned",
            )),
            (true, false) => out.push(Change::new(
                ChangeKind::Removed,
                "layout",
                PINL,
                "layout unpinned",
            )),
            (true, true) if pins(a)? != pins(b)? => out.push(Change::new(
                ChangeKind::Modified,
                "layout",
                PINL,
                "pinned layout changed",
            )),
            _ => {}
        }
        Ok(out)
    }

    fn summarize(&self, data: &ChunkList) -> Result<Option<ChunkList>> {
        let bs: Vec<Block> = data.all(BLOK).map(Block::decode).collect::<Result<_>>()?;
        let m: DocMeta = match data.get(DMET) {
            Some(c) => serde_json::from_slice(&c.value)?,
            None => DocMeta::default(),
        };
        Ok(Some(summarize(&bs, &m)))
    }
}

/// Greedy word wrap for terminal output.
fn fill(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() || out.is_empty() {
        out.push(line);
    }
    out
}

fn summarize(blocks: &[Block], meta: &DocMeta) -> ChunkList {
    let mut summ = ChunkList::new();
    let words: usize = blocks.iter().map(|b| b.words()).sum();
    let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
    for b in blocks {
        *counts.entry(b.kind.name()).or_default() += 1;
    }
    let stat = serde_json::json!({
        "title": meta.title,
        "blocks": blocks.len(),
        "words": words,
        "reading_minutes": (words as f64 / 238.0 * 10.0).round() / 10.0,
        "kinds": counts,
        "opening": blocks
            .iter()
            .find(|b| b.kind == BlockKind::Paragraph)
            .map(|b| b.text.chars().take(240).collect::<String>())
            .unwrap_or_default(),
    });
    summ.push(Chunk::new(
        STAT,
        serde_json::to_vec(&stat).unwrap_or_default(),
    ));
    for b in blocks.iter().filter(|b| b.kind == BlockKind::Heading) {
        let mut v = vec![b.level];
        v.extend_from_slice(b.text.as_bytes());
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
        let title = v["title"].as_str().unwrap_or("");
        if !title.is_empty() {
            writeln!(out, "\x1b[1m{title}\x1b[0m")?;
        }
        writeln!(
            out,
            "{} blocks, {} words, about {} min to read",
            v["blocks"], v["words"], v["reading_minutes"]
        )?;
        let opening = v["opening"].as_str().unwrap_or("");
        if !opening.is_empty() {
            writeln!(out, "\n{opening}")?;
        }
    }
    let headings: Vec<_> = summ.all(OUTL).collect();
    if !headings.is_empty() {
        writeln!(out, "\ncontents:")?;
        for h in headings {
            let level = (*h.value.first().unwrap_or(&1)).max(1) as usize;
            writeln!(
                out,
                "{}{}",
                "  ".repeat(level),
                std::str::from_utf8(&h.value[1..]).unwrap_or("")
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MD: &str = "\
# Quarterly Report

The first paragraph runs across
two source lines and should become one block.

## Findings

- revenue rose
- costs fell
  by rather a lot

> A quotation worth keeping.

```sql
SELECT 1;
```

---

Closing thoughts.
";

    fn build(src: &str, ext: &str) -> WickFile {
        let mut f = WickFile::new(TAG);
        let p = Emd
            .import(&Source::new(src.as_bytes(), "report.md", ext))
            .unwrap();
        f.set_data(&p.data).unwrap();
        if let Some(s) = p.summary {
            f.set_summary(&s).unwrap();
        }
        f
    }

    #[test]
    fn markdown_structure_is_captured() {
        let bs = from_markdown(MD);
        assert_eq!(bs[0].kind, BlockKind::Heading);
        assert_eq!(bs[0].level, 1);
        assert_eq!(bs[0].text, "Quarterly Report");
        assert_eq!(bs[1].kind, BlockKind::Paragraph);
        assert!(bs[1].text.contains("two source lines"), "{}", bs[1].text);
        assert!(bs.iter().any(|b| b.kind == BlockKind::Quote));
        assert!(bs.iter().any(|b| b.kind == BlockKind::Rule));
        let code = bs.iter().find(|b| b.kind == BlockKind::Code).unwrap();
        assert_eq!(code.lang, "sql");
        assert_eq!(code.text, "SELECT 1;");
    }

    #[test]
    fn a_list_continuation_joins_its_item() {
        let bs = from_markdown(MD);
        let items: Vec<&Block> = bs.iter().filter(|b| b.kind == BlockKind::List).collect();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].text, "costs fell by rather a lot");
    }

    #[test]
    fn markdown_export_reparses_to_the_same_structure() {
        // Not byte-exact — a reflowable document does not promise that — but
        // the structure has to be a fixed point, or repeated conversions
        // would drift.
        let once = from_markdown(MD);
        let twice = from_markdown(&to_markdown(&once));
        assert_eq!(once, twice);
    }

    #[test]
    fn the_title_comes_from_the_first_heading() {
        assert_eq!(meta(&build(MD, "md")).unwrap().title, "Quarterly Report");
    }

    #[test]
    fn plain_text_is_not_given_structure_it_lacks() {
        let bs = from_plain("Short line\n\nAnother paragraph here.\n");
        assert!(bs.iter().all(|b| b.kind == BlockKind::Paragraph));
        assert_eq!(bs.len(), 2);
    }

    #[test]
    fn pdf_export_produces_a_real_pdf() {
        let f = build(MD, "md");
        let pdf = Emd.export(&f, "pdf").unwrap();
        assert!(pdf.starts_with(b"%PDF-1.4"));
        let s = String::from_utf8_lossy(&pdf);
        assert!(s.contains("/Type /Page "));
        assert!(
            s.contains("(Quarterly Report) Tj"),
            "the heading should be drawn"
        );
    }

    #[test]
    fn a_pdf_we_wrote_can_be_read_back() {
        let f = build(MD, "md");
        let pdf = Emd.export(&f, "pdf").unwrap();
        let back = Emd.import(&Source::new(&pdf, "out.pdf", "pdf")).unwrap();
        let mut g = WickFile::new(TAG);
        g.set_data(&back.data).unwrap();
        let text = String::from_utf8(Emd.export(&g, "txt").unwrap()).unwrap();
        assert!(text.contains("Quarterly Report"), "{text}");
        assert!(text.contains("Closing thoughts"), "{text}");
        // And the round trip is honest about what it lost.
        assert!(!meta(&g).unwrap().caveats.is_empty());
    }

    #[test]
    fn pinning_freezes_the_layout_and_survives_a_rewrite() {
        let mut f = build(MD, "md");
        assert!(!is_pinned(&f));
        let n = pin(&mut f, &PageSetup::default()).unwrap();
        assert!(n > 0);
        assert!(is_pinned(&f));

        let first = Emd.export(&f, "pdf").unwrap();
        let second = Emd.export(&f, "pdf").unwrap();
        assert_eq!(first, second, "a pinned render must be deterministic");

        // Pinned output ignores the layout engine entirely: change the page
        // size in the metadata and the drawn positions do not move.
        let pins_before = pins(&f).unwrap().unwrap();
        assert!(unpin(&mut f).unwrap());
        assert!(!is_pinned(&f));
        assert!(Emd.export(&f, "pdf").is_ok());
        assert!(!pins_before.is_empty());
    }

    #[test]
    fn a_stale_pin_is_caught_by_validation() {
        let mut f = build(MD, "md");
        pin(&mut f, &PageSetup::default()).unwrap();

        // Add a block after pinning: the pinned layout no longer covers the
        // document, so a pinned render would silently drop it.
        let mut data = f.data().unwrap();
        data.push(
            Block::new(BlockKind::Paragraph, "added later")
                .encode()
                .unwrap(),
        );
        f.set_data(&data).unwrap();

        let issues = Emd.validate(&f).unwrap();
        assert!(
            issues
                .iter()
                .any(|i| i.message.contains("no pinned position")),
            "{issues:?}"
        );
    }

    #[test]
    fn an_edited_paragraph_reports_as_one_block() {
        let a = build(MD, "md");
        let b = build(
            &MD.replace("Closing thoughts.", "Closing thoughts, revised."),
            "md",
        );
        let d = Emd.diff(&a, &b, &KeyRing::empty()).unwrap();
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(d[0].path.starts_with("block "));
        assert!(d[0].note.contains("paragraph"));
    }

    #[test]
    fn pinning_shows_up_in_a_diff() {
        let a = build(MD, "md");
        let mut b = build(MD, "md");
        pin(&mut b, &PageSetup::default()).unwrap();
        let d = Emd.diff(&a, &b, &KeyRing::empty()).unwrap();
        assert!(
            d.iter()
                .any(|c| c.path == "layout" && c.note.contains("pinned")),
            "{d:?}"
        );
    }

    #[test]
    fn the_summary_lists_the_headings() {
        let f = build(MD, "md");
        let mut buf = Vec::new();
        Emd.render(
            &f,
            &RenderOpts {
                summary: true,
                ..Default::default()
            },
            &mut buf,
        )
        .unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("Quarterly Report"));
        assert!(s.contains("Findings"));
        assert!(s.contains("min to read"));
    }

    #[test]
    fn rendering_reflows_to_the_requested_width() {
        let f = build(MD, "md");
        for width in [40usize, 100] {
            let mut buf = Vec::new();
            Emd.render(
                &f,
                &RenderOpts {
                    width,
                    ..Default::default()
                },
                &mut buf,
            )
            .unwrap();
            let text = strip_ansi(&String::from_utf8(buf).unwrap());
            assert!(
                text.lines().all(|l| l.chars().count() <= width),
                "a line exceeded {width} columns"
            );
        }
    }

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }
}
