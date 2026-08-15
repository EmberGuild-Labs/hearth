//! Machine-readable output.
//!
//! Hearth's ordinary output is written for a person: aligned columns, colour,
//! dimmed asides, counts phrased as sentences. All of that is hostile to a
//! script, and a script that resorts to parsing it will break the first time
//! a column moves.
//!
//! So every command that reports something has a `--json` form, and this is
//! where those shapes are defined — in one place, so they stay consistent
//! with each other. Two rules hold across all of them:
//!
//! * **Data goes to stdout, everything else to stderr.** A `--json` run
//!   writes exactly one JSON document to stdout and nothing else, so
//!   `hearth info x.emt --json | jq` works without `--quiet`.
//! * **The exit status still means what it meant.** `--json` changes the
//!   shape of the answer, never the verdict: `diff` still exits 1 when files
//!   differ, `validate` still exits 1 when a file fails its own rules.

use libwick::diff::Change;
use libwick::schema::{Issue, Severity};
use serde_json::{json, Value};

pub fn severity(s: Severity) -> &'static str {
    match s {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Note => "note",
    }
}

pub fn issue(i: &Issue) -> Value {
    json!({
        "severity": severity(i.severity),
        "path": i.path,
        "message": i.message,
    })
}

pub fn change(c: &Change) -> Value {
    json!({
        "kind": match c.kind {
            libwick::ChangeKind::Added => "added",
            libwick::ChangeKind::Removed => "removed",
            libwick::ChangeKind::Modified => "modified",
            libwick::ChangeKind::Moved => "moved",
        },
        "path": c.path,
        "chunk": c.ty.to_string(),
        "note": c.note,
    })
}

/// Print one JSON document to stdout, and nothing else.
pub fn emit(v: &Value) -> anyhow::Result<()> {
    use std::io::Write;
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    writeln!(out, "{}", serde_json::to_string_pretty(v)?)?;
    out.flush()?;
    Ok(())
}
