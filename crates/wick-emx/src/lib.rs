//! `.emx` — tabular data with types and units it can check.
//!
//! CSV has no types. Every reader guesses, and the guesses differ: one tool
//! decides a column of `01`, `02`, `03` is numeric and drops the leading
//! zeros, another keeps them as text, and the two disagree forever after.
//! JSON has types but no units, which is the same failure one level up — a
//! column of numbers labelled `distance` is metres or miles depending on who
//! you ask.
//!
//! `.emx` writes both down. A column declares its type and, optionally, its
//! unit, and both travel inside the file. Then the interesting part:
//!
//! ## Unit-mismatched arithmetic fails loud
//!
//! A column can be *computed* — it stores a formula rather than values:
//!
//! ```text
//! speed  float  m/s  = distance / elapsed
//! ```
//!
//! The formula's units are checked against the column's declared unit
//! symbolically, from the schema alone, with no rows involved. `distance /
//! elapsed` yields `m/s` and agrees. `distance + elapsed` is refused at
//! validate time with "cannot add m and s", rather than producing a plausible
//! wrong number that survives into a report. Mixed scales are converted, not
//! rejected: `1 km + 500 m` is 1.5 km.
//!
//! ## Payload layout
//!
//! `DATA` holds one `COLS` chunk describing the columns, then a series of
//! `RGRP` row groups of up to [`ROWS_PER_GROUP`] rows each, stored
//! column-major. Grouping is what keeps a diff proportional to the edit: a
//! changed cell in a million-row table dirties one group, so `hearth diff`
//! decodes two groups rather than two files.

pub mod expr;
pub mod units;

use libwick::chunks::{Chunk, ChunkList, ChunkType};
use libwick::error::{Error, Result};
use libwick::plugin::{Enough, Payload, Plugin, RenderOpts, Source, Starter};
use libwick::schema::{FieldRule, Issue, Schema};
use libwick::value::{format_float, Value};
use libwick::{Change, ChangeKind, KeyRing, Tag, WickFile};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use units::Unit;

pub const TAG: Tag = Tag::new(b"MX");
pub const SCHEMA_VERSION: u32 = 1;

/// Rows per `RGRP` chunk. Small enough that one edited cell dirties a small
/// chunk, large enough that the per-chunk overhead stays negligible.
pub const ROWS_PER_GROUP: usize = 512;

/// Rows `hearth view` shows when nothing says otherwise.
const DEFAULT_ROWS: usize = 20;

const COLS: ChunkType = ChunkType::new(b"COLS");
const RGRP: ChunkType = ChunkType::new(b"RGRP");
const STAT: ChunkType = ChunkType::new(b"STAT");
const HEAD: ChunkType = ChunkType::new(b"HEAD");

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColType {
    Int,
    Float,
    Str,
    Bool,
}

impl ColType {
    pub fn name(self) -> &'static str {
        match self {
            ColType::Int => "int",
            ColType::Float => "float",
            ColType::Str => "string",
            ColType::Bool => "bool",
        }
    }

    pub fn is_numeric(self) -> bool {
        matches!(self, ColType::Int | ColType::Float)
    }

    fn accepts(self, v: &Value) -> bool {
        match (self, v) {
            (_, Value::Null) => true,
            (ColType::Int, Value::Int(_)) => true,
            // A whole number is a legal float; the reverse would need a
            // rounding decision the file has not made.
            (ColType::Float, Value::Float(_) | Value::Int(_)) => true,
            (ColType::Str, Value::Str(_)) => true,
            (ColType::Bool, Value::Bool(_)) => true,
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: ColType,
    /// Unit symbol as written: `m/s`, `USD`, or empty for dimensionless.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unit: String,
    /// A formula, for a computed column. Stored, not evaluated away, so it
    /// can be re-checked and re-run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

impl Column {
    pub fn new(name: &str, ty: ColType) -> Self {
        Column {
            name: name.to_string(),
            ty,
            unit: String::new(),
            expr: None,
            doc: None,
        }
    }

    pub fn with_unit(mut self, u: &str) -> Self {
        self.unit = u.to_string();
        self
    }

    pub fn computed(mut self, e: &str) -> Self {
        self.expr = Some(e.to_string());
        self
    }

    pub fn parsed_unit(&self) -> std::result::Result<Unit, String> {
        Unit::parse(&self.unit)
    }

    /// `distance (m)`, or `speed (m/s) = distance / elapsed` for a computed
    /// column. This is the header a CSV export writes and an import reads,
    /// which is what lets a plain CSV declare a unit and a formula and get
    /// them back unchanged.
    pub fn header(&self) -> String {
        let mut h = if self.unit.is_empty() {
            self.name.clone()
        } else {
            format!("{} ({})", self.name, self.unit)
        };
        if let Some(e) = &self.expr {
            h.push_str(" = ");
            h.push_str(e);
        }
        h
    }
}

/// The whole table in memory. Cells are `Value::Null` when empty; an empty
/// cell is not zero and not an empty string, and collapsing the distinction
/// is how a mean silently shifts.
#[derive(Clone, Debug, Default)]
pub struct Table {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
}

impl Table {
    pub fn column(&self, name: &str) -> Option<(usize, &Column)> {
        self.columns
            .iter()
            .enumerate()
            .find(|(_, c)| c.name == name)
    }

    pub fn unit_map(&self) -> std::result::Result<HashMap<String, Unit>, String> {
        self.columns
            .iter()
            .map(|c| Ok((c.name.clone(), c.parsed_unit()?)))
            .collect()
    }

    /// Evaluate every computed column, in declaration order.
    ///
    /// Returns the number of cells filled. Rows where an input is empty stay
    /// empty rather than picking up a zero.
    pub fn recompute(&mut self) -> std::result::Result<usize, String> {
        let unit_map = self.unit_map()?;
        let plan: Vec<(usize, expr::Expr)> = self
            .columns
            .iter()
            .enumerate()
            .filter_map(|(i, c)| c.expr.as_ref().map(|e| (i, e.clone())))
            .map(|(i, e)| expr::parse(&e).map(|p| (i, p)))
            .collect::<std::result::Result<_, _>>()?;

        let mut filled = 0;
        for (idx, parsed) in plan {
            let target = self.columns[idx].clone();
            let target_unit = target.parsed_unit()?;
            let produced = parsed.unit(&unit_map)?;
            if !produced.compatible(&target_unit) {
                return Err(format!(
                    "column '{}' is declared {} but its formula produces {}",
                    target.name,
                    target_unit.describe(),
                    produced.describe()
                ));
            }
            let convert = produced.factor_to(&target_unit).unwrap_or(1.0);

            for row in self.rows.iter_mut() {
                let mut env: HashMap<String, f64> = HashMap::new();
                for (i, c) in self.columns.iter().enumerate() {
                    if let Some(n) = numeric(&row[i]) {
                        env.insert(c.name.clone(), n);
                    }
                }
                row[idx] = match parsed.eval(&env, &unit_map) {
                    Some(v) => {
                        filled += 1;
                        let v = v * convert;
                        match target.ty {
                            ColType::Int => Value::Int(v.round() as i64),
                            _ => Value::Float(v),
                        }
                    }
                    None => Value::Null,
                };
            }
        }
        Ok(filled)
    }

    /// Serialise into `DATA` sub-chunks: one `COLS`, then row groups.
    pub fn encode(&self) -> Result<ChunkList> {
        let mut data = ChunkList::new();
        data.push(Chunk::new(COLS, serde_json::to_vec(&self.columns)?));
        for group in self.rows.chunks(ROWS_PER_GROUP) {
            let mut v = Vec::new();
            v.extend_from_slice(&(group.len() as u32).to_le_bytes());
            v.extend_from_slice(&(self.columns.len() as u32).to_le_bytes());
            // Column-major: like values sit together, which is what lets the
            // chunk's own zstd frame find the redundancy in a column of
            // timestamps or repeated categories.
            for c in 0..self.columns.len() {
                for row in group {
                    row.get(c).unwrap_or(&Value::Null).encode_into(&mut v);
                }
            }
            data.push(Chunk::new(RGRP, v));
        }
        Ok(data)
    }

    /// Read a table back out of `DATA`.
    pub fn decode(data: &ChunkList) -> Result<Table> {
        let cols_chunk = data.require(COLS, "COLS")?;
        let columns: Vec<Column> = serde_json::from_slice(&cols_chunk.value)?;
        let ncols = columns.len();
        let mut rows = Vec::new();

        for g in data.all(RGRP) {
            if g.value.len() < 8 {
                return Err(Error::Truncated("RGRP header"));
            }
            let nrows = u32::from_le_bytes(g.value[0..4].try_into().unwrap()) as usize;
            let stored_cols = u32::from_le_bytes(g.value[4..8].try_into().unwrap()) as usize;
            if stored_cols != ncols {
                return Err(Error::Other(format!(
                    "row group declares {stored_cols} columns but COLS declares {ncols}"
                )));
            }
            let mut cells = vec![vec![Value::Null; ncols]; nrows];
            let mut i = 8usize;
            for c in 0..ncols {
                for row in cells.iter_mut() {
                    let (v, used) = decode_value_at(&g.value, i)?;
                    row[c] = v;
                    i = used;
                }
            }
            rows.extend(cells);
        }
        Ok(Table { columns, rows })
    }
}

/// `Value::decode` requires the slice to end exactly at the value, so the
/// column-major stream needs a variant that reports where it stopped.
fn decode_value_at(b: &[u8], at: usize) -> Result<(Value, usize)> {
    // Values are self-delimiting, so the cheapest correct approach is to try
    // successively longer slices only where necessary; instead, decode by
    // re-encoding length awareness into the walk below.
    let mut end = at;
    let v = scan_value(b, &mut end)?;
    Ok((v, end))
}

/// Walk one encoded value, advancing `i` past it.
fn scan_value(b: &[u8], i: &mut usize) -> Result<Value> {
    let need = |i: usize, n: usize| -> Result<()> {
        if i + n > b.len() {
            Err(Error::Truncated("row group cell"))
        } else {
            Ok(())
        }
    };
    need(*i, 1)?;
    let tag = b[*i];
    let start = *i;
    *i += 1;
    let len = match tag {
        0 => 0,
        1 => 1,
        2 | 3 => 8,
        4 | 5 => {
            need(*i, 4)?;
            let n = u32::from_le_bytes(b[*i..*i + 4].try_into().unwrap()) as usize;
            *i += 4;
            n
        }
        6 | 7 => {
            // Nested containers do not appear in a table cell; refusing them
            // here keeps the scan a straight walk with no recursion.
            return Err(Error::Other(
                "a table cell cannot hold a list or map".into(),
            ));
        }
        other => return Err(Error::Other(format!("unknown cell tag {other}"))),
    };
    need(*i, len)?;
    *i += len;
    Value::decode(&b[start..*i])
}

fn numeric(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

pub fn table(file: &WickFile) -> Result<Table> {
    Table::decode(&file.data()?)
}

// ---------------------------------------------------------------------------
// CSV
// ---------------------------------------------------------------------------

/// Split a CSV header into a name, a unit and an optional formula.
///
/// `distance (m)`, `distance [m]`, `speed (m/s) = distance / elapsed`.
/// Recognising the formula here is what makes a computed column expressible
/// in a plain CSV: the declaration survives a round trip through a
/// spreadsheet, which is the only way anyone will actually adopt it.
fn split_header(h: &str) -> (String, String, Option<String>) {
    let h = h.trim();
    let (head, expr) = match h.split_once('=') {
        Some((head, e)) if !e.trim().is_empty() => (head.trim(), Some(e.trim().to_string())),
        _ => (h, None),
    };
    for (open, close) in [('(', ')'), ('[', ']')] {
        if let Some(rest) = head.strip_suffix(close) {
            if let Some(at) = rest.rfind(open) {
                let name = rest[..at].trim();
                let unit = rest[at + 1..].trim();
                if !name.is_empty() && !unit.is_empty() {
                    return (name.to_string(), unit.to_string(), expr);
                }
            }
        }
    }
    (head.to_string(), String::new(), expr)
}

/// Decide a column's type from every value in it.
///
/// Strictly: one unparseable cell makes the column text. Numbers that lose
/// information when parsed — a leading zero, a leading `+` — are text too,
/// because `007` becoming `7` is the exact CSV failure this format exists to
/// stop. Empty cells vote for nothing.
fn infer_type(values: &[&str]) -> ColType {
    let mut seen = false;
    let mut int = true;
    let mut float = true;
    let mut boolean = true;

    for raw in values {
        let v = raw.trim();
        if v.is_empty() {
            continue;
        }
        seen = true;
        if !matches!(v.to_ascii_lowercase().as_str(), "true" | "false") {
            boolean = false;
        }
        // A number whose text form is not what parsing it back would
        // produce is an identifier wearing digits: a zip code, a part
        // number, `007`. Parsing it destroys information, so it is text.
        if !round_trips_as_number(v) {
            int = false;
            float = false;
            continue;
        }
        match v.parse::<i64>() {
            Ok(n) if n.to_string() == v => {}
            _ => int = false,
        }
        match v.parse::<f64>() {
            Ok(f) if f.is_finite() => {}
            _ => float = false,
        }
    }
    if !seen {
        return ColType::Str;
    }
    if boolean {
        ColType::Bool
    } else if int {
        ColType::Int
    } else if float {
        ColType::Float
    } else {
        ColType::Str
    }
}

/// Reject the spellings where parsing loses something: a leading zero, an
/// explicit plus, a trailing dot. These are the CSV corruptions people
/// actually hit, and they are cheap to refuse.
fn round_trips_as_number(v: &str) -> bool {
    let body = v.strip_prefix('-').unwrap_or(v);
    if v.starts_with('+') || body.starts_with('.') || body.ends_with('.') {
        return false;
    }
    let mut chars = body.chars();
    !matches!((chars.next(), chars.next()), (Some('0'), Some(c)) if c.is_ascii_digit())
}

fn parse_cell(raw: &str, ty: ColType) -> Value {
    let v = raw.trim();
    if v.is_empty() {
        return Value::Null;
    }
    match ty {
        ColType::Int => v.parse().map(Value::Int).unwrap_or(Value::Null),
        ColType::Float => v.parse().map(Value::Float).unwrap_or(Value::Null),
        ColType::Bool => Value::Bool(v.eq_ignore_ascii_case("true")),
        ColType::Str => Value::Str(v.to_string()),
    }
}

pub fn from_csv(src: &[u8]) -> Result<Table> {
    let mut rdr = csv::ReaderBuilder::new().flexible(true).from_reader(src);
    let headers = rdr
        .headers()
        .map_err(|e| Error::Other(format!("could not read the CSV header: {e}")))?
        .clone();

    let records: Vec<csv::StringRecord> = rdr
        .records()
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| Error::Other(format!("malformed CSV: {e}")))?;

    let columns: Vec<Column> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let (name, unit, expr) = split_header(h);
            let col: Vec<&str> = records.iter().map(|r| r.get(i).unwrap_or("")).collect();
            // A computed column is numeric even when it arrives empty: its
            // type comes from the formula's role, not from a column of
            // blanks that would otherwise be inferred as text.
            let ty = match (&expr, infer_type(&col)) {
                (Some(_), ColType::Str) => ColType::Float,
                (_, t) => t,
            };
            Column {
                name: if name.is_empty() {
                    format!("column_{}", i + 1)
                } else {
                    name
                },
                ty,
                unit,
                expr,
                doc: None,
            }
        })
        .collect();

    let rows = records
        .iter()
        .map(|r| {
            columns
                .iter()
                .enumerate()
                .map(|(i, c)| parse_cell(r.get(i).unwrap_or(""), c.ty))
                .collect()
        })
        .collect();

    Ok(Table { columns, rows })
}

pub fn to_csv(t: &Table) -> Result<Vec<u8>> {
    let mut w = csv::Writer::from_writer(Vec::new());
    w.write_record(t.columns.iter().map(|c| c.header()))
        .map_err(|e| Error::Other(format!("could not write CSV: {e}")))?;
    for row in &t.rows {
        w.write_record(row.iter().map(cell_text))
            .map_err(|e| Error::Other(format!("could not write CSV: {e}")))?;
    }
    w.into_inner()
        .map_err(|e| Error::Other(format!("could not write CSV: {e}")))
}

/// Same as [`cell_text`] but with floats trimmed to something a person can
/// read. Used only for display: an export has to keep every digit, because
/// a round trip that loses precision is a round trip that loses data.
fn cell_display(v: &Value) -> String {
    match v {
        Value::Float(f) if f.fract() != 0.0 => {
            let s = format!("{f:.6}");
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        }
        other => cell_text(other),
    }
}

fn cell_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => format_float(*f),
        Value::Bool(b) => b.to_string(),
        Value::Str(s) => s.clone(),
        other => other.preview(),
    }
}

pub fn to_json(t: &Table) -> Result<Vec<u8>> {
    let rows: Vec<serde_json::Value> = t
        .rows
        .iter()
        .map(|row| {
            let mut o = serde_json::Map::new();
            for (i, c) in t.columns.iter().enumerate() {
                o.insert(
                    c.name.clone(),
                    match row.get(i).unwrap_or(&Value::Null) {
                        Value::Null => serde_json::Value::Null,
                        Value::Int(n) => (*n).into(),
                        Value::Float(f) => serde_json::Number::from_f64(*f)
                            .map(serde_json::Value::Number)
                            .unwrap_or(serde_json::Value::Null),
                        Value::Bool(b) => (*b).into(),
                        Value::Str(s) => s.clone().into(),
                        other => other.preview().into(),
                    },
                );
            }
            serde_json::Value::Object(o)
        })
        .collect();

    // The units go with it. A bare array of objects would throw away the
    // declaration that makes this format worth using.
    let doc = serde_json::json!({
        "columns": t.columns,
        "rows": rows,
    });
    Ok(serde_json::to_vec_pretty(&doc)?)
}

/// Read back what [`to_json`] wrote.
///
/// The half that was missing: `.emx` could export JSON but not take it back,
/// so `hearth edit --to json` had no way home and anything holding a table as
/// JSON had to go out through CSV and be re-inferred.
///
/// The difference from [`from_csv`] is the whole reason this exists — **the
/// columns are read, not guessed.** A CSV import decides that a column of
/// `01, 02, 03` is text or numbers by looking at it; here the file says which
/// it is, so a round trip cannot change a column's type behind you. That also
/// means a cell whose JSON type contradicts its column is an error rather
/// than a coercion: a `"4.2"` in an `int` column is a mistake somebody wants
/// to hear about, not a `4`.
pub fn from_json(bytes: &[u8]) -> Result<Table> {
    let doc: serde_json::Value = serde_json::from_slice(bytes)?;
    let obj = doc.as_object().ok_or_else(|| {
        Error::Other("a .emx JSON document is an object with \"columns\" and \"rows\"".into())
    })?;

    let columns: Vec<Column> = match obj.get("columns") {
        Some(v) => serde_json::from_value(v.clone())?,
        None => {
            return Err(Error::Other(
                "this JSON has no \"columns\": a table without its column \
                 declarations is a CSV with extra punctuation"
                    .into(),
            ))
        }
    };
    if columns.is_empty() {
        return Err(Error::Other("\"columns\" is empty".into()));
    }

    let empty = Vec::new();
    let rows_json = match obj.get("rows") {
        Some(serde_json::Value::Array(a)) => a,
        // No rows at all is a legitimate table: `hearth create` makes one.
        None | Some(serde_json::Value::Null) => &empty,
        Some(_) => return Err(Error::Other("\"rows\" is not an array".into())),
    };

    let mut rows = Vec::with_capacity(rows_json.len());
    for (n, r) in rows_json.iter().enumerate() {
        let o = r
            .as_object()
            .ok_or_else(|| Error::Other(format!("row {n} is not an object")))?;
        // A key that names no column is refused rather than dropped. Silently
        // discarding `dsitance` is exactly the failure this format exists to
        // stop, and it looks identical to a column of empty cells.
        for k in o.keys() {
            if !columns.iter().any(|c| &c.name == k) {
                return Err(Error::Other(format!(
                    "row {n} has a value for '{k}', which is not a column of this table"
                )));
            }
        }
        let mut cells = Vec::with_capacity(columns.len());
        for c in &columns {
            // An absent key is an empty cell, which is a real value here and
            // not the same as a zero or an empty string.
            let v = match o.get(&c.name) {
                None => Value::Null,
                Some(j) => cell_from_json(j, c, n)?,
            };
            cells.push(v);
        }
        rows.push(cells);
    }

    Ok(Table { columns, rows })
}

fn cell_from_json(j: &serde_json::Value, c: &Column, row: usize) -> Result<Value> {
    let refuse = |saw: &str| {
        Err(Error::Other(format!(
            "row {row}, column '{}' is declared {} but holds {saw}",
            c.name,
            c.ty.name()
        )))
    };
    match j {
        serde_json::Value::Null => Ok(Value::Null),
        serde_json::Value::Bool(b) => match c.ty {
            ColType::Bool => Ok(Value::Bool(*b)),
            _ => refuse("a boolean"),
        },
        serde_json::Value::String(s) => match c.ty {
            ColType::Str => Ok(Value::Str(s.clone())),
            _ => refuse(&format!("the string {s:?}")),
        },
        serde_json::Value::Number(n) => match c.ty {
            // A whole number is a legal float. The reverse needs a rounding
            // decision the document has not made, so it is refused.
            ColType::Float => n
                .as_f64()
                .map(Value::Float)
                .ok_or_else(|| Error::Other(format!("row {row}: {n} is not a finite number"))),
            ColType::Int => match n.as_i64() {
                Some(i) => Ok(Value::Int(i)),
                None => refuse(&format!("{n}, which is not a whole number")),
            },
            _ => refuse("a number"),
        },
        // Same refusal the row-group decoder makes, for the same reason: a
        // cell is one value, and a format that nested here would need a
        // second answer for what a column's type means.
        _ => Err(Error::Other(format!(
            "row {row}, column '{}': a table cell cannot hold a list or map",
            c.name
        ))),
    }
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

pub struct Emx;

impl Plugin for Emx {
    fn tag(&self) -> Tag {
        TAG
    }
    fn ext(&self) -> &'static str {
        "emx"
    }
    fn name(&self) -> &'static str {
        "table"
    }
    fn about(&self) -> &'static str {
        "typed, unit-aware tabular data whose arithmetic is checked (replaces .csv)"
    }
    fn imports(&self) -> &'static [&'static str] {
        &["csv", "tsv", "json"]
    }
    fn exports(&self) -> &'static [&'static str] {
        &["csv", "json"]
    }
    fn schema_version(&self) -> u32 {
        SCHEMA_VERSION
    }

    fn import(&self, src: &Source) -> Result<Payload> {
        let t = if src.ext == "json" {
            from_json(src.bytes)?
        } else {
            let bytes: Vec<u8> = if src.ext == "tsv" {
                // csv::Reader handles the delimiter, but normalising here
                // keeps one code path for inference and one for writing.
                src.bytes
                    .iter()
                    .map(|b| if *b == b'\t' { b',' } else { *b })
                    .collect()
            } else {
                src.bytes.to_vec()
            };
            from_csv(&bytes)?
        };

        let mut schema = Schema::new("table");
        schema.version = SCHEMA_VERSION;
        schema.fields = t
            .columns
            .iter()
            .map(|c| {
                let mut f = FieldRule::new(&c.name, c.ty.name());
                if !c.unit.is_empty() {
                    f = f.with_unit(&c.unit);
                }
                f
            })
            .collect();

        Ok(Payload {
            summary: Some(summarize(&t)),
            data: t.encode()?,
            schema: Some(schema),
            caps: None,
            migrations: None,
        })
    }

    /// A table with columns and no rows.
    ///
    /// The column list is written in the same `name (unit) = formula` syntax
    /// a CSV export uses, so there is one header dialect to learn rather than
    /// two. Columns are required: a table with none is not an empty table,
    /// it is a file with nothing to say, and inventing a `column_1` would put
    /// a fabrication into the schema.
    fn starter(&self, spec: &Starter) -> Result<(&'static str, Vec<u8>)> {
        spec.only("emx", &["columns"])?;
        let Some(cols) = spec.columns else {
            return Err(Error::Other(
                "a new .emx needs its columns: --columns \"station, distance (m), \
                 speed (m/s) = distance / elapsed\""
                    .into(),
            ));
        };
        let names: Vec<&str> = cols.split(',').map(|c| c.trim()).collect();
        if let Some(i) = names.iter().position(|n| n.is_empty()) {
            return Err(Error::Other(format!(
                "column {} of --columns is empty",
                i + 1
            )));
        }
        // Round-trip the header through the CSV writer so that a column name
        // containing a comma or a quote is escaped the way the reader on the
        // other side expects.
        let mut w = csv::Writer::from_writer(Vec::new());
        w.write_record(&names)
            .map_err(|e| Error::Other(format!("could not write the column header: {e}")))?;
        let bytes = w
            .into_inner()
            .map_err(|e| Error::Other(format!("could not write the column header: {e}")))?;
        Ok(("csv", bytes))
    }

    fn export(&self, file: &WickFile, to: &str) -> Result<Vec<u8>> {
        let t = table(file)?;
        match to {
            "csv" => to_csv(&t),
            "json" => to_json(&t),
            other => Err(Error::Other(format!(".emx cannot export to .{other}"))),
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
        let t = table(file)?;
        let limit = opts.limit.unwrap_or(DEFAULT_ROWS);

        // Width each column needs, over the rows actually shown.
        let widths: Vec<usize> = t
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| {
                t.rows
                    .iter()
                    .take(limit)
                    .map(|r| {
                        cell_display(r.get(i).unwrap_or(&Value::Null))
                            .chars()
                            .count()
                    })
                    .chain(std::iter::once(c.header().chars().count()))
                    .max()
                    .unwrap_or(1)
                    .min(28)
            })
            .collect();

        let header: Vec<String> = t
            .columns
            .iter()
            .zip(&widths)
            .map(|(c, w)| format!("{:<w$}", c.header(), w = w))
            .collect();
        writeln!(out, "\x1b[1m{}\x1b[0m", header.join("  "))?;
        writeln!(
            out,
            "\x1b[2m{}\x1b[0m",
            widths
                .iter()
                .map(|w| "─".repeat(*w))
                .collect::<Vec<_>>()
                .join("  ")
        )?;

        for row in t.rows.iter().take(limit) {
            let cells: Vec<String> = row
                .iter()
                .zip(&widths)
                .map(|(v, w)| {
                    let s = cell_display(v);
                    let s: String = s.chars().take(*w).collect();
                    format!("{s:<w$}", w = w)
                })
                .collect();
            writeln!(out, "{}", cells.join("  "))?;
        }
        // How many rows the *file* holds, which after a partial read is not
        // how many this table holds. The summary tier records it; without one
        // there is no cheap way to know, and "more" is then the most that can
        // be said without counting rows nobody asked to see.
        let total = if file.is_partial() {
            summary_rows(file)
        } else {
            Some(t.rows.len())
        };
        match total {
            Some(n) if n > limit => writeln!(out, "\x1b[2m… {} more rows\x1b[0m", n - limit)?,
            None if t.rows.len() > limit => writeln!(out, "\x1b[2m… more rows\x1b[0m")?,
            _ => {}
        }
        Ok(())
    }

    fn enough(&self, taken: &ChunkList, opts: &RenderOpts) -> Enough {
        // `--summary` renders from `SUMM` and never looks at `DATA`, so
        // nothing needs reading at all.
        if opts.summary {
            return Enough::Yes;
        }
        // Rows mean nothing without their column names, and `COLS` is written
        // first, so this only ever waits on the very first child.
        if taken.get(COLS).is_none() {
            return Enough::More;
        }
        // One row past the limit, so the renderer can tell a table of exactly
        // `limit` rows from the first `limit` of more.
        let limit = opts.limit.unwrap_or(DEFAULT_ROWS);
        if taken.all(RGRP).map(group_rows).sum::<usize>() > limit {
            Enough::Yes
        } else {
            Enough::More
        }
    }

    fn validate(&self, file: &WickFile) -> Result<Vec<Issue>> {
        let t = table(file)?;
        let mut issues = Vec::new();

        let mut seen = std::collections::HashSet::new();
        for c in &t.columns {
            if c.name.trim().is_empty() {
                issues.push(Issue::error("", "a column has no name"));
            }
            if !seen.insert(c.name.clone()) {
                issues.push(Issue::error(&c.name, "column name is used more than once"));
            }
            if let Err(e) = c.parsed_unit() {
                issues.push(Issue::error(&c.name, e));
            }
        }

        // The headline check: every formula's units, resolved from the
        // schema alone. No rows are read to get this answer.
        match t.unit_map() {
            Err(e) => issues.push(Issue::error("", e)),
            Ok(unit_map) => {
                for c in t.columns.iter().filter(|c| c.expr.is_some()) {
                    let src = c.expr.as_deref().unwrap_or("");
                    let parsed = match expr::parse(src) {
                        Ok(p) => p,
                        Err(e) => {
                            issues.push(Issue::error(&c.name, format!("formula {src:?}: {e}")));
                            continue;
                        }
                    };
                    for referenced in parsed.columns() {
                        if t.column(&referenced).is_none() {
                            issues.push(Issue::error(
                                &c.name,
                                format!("formula refers to '{referenced}', which is not a column"),
                            ));
                        }
                    }
                    match (parsed.unit(&unit_map), c.parsed_unit()) {
                        (Err(e), _) => {
                            issues.push(Issue::error(&c.name, format!("formula {src:?}: {e}")))
                        }
                        (Ok(produced), Ok(declared)) if !produced.compatible(&declared) => {
                            issues.push(Issue::error(
                                &c.name,
                                format!(
                                    "declared {} but the formula {src:?} produces {}",
                                    declared.describe(),
                                    produced.describe()
                                ),
                            ));
                        }
                        _ => {}
                    }
                    if !c.ty.is_numeric() {
                        issues.push(Issue::error(
                            &c.name,
                            format!("a computed column must be numeric, not {}", c.ty.name()),
                        ));
                    }
                }
            }
        }

        // Cell types. Reported once per column, because a mistyped column
        // produces one problem, not one per row.
        for (i, c) in t.columns.iter().enumerate() {
            if let Some(bad) = t
                .rows
                .iter()
                .position(|r| !c.ty.accepts(r.get(i).unwrap_or(&Value::Null)))
            {
                issues.push(Issue::error(
                    &c.name,
                    format!(
                        "declared {} but row {bad} holds {}",
                        c.ty.name(),
                        t.rows[bad][i].type_name()
                    ),
                ));
            }
        }
        Ok(issues)
    }

    fn diff(&self, a: &WickFile, b: &WickFile, _keys: &KeyRing) -> Result<Vec<Change>> {
        let (ta, tb) = (table(a)?, table(b)?);
        let mut out = Vec::new();

        // Columns first: a schema change explains every cell difference under
        // it, so reporting it first stops the output being a wall of noise.
        for ca in &ta.columns {
            match tb.column(&ca.name) {
                None => out.push(Change::new(
                    ChangeKind::Removed,
                    format!("column {}", ca.name),
                    COLS,
                    format!("was {} {}", ca.ty.name(), ca.unit),
                )),
                Some((_, cb)) => {
                    if ca.ty != cb.ty {
                        out.push(Change::new(
                            ChangeKind::Modified,
                            format!("column {}", ca.name),
                            COLS,
                            format!("type {} -> {}", ca.ty.name(), cb.ty.name()),
                        ));
                    }
                    if ca.unit != cb.unit {
                        out.push(Change::new(
                            ChangeKind::Modified,
                            format!("column {}", ca.name),
                            COLS,
                            format!("unit {:?} -> {:?}", ca.unit, cb.unit),
                        ));
                    }
                    if ca.expr != cb.expr {
                        out.push(Change::new(
                            ChangeKind::Modified,
                            format!("column {}", ca.name),
                            COLS,
                            format!("formula {:?} -> {:?}", ca.expr, cb.expr),
                        ));
                    }
                }
            }
        }
        for cb in &tb.columns {
            if ta.column(&cb.name).is_none() {
                out.push(Change::new(
                    ChangeKind::Added,
                    format!("column {}", cb.name),
                    COLS,
                    format!("{} {}", cb.ty.name(), cb.unit),
                ));
            }
        }

        // Then cells, by row index and column name.
        const MAX_CELLS: usize = 200;
        let mut reported = 0;
        for r in 0..ta.rows.len().min(tb.rows.len()) {
            for (i, ca) in ta.columns.iter().enumerate() {
                let Some((j, _)) = tb.column(&ca.name) else {
                    continue;
                };
                let (old, new) = (&ta.rows[r][i], &tb.rows[r][j]);
                if old == new {
                    continue;
                }
                if reported == MAX_CELLS {
                    out.push(Change::new(
                        ChangeKind::Modified,
                        "…",
                        RGRP,
                        "further cell changes not listed",
                    ));
                    reported += 1;
                }
                if reported > MAX_CELLS {
                    continue;
                }
                reported += 1;
                out.push(Change::new(
                    ChangeKind::Modified,
                    format!("row {r} · {}", ca.name),
                    RGRP,
                    format!("{} -> {}", cell_display(old), cell_display(new)),
                ));
            }
        }
        if tb.rows.len() > ta.rows.len() {
            out.push(Change::new(
                ChangeKind::Added,
                format!("rows {}..{}", ta.rows.len(), tb.rows.len()),
                RGRP,
                format!("{} rows appended", tb.rows.len() - ta.rows.len()),
            ));
        } else if ta.rows.len() > tb.rows.len() {
            out.push(Change::new(
                ChangeKind::Removed,
                format!("rows {}..{}", tb.rows.len(), ta.rows.len()),
                RGRP,
                format!("{} rows removed", ta.rows.len() - tb.rows.len()),
            ));
        }
        Ok(out)
    }

    fn migrate_op(
        &self,
        op: &libwick::migrate::Op,
        data: &mut ChunkList,
    ) -> Result<Option<String>> {
        let mut columns: Vec<Column> = match data.get(COLS) {
            Some(c) => serde_json::from_slice(&c.value)?,
            None => return Ok(None),
        };
        let line = match op.op.as_str() {
            "rename_column" => {
                let from = op.str_arg("from")?;
                let to = op.str_arg("to")?;
                match columns.iter_mut().find(|c| c.name == from) {
                    Some(c) => {
                        c.name = to.to_string();
                        format!("renamed column '{from}' to '{to}'")
                    }
                    None => format!("no column '{from}' to rename"),
                }
            }
            "set_unit" => {
                let name = op.str_arg("column")?;
                let unit = op.str_arg("unit")?;
                Unit::parse(unit).map_err(Error::Other)?;
                match columns.iter_mut().find(|c| c.name == name) {
                    Some(c) => {
                        let was = std::mem::replace(&mut c.unit, unit.to_string());
                        format!("column '{name}' unit {was:?} -> {unit:?}")
                    }
                    None => format!("no column '{name}' to give a unit"),
                }
            }
            "set_formula" => {
                let name = op.str_arg("column")?;
                let formula = op.str_arg("formula")?;
                expr::parse(formula).map_err(Error::Other)?;
                match columns.iter_mut().find(|c| c.name == name) {
                    Some(c) => {
                        c.expr = Some(formula.to_string());
                        format!("column '{name}' is now computed as {formula:?}")
                    }
                    None => format!("no column '{name}' to give a formula"),
                }
            }
            _ => return Ok(None),
        };
        data.set(Chunk::new(COLS, serde_json::to_vec(&columns)?));
        Ok(Some(line))
    }

    fn summarize(&self, data: &ChunkList) -> Result<Option<ChunkList>> {
        Ok(Some(summarize(&Table::decode(data)?)))
    }
}

/// Schema, row count and a handful of sample rows — enough to know what a
/// table is without reading a million rows of it.
fn summarize(t: &Table) -> ChunkList {
    let mut summ = ChunkList::new();
    let stat = serde_json::json!({
        "rows": t.rows.len(),
        "columns": t.columns.len(),
        "row_groups": t.rows.len().div_ceil(ROWS_PER_GROUP),
        "schema": t.columns.iter().map(|c| serde_json::json!({
            "name": c.name,
            "type": c.ty.name(),
            "unit": c.unit,
            "computed": c.expr,
        })).collect::<Vec<_>>(),
    });
    summ.push(Chunk::new(
        STAT,
        serde_json::to_vec(&stat).unwrap_or_default(),
    ));

    for row in t.rows.iter().take(5) {
        let cells: Vec<String> = row.iter().map(cell_display).collect();
        summ.push(Chunk::text(HEAD, &cells.join("\t")));
    }
    summ
}

/// Rows a group holds, from its header alone. No cell is decoded, which is
/// what lets a partial read stop at the right group rather than at a guess
/// about how many rows a group is worth.
fn group_rows(c: &Chunk) -> usize {
    match c.value.get(0..4) {
        Some(b) => u32::from_le_bytes(b.try_into().unwrap()) as usize,
        None => 0,
    }
}

/// The row count the summary tier records, if the file has one.
fn summary_rows(file: &WickFile) -> Option<usize> {
    let summ = file.summary().ok().flatten()?;
    let stat = summ.get(STAT)?.as_json().ok()?;
    stat["rows"].as_u64().map(|n| n as usize)
}

fn render_summary(file: &WickFile, out: &mut dyn std::io::Write) -> Result<()> {
    let Some(summ) = file.summary()? else {
        return Err(Error::MissingChunk("SUMM"));
    };
    if let Some(stat) = summ.get(STAT) {
        let v: serde_json::Value = stat.as_json()?;
        writeln!(out, "{} rows × {} columns", v["rows"], v["columns"])?;
        if let Some(cols) = v["schema"].as_array() {
            for c in cols {
                let unit = c["unit"].as_str().unwrap_or("");
                let computed = c["computed"]
                    .as_str()
                    .map(|e| format!("  = {e}"))
                    .unwrap_or_default();
                writeln!(
                    out,
                    "  {:<20} {:<8} {unit}{computed}",
                    c["name"].as_str().unwrap_or(""),
                    c["type"].as_str().unwrap_or("")
                )?;
            }
        }
    }
    let sample: Vec<_> = summ.all(HEAD).collect();
    if !sample.is_empty() {
        writeln!(out, "\nfirst rows:")?;
        for s in sample {
            writeln!(out, "  {}", s.as_str()?.replace('\t', "  "))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CSV: &str = "\
name,distance (m),elapsed (s),id,active
alpha,100,10,007,true
beta,250,25,008,false
gamma,,30,009,true
";

    fn build(src: &str) -> WickFile {
        let mut f = WickFile::new(TAG);
        let p = Emx
            .import(&Source::new(src.as_bytes(), "sample.csv", "csv"))
            .unwrap();
        f.set_data(&p.data).unwrap();
        if let Some(s) = p.schema {
            f.set_schema(&s).unwrap();
        }
        if let Some(s) = p.summary {
            f.set_summary(&s).unwrap();
        }
        f
    }

    #[test]
    fn json_round_trips_with_its_types_intact() {
        let t = from_csv(CSV.as_bytes()).unwrap();
        let json = to_json(&t).unwrap();
        let back = from_json(&json).unwrap();

        assert_eq!(back.columns.len(), t.columns.len());
        assert_eq!(back.rows, t.rows);
        // The unit declaration travels with it.
        assert_eq!(back.columns[1].unit, "m");
        // The identifier column keeps its leading zeros *and* its type, which
        // is the thing a CSV round trip has to re-infer and can get wrong.
        assert_eq!(back.columns[3].ty, ColType::Str);
        assert_eq!(back.rows[0][3], Value::Str("007".into()));
        // An empty cell is still empty, not zero.
        assert_eq!(back.rows[2][1], Value::Null);
    }

    #[test]
    fn a_computed_column_survives_the_json_round_trip() {
        let src = "distance (m),elapsed (s),speed (m/s) = distance / elapsed\n100,10,\n250,25,\n";
        let t = from_csv(src.as_bytes()).unwrap();
        let mut back = from_json(&to_json(&t).unwrap()).unwrap();
        assert_eq!(back.columns[2].expr.as_deref(), Some("distance / elapsed"));
        assert_eq!(back.columns[2].unit, "m/s");
        assert_eq!(back.recompute().unwrap(), 2);
        assert_eq!(back.rows[0][2], Value::Float(10.0));
    }

    #[test]
    fn json_reads_its_columns_rather_than_guessing_them() {
        // The same values a CSV import would read as numbers, declared as
        // strings. The declaration wins — that is the whole difference.
        let doc = br#"{"columns":[{"name":"id","type":"str"}],
                       "rows":[{"id":"007"},{"id":"008"}]}"#;
        let t = from_json(doc).unwrap();
        assert_eq!(t.columns[0].ty, ColType::Str);
        assert_eq!(t.rows[0][0], Value::Str("007".into()));
    }

    #[test]
    fn json_refuses_a_cell_that_contradicts_its_column() {
        let doc = br#"{"columns":[{"name":"port","type":"int"}],
                       "rows":[{"port":"8080"}]}"#;
        let e = from_json(doc).unwrap_err().to_string();
        assert!(e.contains("declared int"), "{e}");
        assert!(e.contains("row 0"), "{e}");

        // A fraction in an int column is a rounding decision the document
        // did not make.
        let doc = br#"{"columns":[{"name":"n","type":"int"}],"rows":[{"n":4.2}]}"#;
        assert!(from_json(doc).is_err());
    }

    #[test]
    fn json_refuses_a_value_for_a_column_that_does_not_exist() {
        // A misspelled key that was silently dropped would look exactly like
        // a column of empty cells.
        let doc = br#"{"columns":[{"name":"distance","type":"int"}],
                       "rows":[{"dsitance":100}]}"#;
        let e = from_json(doc).unwrap_err().to_string();
        assert!(e.contains("'dsitance'"), "{e}");
        assert!(e.contains("not a column"), "{e}");
    }

    #[test]
    fn json_without_columns_is_refused() {
        let e = from_json(br#"{"rows":[{"a":1}]}"#).unwrap_err().to_string();
        assert!(e.contains("no \"columns\""), "{e}");
    }

    #[test]
    fn a_table_with_no_rows_is_a_table() {
        let doc = br#"{"columns":[{"name":"station","type":"str"}]}"#;
        let t = from_json(doc).unwrap();
        assert_eq!(t.columns.len(), 1);
        assert!(t.rows.is_empty());
    }

    #[test]
    fn a_formula_can_be_declared_in_the_csv_header() {
        let src = "distance (m),elapsed (s),speed (m/s) = distance / elapsed\n100,10,\n250,25,\n";
        let mut t = from_csv(src.as_bytes()).unwrap();
        assert_eq!(t.columns[2].expr.as_deref(), Some("distance / elapsed"));
        assert_eq!(t.columns[2].ty, ColType::Float);

        assert_eq!(t.recompute().unwrap(), 2);
        assert_eq!(t.rows[0][2], Value::Float(10.0));

        // And the declaration survives being written back out.
        let out = String::from_utf8(to_csv(&t).unwrap()).unwrap();
        assert!(
            out.starts_with("distance (m),elapsed (s),speed (m/s) = distance / elapsed"),
            "{out}"
        );
        assert_eq!(
            from_csv(out.as_bytes()).unwrap().columns[2].expr.as_deref(),
            Some("distance / elapsed")
        );
    }

    #[test]
    fn types_and_units_are_read_from_the_header() {
        let t = from_csv(CSV.as_bytes()).unwrap();
        assert_eq!(t.columns[0].name, "name");
        assert_eq!(t.columns[0].ty, ColType::Str);
        assert_eq!(t.columns[1].name, "distance");
        assert_eq!(t.columns[1].unit, "m");
        assert_eq!(t.columns[1].ty, ColType::Int);
        assert_eq!(t.columns[4].ty, ColType::Bool);
    }

    #[test]
    fn leading_zeros_stay_text() {
        // The canonical CSV data loss: `007` must not become `7`.
        let t = from_csv(CSV.as_bytes()).unwrap();
        assert_eq!(t.columns[3].ty, ColType::Str);
        assert_eq!(t.rows[0][3], Value::Str("007".into()));
    }

    #[test]
    fn an_empty_cell_is_null_not_zero() {
        let t = from_csv(CSV.as_bytes()).unwrap();
        assert_eq!(t.rows[2][1], Value::Null);
    }

    #[test]
    fn csv_round_trips_through_the_chunk_tree() {
        let f = build(CSV);
        let out = String::from_utf8(Emx.export(&f, "csv").unwrap()).unwrap();
        assert_eq!(out, CSV);
    }

    #[test]
    fn row_groups_hold_the_whole_table() {
        let big: String = std::iter::once("n,v\n".to_string())
            .chain((0..1500).map(|i| format!("{i},{}\n", i * 2)))
            .collect();
        let f = build(&big);
        let t = table(&f).unwrap();
        assert_eq!(t.rows.len(), 1500);
        assert_eq!(f.data().unwrap().all(RGRP).count(), 3);
        assert_eq!(t.rows[1499][1], Value::Int(2998));
    }

    #[test]
    fn a_consistent_formula_validates_and_computes() {
        let mut t = from_csv(CSV.as_bytes()).unwrap();
        t.columns.push(
            Column::new("speed", ColType::Float)
                .with_unit("m/s")
                .computed("distance / elapsed"),
        );
        for row in t.rows.iter_mut() {
            row.push(Value::Null);
        }
        assert_eq!(t.recompute().unwrap(), 2); // the empty distance stays empty

        let mut f = WickFile::new(TAG);
        f.set_data(&t.encode().unwrap()).unwrap();
        assert!(Emx.validate(&f).unwrap().is_empty());
        assert_eq!(table(&f).unwrap().rows[0][5], Value::Float(10.0));
        assert_eq!(table(&f).unwrap().rows[2][5], Value::Null);
    }

    #[test]
    fn a_unit_mismatched_formula_fails_loudly() {
        let mut t = from_csv(CSV.as_bytes()).unwrap();
        t.columns.push(
            Column::new("nonsense", ColType::Float)
                .with_unit("m")
                .computed("distance + elapsed"),
        );
        for row in t.rows.iter_mut() {
            row.push(Value::Null);
        }
        let err = t.recompute().unwrap_err();
        assert!(err.contains("cannot add m and s"), "{err}");

        let mut f = WickFile::new(TAG);
        f.set_data(&t.encode().unwrap()).unwrap();
        let issues = Emx.validate(&f).unwrap();
        assert!(
            issues
                .iter()
                .any(|i| i.message.contains("cannot add m and s")),
            "{issues:?}"
        );
    }

    #[test]
    fn a_formula_producing_the_wrong_dimension_is_caught_without_any_rows() {
        let t = Table {
            columns: vec![
                Column::new("distance", ColType::Float).with_unit("m"),
                Column::new("elapsed", ColType::Float).with_unit("s"),
                // Says metres, computes metres per second.
                Column::new("speed", ColType::Float)
                    .with_unit("m")
                    .computed("distance / elapsed"),
            ],
            rows: Vec::new(),
        };
        let mut f = WickFile::new(TAG);
        f.set_data(&t.encode().unwrap()).unwrap();
        let issues = Emx.validate(&f).unwrap();
        assert!(
            issues
                .iter()
                .any(|i| i.message.contains("declared m but the formula")),
            "{issues:?}"
        );
    }

    #[test]
    fn mixed_scales_convert_rather_than_failing() {
        let mut t = Table {
            columns: vec![
                Column::new("long", ColType::Float).with_unit("km"),
                Column::new("short", ColType::Float).with_unit("m"),
                Column::new("total", ColType::Float)
                    .with_unit("m")
                    .computed("long + short"),
            ],
            rows: vec![vec![Value::Float(1.0), Value::Float(500.0), Value::Null]],
        };
        t.recompute().unwrap();
        // 1 km + 500 m, expressed in metres.
        assert_eq!(t.rows[0][2], Value::Float(1500.0));
    }

    #[test]
    fn a_cell_change_names_the_row_and_column() {
        let a = build(CSV);
        let b = build(&CSV.replace("beta,250", "beta,260"));
        let d = Emx.diff(&a, &b, &KeyRing::empty()).unwrap();
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].path, "row 1 · distance");
        assert_eq!(d[0].note, "250 -> 260");
    }

    #[test]
    fn a_unit_change_is_reported_before_the_cells() {
        let a = build(CSV);
        let b = build(&CSV.replace("distance (m)", "distance (km)"));
        let d = Emx.diff(&a, &b, &KeyRing::empty()).unwrap();
        assert_eq!(d[0].path, "column distance");
        assert!(d[0].note.contains("unit"), "{}", d[0].note);
    }

    #[test]
    fn appended_rows_are_summarised_not_listed() {
        let a = build(CSV);
        let b = build(&format!("{CSV}delta,400,40,010,true\n"));
        let d = Emx.diff(&a, &b, &KeyRing::empty()).unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, ChangeKind::Added);
        assert!(d[0].note.contains("1 rows appended"));
    }

    #[test]
    fn duplicate_columns_are_rejected() {
        let f = build("a,a\n1,2\n");
        let issues = Emx.validate(&f).unwrap();
        assert!(
            issues.iter().any(|i| i.message.contains("more than once")),
            "{issues:?}"
        );
    }

    #[test]
    fn migration_can_rename_a_column_and_set_a_unit() {
        use libwick::migrate::{Op, Rule, RuleSet};
        let f = build(CSV);
        let mut data = f.data().unwrap();
        let rules = RuleSet::new().with(Rule {
            from: 1,
            to: 2,
            note: None,
            ops: vec![
                Op::new(
                    "rename_column",
                    serde_json::json!({"from": "elapsed", "to": "duration"}),
                ),
                Op::new(
                    "set_unit",
                    serde_json::json!({"column": "duration", "unit": "ms"}),
                ),
            ],
        });
        libwick::migrate::apply(&rules, &mut data, 1, 2, &mut |op, d| Emx.migrate_op(op, d))
            .unwrap();

        let t = Table::decode(&data).unwrap();
        assert_eq!(t.columns[2].name, "duration");
        assert_eq!(t.columns[2].unit, "ms");
    }

    #[test]
    fn a_bad_unit_in_a_migration_is_refused() {
        use libwick::migrate::Op;
        let f = build(CSV);
        let mut data = f.data().unwrap();
        let op = Op::new(
            "set_unit",
            serde_json::json!({"column": "distance", "unit": "m^x"}),
        );
        assert!(Emx.migrate_op(&op, &mut data).is_err());
    }

    #[test]
    fn the_summary_describes_the_schema_without_the_rows() {
        let f = build(CSV);
        let mut buf = Vec::new();
        Emx.render(
            &f,
            &RenderOpts {
                summary: true,
                ..Default::default()
            },
            &mut buf,
        )
        .unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("3 rows × 5 columns"), "{s}");
        assert!(s.contains("distance"));
    }

    #[test]
    fn json_export_carries_the_units() {
        let f = build(CSV);
        let out = String::from_utf8(Emx.export(&f, "json").unwrap()).unwrap();
        assert!(out.contains("\"unit\": \"m\""), "{out}");
        assert!(out.contains("\"rows\""));
    }
}
