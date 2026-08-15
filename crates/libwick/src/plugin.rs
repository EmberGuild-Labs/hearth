//! The interface a format plugin implements.
//!
//! The whole architectural bet of the ecosystem is here: adding a format
//! should mean writing this trait, not writing a file format. A plugin
//! defines what goes inside `DATA`, how to get there from a legacy file, how
//! to get back out, and how to display and compare the result. It never
//! touches the header, the chunk table, hashing, provenance, encryption or
//! the migration engine, because those are the same for every format and
//! reimplementing them per format is precisely the failure the spine exists
//! to prevent.
//!
//! Every method except the first six has a default. A new format is viable
//! with import, export and render; validation, semantic diff, summaries and
//! custom migration operations are improvements a plugin adds when it has
//! something better to say than the generic answer.

use crate::caps::Capabilities;
use crate::chunks::ChunkList;
use crate::crypto::KeyRing;
use crate::diff::Change;
use crate::error::Result;
use crate::file::WickFile;
use crate::header::Tag;
use crate::migrate::{Op, RuleSet};
use crate::schema::{Issue, Schema};

/// A legacy file on its way in.
pub struct Source<'a> {
    pub bytes: &'a [u8],
    /// Original file name, for provenance and for format sniffing.
    pub name: &'a str,
    /// Lowercased extension without the dot: `txt`, `csv`, `png`.
    pub ext: &'a str,
}

impl<'a> Source<'a> {
    pub fn new(bytes: &'a [u8], name: &'a str, ext: &'a str) -> Self {
        Source { bytes, name, ext }
    }

    pub fn text(&self) -> Result<&'a str> {
        std::str::from_utf8(self.bytes).map_err(|e| {
            crate::error::Error::Other(format!(
                "{} is not valid UTF-8 (byte {})",
                self.name,
                e.valid_up_to()
            ))
        })
    }
}

/// Everything a plugin produces from one legacy file. The spine assembles
/// these into chunks, computes hashes, and writes the container.
#[derive(Default)]
pub struct Payload {
    pub data: ChunkList,
    pub schema: Option<Schema>,
    pub summary: Option<ChunkList>,
    pub caps: Option<Capabilities>,
    pub migrations: Option<RuleSet>,
}

/// What `hearth create` was asked for.
///
/// Every field is optional and most are meaningful to exactly one format. A
/// plugin takes what applies to its payload and rejects the rest through
/// [`Starter::only`]: an option that is silently ignored is worse than one
/// that fails, because the file it produces is not the file that was asked
/// for and nothing says so.
#[derive(Default, Clone, Copy, Debug)]
pub struct Starter<'a> {
    /// A title or opening heading, for formats that have one.
    pub title: Option<&'a str>,
    /// Column list in the plugin's own header syntax, for tabular formats.
    pub columns: Option<&'a str>,
    /// Canvas size in pixels, for raster formats.
    pub size: Option<(u32, u32)>,
}

impl Starter<'_> {
    /// Accept only the named options; error on any other that was supplied.
    /// `allow` holds the long flag names without dashes: `"title"`,
    /// `"columns"`, `"size"`.
    pub fn only(&self, ext: &str, allow: &[&str]) -> Result<()> {
        let supplied = [
            ("title", self.title.is_some()),
            ("columns", self.columns.is_some()),
            ("size", self.size.is_some()),
        ];
        let stray: Vec<String> = supplied
            .iter()
            .filter(|(name, given)| *given && !allow.contains(name))
            .map(|(name, _)| format!("--{name}"))
            .collect();
        if stray.is_empty() {
            return Ok(());
        }
        Err(crate::error::Error::Other(format!(
            "{} {} nothing to a .{ext} file",
            stray.join(" and "),
            if stray.len() == 1 { "means" } else { "mean" },
        )))
    }
}

#[derive(Clone, Debug)]
pub struct RenderOpts {
    /// Render from `SUMM` alone and never touch `DATA`.
    pub summary: bool,
    /// Terminal width to wrap to.
    pub width: usize,
    /// Stop after this many payload units (lines, rows, sections).
    pub limit: Option<usize>,
    pub color: bool,
}

impl Default for RenderOpts {
    fn default() -> Self {
        RenderOpts {
            summary: false,
            width: 80,
            limit: None,
            color: false,
        }
    }
}

pub trait Plugin: Send + Sync {
    /// The two-character format tag in the header.
    fn tag(&self) -> Tag;
    /// The file extension this plugin owns, without the dot.
    fn ext(&self) -> &'static str;
    /// Short human name: "text", "config", "table".
    fn name(&self) -> &'static str;
    /// One line for `hearth formats`.
    fn about(&self) -> &'static str;
    /// Legacy extensions this plugin can import.
    fn imports(&self) -> &'static [&'static str];
    /// Legacy extensions this plugin can export back to.
    fn exports(&self) -> &'static [&'static str];

    fn import(&self, src: &Source) -> Result<Payload>;

    /// Starter content for a brand-new file, as a legacy source this plugin
    /// can import: `(extension, bytes)`.
    ///
    /// It returns a legacy source rather than a [`Payload`] on purpose, so
    /// that `hearth create` and `hearth convert` build a file through exactly
    /// the same code. There is then only one way a file of a given format
    /// comes into being, and a created file cannot differ from a converted
    /// one in some detail nobody thought to check.
    ///
    /// A format that cannot describe a new document without being told
    /// something first — a table has no columns, an image has no size — says
    /// so here rather than inventing an answer.
    fn starter(&self, spec: &Starter) -> Result<(&'static str, Vec<u8>)> {
        let _ = spec;
        Err(crate::error::Error::Other(format!(
            ".{} cannot be created empty; convert an existing file into it instead",
            self.ext()
        )))
    }

    /// Render back to a legacy format. `to` is one of [`Plugin::exports`].
    fn export(&self, file: &WickFile, to: &str) -> Result<Vec<u8>>;

    /// The legacy format `hearth edit` should round-trip *this* file
    /// through. It has to be one the plugin both exports and imports, or the
    /// edit could not be read back in.
    ///
    /// The default — the first extension in both lists — is right for a
    /// format with one obvious answer. A format with several picks the one
    /// that loses least for the file in hand: a `.emt` that came from
    /// Markdown goes back out as Markdown, because handing the editor plain
    /// text would return every heading as a paragraph.
    fn edit_ext(&self, file: &WickFile) -> Result<&'static str> {
        let _ = file;
        self.exports()
            .iter()
            .copied()
            .find(|e| self.imports().contains(e))
            .ok_or_else(|| {
                crate::error::Error::Other(format!(
                    ".{} exports to .{} but imports none of them, so an edit could not be \
                     read back in",
                    self.ext(),
                    self.exports().join(", .")
                ))
            })
    }

    /// Human-readable display for `hearth view`.
    fn render(
        &self,
        file: &WickFile,
        opts: &RenderOpts,
        out: &mut dyn std::io::Write,
    ) -> Result<()>;

    /// The payload schema version this build writes. `MIGR` rules move a
    /// file's `SCHM` version up to this.
    fn schema_version(&self) -> u32 {
        1
    }

    /// Format-specific checks beyond what `SCHM` can express.
    fn validate(&self, file: &WickFile) -> Result<Vec<Issue>> {
        let _ = file;
        Ok(Vec::new())
    }

    /// Semantic diff. The default is the spine's structural walk, which is
    /// correct but says "chunk 4 changed" where a plugin would say
    /// "database.port: 5432 -> 5433".
    fn diff(&self, a: &WickFile, b: &WickFile, keys: &KeyRing) -> Result<Vec<Change>> {
        Ok(crate::diff::structural(&a.chunks, &b.chunks, keys))
    }

    /// Handle a migration operation the spine does not know. Return `None`
    /// for operations that are not this plugin's, so the engine can report
    /// an honest error rather than skipping them.
    fn migrate_op(&self, op: &Op, data: &mut ChunkList) -> Result<Option<String>> {
        let _ = (op, data);
        Ok(None)
    }

    /// Rebuild the cheap tier from the full payload. Called after a
    /// migration, when the summary a file was written with may no longer
    /// describe it.
    fn summarize(&self, data: &ChunkList) -> Result<Option<ChunkList>> {
        let _ = data;
        Ok(None)
    }

    /// Whether `taken` — the leading children of `DATA`, decoded in order —
    /// already holds everything a render under `opts` will show.
    ///
    /// The spine asks before each child and stops at the first
    /// [`Enough::Yes`], so viewing the first twenty rows of a million-row
    /// table decompresses one row group and leaves the rest on disk. See
    /// [`WickFile::read_partial`](crate::file::WickFile::read_partial).
    ///
    /// It asks repeatedly rather than being told a count up front because a
    /// payload does not record how many rows a group holds or how many pixels
    /// a tile covers. A reader that assumed would show twenty rows or five
    /// hundred depending on a constant it cannot see in the file.
    /// The default answers the one case that holds for every format: a
    /// summary render works from `SUMM` and never opens `DATA` at all, which
    /// is what the tier *is*. Everything else needs the payload whole until a
    /// plugin says otherwise.
    fn enough(&self, taken: &ChunkList, opts: &RenderOpts) -> Enough {
        let _ = taken;
        if opts.summary {
            Enough::Yes
        } else {
            Enough::All
        }
    }
}

/// How much of `DATA` a format needs, answered one child at a time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Enough {
    /// The children decoded so far are all this render will use. Stop.
    Yes,
    /// Not yet — decode the next child and ask again.
    More,
    /// This format cannot work from a prefix; read the payload whole.
    ///
    /// The default, and a real answer rather than a fallback: an image is not
    /// most of an image, and a document's word count is not most of a word
    /// count. Saying it here means the spine stops asking, so a format with
    /// nothing to gain from a partial read pays nothing for the question.
    All,
}
