//! `.emc` — configuration that validates, scopes and compartmentalises
//! itself.
//!
//! Three things go wrong with `.json`, `.yaml` and `.toml` config, and none
//! of them are fixable inside those formats:
//!
//! 1. **The schema is somewhere else, or nowhere.** An external
//!    `config.schema.json` drifts from the config it describes the first time
//!    someone edits one without the other. `.emc` carries its rules in `SCHM`,
//!    covered by the same content hash as the data, so they cannot separate.
//!
//! 2. **A config file is trusted with whatever the program is trusted with.**
//!    Config decides what gets read, written and called out to, but declares
//!    none of it. `.emc` carries a `CAPS` declaration, and a runtime that
//!    honours it refuses to grant more than the file asked for. The file
//!    states its own blast radius.
//!
//! 3. **Secrets and settings cannot live together.** The usual answer is two
//!    files, one of them in a vault, and a deployment step that hopes they
//!    match. `.emc` seals individual chunks to individual key slots: public
//!    config in plaintext, production secrets under one passphrase, staging
//!    under another, one file. Holding the staging passphrase reveals the
//!    staging secrets and nothing else, and rewriting the file with only that
//!    passphrase leaves the production half byte-identical.
//!
//! ## Payload layout
//!
//! `DATA` is a flat, ordered list of `NODE` chunks — one per leaf value, each
//! carrying its full path. Flat rather than nested because that is what makes
//! the diff readable: comparing two lists of paths yields
//! `database.port: 5432 -> 5433`, where comparing two trees yields "the
//! database table changed" and leaves the reader to hunt. Sealed values live
//! in `SECR` chunks with the same inner layout.

pub mod convert;

use libwick::caps::Capabilities;
use libwick::chunks::{Chunk, ChunkList, ChunkType};
use libwick::error::{Error, Result};
use libwick::migrate::Op;
use libwick::plugin::{Payload, Plugin, RenderOpts, Source, Starter};
use libwick::schema::{collapse_indices, FieldRule, Issue, Schema};
use libwick::value::Value;
use libwick::{Change, ChangeKind, KeyRing, Tag, WickFile};

pub const TAG: Tag = Tag::new(b"MC");
pub const SCHEMA_VERSION: u32 = 1;

const NODE: ChunkType = ChunkType::new(b"NODE");
/// A sealed group of `NODE`s. Same layout, encrypted to one key slot.
const SECR: ChunkType = ChunkType::new(b"SECR");
const STAT: ChunkType = ChunkType::new(b"STAT");
const KEYL: ChunkType = ChunkType::new(b"KEYL");
/// Where each sealed node sat in `DATA`, so unsealing can put it back rather
/// than appending it. A little-endian `u32` per node, in group order. Config
/// key order is meaningful — it is what an export writes and what a diff
/// reads — so losing it would make unsealing a visible edit to a file that
/// nobody edited.
const SIDX: ChunkType = ChunkType::new(b"SIDX");

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// One step of a path. The distinction matters: rebuilding a tree from
/// `servers.0.host` has to know whether `0` addresses a list slot or a map
/// key literally spelled "0". Storing the answer removes the guess.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seg {
    Key(String),
    Index(u32),
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Path(pub Vec<Seg>);

impl Path {
    /// Dotted rendering, for humans and for schema lookups.
    pub fn dotted(&self) -> String {
        self.0
            .iter()
            .map(|s| match s {
                Seg::Key(k) => k.clone(),
                Seg::Index(i) => i.to_string(),
            })
            .collect::<Vec<_>>()
            .join(".")
    }

    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&(self.0.len() as u16).to_le_bytes());
        for seg in &self.0 {
            match seg {
                Seg::Key(k) => {
                    out.push(0);
                    out.extend_from_slice(&(k.len() as u16).to_le_bytes());
                    out.extend_from_slice(k.as_bytes());
                }
                Seg::Index(i) => {
                    out.push(1);
                    out.extend_from_slice(&i.to_le_bytes());
                }
            }
        }
    }

    fn decode(b: &[u8], i: &mut usize) -> Result<Path> {
        let need = |i: usize, n: usize| -> Result<()> {
            if i + n > b.len() {
                Err(Error::Truncated("NODE path"))
            } else {
                Ok(())
            }
        };
        need(*i, 2)?;
        let n = u16::from_le_bytes([b[*i], b[*i + 1]]) as usize;
        *i += 2;
        let mut segs = Vec::with_capacity(n);
        for _ in 0..n {
            need(*i, 1)?;
            let kind = b[*i];
            *i += 1;
            match kind {
                0 => {
                    need(*i, 2)?;
                    let len = u16::from_le_bytes([b[*i], b[*i + 1]]) as usize;
                    *i += 2;
                    need(*i, len)?;
                    let s = String::from_utf8(b[*i..*i + len].to_vec())
                        .map_err(|_| Error::Other("a .emc key is not valid UTF-8".into()))?;
                    *i += len;
                    segs.push(Seg::Key(s));
                }
                1 => {
                    need(*i, 4)?;
                    segs.push(Seg::Index(u32::from_le_bytes(
                        b[*i..*i + 4].try_into().unwrap(),
                    )));
                    *i += 4;
                }
                other => return Err(Error::Other(format!("unknown path segment kind {other}"))),
            }
        }
        Ok(Path(segs))
    }
}

/// One leaf value at one path.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub path: Path,
    pub value: Value,
}

impl Node {
    fn encode(&self) -> Chunk {
        let mut v = Vec::new();
        self.path.encode(&mut v);
        self.value.encode_into(&mut v);
        Chunk::new(NODE, v)
    }

    fn decode(c: &Chunk) -> Result<Node> {
        let mut i = 0usize;
        let path = Path::decode(&c.value, &mut i)?;
        let value = Value::decode(&c.value[i..])?;
        Ok(Node { path, value })
    }
}

/// Flatten a value into leaf nodes, keeping segment kinds.
pub fn flatten(v: &Value) -> Vec<Node> {
    let mut out = Vec::new();
    walk(v, &mut Vec::new(), &mut out);
    out
}

fn walk(v: &Value, prefix: &mut Vec<Seg>, out: &mut Vec<Node>) {
    match v {
        Value::Map(m) if !m.is_empty() => {
            for (k, child) in m {
                prefix.push(Seg::Key(k.clone()));
                walk(child, prefix, out);
                prefix.pop();
            }
        }
        Value::List(l) if !l.is_empty() => {
            for (i, child) in l.iter().enumerate() {
                prefix.push(Seg::Index(i as u32));
                walk(child, prefix, out);
                prefix.pop();
            }
        }
        // Empty containers are leaves. `plugins = []` is a statement, and
        // dropping it would turn it into an absent key on the way back.
        leaf => out.push(Node {
            path: Path(prefix.clone()),
            value: leaf.clone(),
        }),
    }
}

/// Rebuild a tree from leaf nodes. Insertion order decides key order, so a
/// round-trip through `DATA` preserves how the file was written.
pub fn unflatten(nodes: &[Node]) -> Result<Value> {
    if nodes.is_empty() {
        return Ok(Value::Map(Vec::new()));
    }
    // A single node with an empty path is the whole document: a scalar or an
    // empty container at the top level.
    if nodes.len() == 1 && nodes[0].path.0.is_empty() {
        return Ok(nodes[0].value.clone());
    }

    let mut root = match nodes[0].path.0.first() {
        Some(Seg::Index(_)) => Value::List(Vec::new()),
        _ => Value::Map(Vec::new()),
    };
    for n in nodes {
        insert(&mut root, &n.path.0, n.value.clone())?;
    }
    Ok(root)
}

fn insert(node: &mut Value, segs: &[Seg], value: Value) -> Result<()> {
    let Some((head, rest)) = segs.split_first() else {
        *node = value;
        return Ok(());
    };
    // A container's kind is decided by the segment addressing into it, which
    // is why the segment kind is stored rather than inferred from the text.
    let child_seed = || match rest.first() {
        Some(Seg::Index(_)) => Value::List(Vec::new()),
        Some(Seg::Key(_)) => Value::Map(Vec::new()),
        None => Value::Null,
    };

    match (head, node) {
        (Seg::Key(k), Value::Map(m)) => {
            if !m.iter().any(|(existing, _)| existing == k) {
                m.push((k.clone(), child_seed()));
            }
            let slot = m.iter_mut().find(|(e, _)| e == k).map(|(_, v)| v).unwrap();
            insert(slot, rest, value)
        }
        (Seg::Index(i), Value::List(l)) => {
            let i = *i as usize;
            while l.len() <= i {
                l.push(child_seed());
            }
            insert(&mut l[i], rest, value)
        }
        (seg, other) => Err(Error::Other(format!(
            "config path is inconsistent: {seg:?} addresses into a {}",
            other.type_name()
        ))),
    }
}

// ---------------------------------------------------------------------------
// Reading a file
// ---------------------------------------------------------------------------

/// Nodes from `DATA` plus every `SECR` group that is unlocked.
///
/// Locked groups are skipped rather than erroring: reading the public half of
/// a split-trust config without holding any secret is the ordinary case.
pub fn nodes(file: &WickFile) -> Result<Vec<Node>> {
    let data = file.data()?;
    let mut out: Vec<Node> = data.all(NODE).map(Node::decode).collect::<Result<_>>()?;
    for group in file.chunks.all(SECR) {
        if group.is_locked() {
            continue;
        }
        let inner = group.as_list(&file.keys)?;
        for n in inner.all(NODE) {
            out.push(Node::decode(n)?);
        }
    }
    Ok(out)
}

/// The config as a tree, with whatever secrets are readable merged in.
pub fn config(file: &WickFile) -> Result<Value> {
    unflatten(&nodes(file)?)
}

/// Paths that exist but are sealed to a slot this reader cannot open.
pub fn locked_paths(file: &WickFile) -> Vec<(u8, String)> {
    file.chunks
        .all(SECR)
        .filter(|c| c.is_locked())
        .map(|c| (c.enc.slot, file.keys.label(c.enc.slot)))
        .collect()
}

/// Move the given paths out of `DATA` and into a `SECR` chunk sealed to
/// `slot`. Prefix matching, so `database` takes `database.password` with it.
///
/// An empty prefix list seals everything, which is what `--all` asks for.
pub fn seal_paths(file: &mut WickFile, slot: u8, prefixes: &[String]) -> Result<usize> {
    let data = file.data()?;
    let matches = |n: &Node| {
        if prefixes.is_empty() {
            return true;
        }
        let d = n.path.dotted();
        prefixes
            .iter()
            .any(|p| d == *p || d.starts_with(&format!("{p}.")))
    };

    let mut public = ChunkList::new();
    let mut secret = ChunkList::new();
    let mut where_from: Vec<u32> = Vec::new();
    for (i, c) in data.iter().enumerate() {
        if c.ty != NODE {
            public.push(c.clone());
            continue;
        }
        if matches(&Node::decode(c)?) {
            secret.push(c.clone());
            where_from.push(i as u32);
        } else {
            public.push(c.clone());
        }
    }
    let moved = secret.len();
    if moved == 0 {
        return Ok(0);
    }

    file.set_data(&public)?;
    // A sealed group carries an explicit key-label chunk so that a reader
    // without the passphrase can still say *what* it is missing, and an
    // index chunk so that unsealing restores the order rather than
    // approximating it.
    let mut group = ChunkList::new();
    group.push(Chunk::text(KEYL, &file.keys.label(slot)));
    group.push(Chunk::new(
        SIDX,
        where_from.iter().flat_map(|i| i.to_le_bytes()).collect(),
    ));
    for c in secret.iter() {
        group.push(c.clone());
    }
    file.chunks
        .push(Chunk::list(SECR, &group, &file.keys)?.sealed_to(slot));
    Ok(moved)
}

/// Read one value by dotted path, from the plaintext half plus whatever
/// sealed groups this reader has unlocked.
pub fn get_path(file: &WickFile, path: &str) -> Result<Option<Value>> {
    Ok(nodes(file)?
        .into_iter()
        .find(|n| n.path.dotted() == path)
        .map(|n| n.value))
}

/// Set one value by dotted path, adding it if it is not there.
///
/// Returns the value that was replaced, or `None` when the path is new. This
/// is the surgical edit: it touches one node and leaves every other chunk —
/// sealed groups included — exactly as it found them, which an export-edit-
/// import round trip cannot promise.
pub fn set_path(file: &mut WickFile, path: &str, value: Value) -> Result<Option<Value>> {
    let mut data = file.data()?;
    for c in data.0.iter_mut().filter(|c| c.ty == NODE) {
        let node = Node::decode(c)?;
        if node.path.dotted() == path {
            let was = node.value.clone();
            *c = Node {
                path: node.path,
                value,
            }
            .encode();
            file.set_data(&data)?;
            return Ok(Some(was));
        }
    }

    // New key. Segment kinds are guessed against a sibling path so that a
    // numeric segment only becomes a list index where one already exists.
    let like = data
        .all(NODE)
        .map(Node::decode)
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .map(|n| n.path)
        .find(|p| {
            let d = p.dotted();
            path.starts_with(&format!(
                "{}.",
                d.rsplit_once('.').map(|(h, _)| h).unwrap_or(&d)
            ))
        })
        .unwrap_or(Path(Vec::new()));
    data.push(
        Node {
            path: parse_dotted(path, &like),
            value,
        }
        .encode(),
    );
    file.set_data(&data)?;
    Ok(None)
}

/// Remove a value, or a whole subtree, by dotted path. Returns how many
/// leaves went.
pub fn unset_path(file: &mut WickFile, path: &str) -> Result<usize> {
    let data = file.data()?;
    let mut keep = ChunkList::new();
    let mut gone = 0;
    for c in data.iter() {
        if c.ty == NODE {
            let d = Node::decode(c)?.path.dotted();
            if d == path || d.starts_with(&format!("{path}.")) {
                gone += 1;
                continue;
            }
        }
        keep.push(c.clone());
    }
    if gone > 0 {
        file.set_data(&keep)?;
    }
    Ok(gone)
}

/// One value as JSON text.
pub fn value_json(v: &Value) -> Result<String> {
    convert::to_json(v)
}

/// One value as bare text: a string without its quotes, everything else as
/// JSON. This is what a shell wants — `$(hearth get x.emc db.host)` should
/// be `db.internal`, not `"db.internal"`.
pub fn value_text(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        // The JSON writer ends documents with a newline, which is right for a
        // file and wrong for a value about to be printed with one.
        other => convert::to_json(other)
            .map(|s| s.trim_end().to_string())
            .unwrap_or_else(|_| other.preview()),
    }
}

/// Parse a command-line value: JSON if it parses as JSON, a string if not.
///
/// `8080` is a number, `true` is a boolean, `null` is null, and `db.internal`
/// is a string — which is what somebody typing at a shell means every time.
/// `--string` on the caller's side forces the last reading.
pub fn parse_value(text: &str, force_string: bool) -> Value {
    if force_string {
        return Value::Str(text.to_string());
    }
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(v) => json_arg_to_value(&v),
        Err(_) => Value::Str(text.to_string()),
    }
}

/// The inverse: bring sealed values back into the plaintext payload.
///
/// Only groups this reader has actually unlocked are considered — a locked
/// group is passed over rather than reported as an error, exactly as every
/// other read of a split-trust file does. Returns how many values came back.
///
/// The key slot itself is left in place. A slot with nothing sealed to it is
/// harmless, and removing it would silently invalidate a passphrase somebody
/// else may still be holding for a different group.
pub fn unseal_paths(file: &mut WickFile, prefixes: &[String]) -> Result<usize> {
    let matches = |n: &Node| {
        if prefixes.is_empty() {
            return true;
        }
        let d = n.path.dotted();
        prefixes
            .iter()
            .any(|p| d == *p || d.starts_with(&format!("{p}.")))
    };

    let mut data = file.data()?;
    // (original index, chunk) for everything coming back.
    let mut returning: Vec<(u32, Chunk)> = Vec::new();
    let mut rebuilt: Vec<Chunk> = Vec::new();

    for group in file.chunks.all(SECR) {
        if group.is_locked() {
            rebuilt.push(group.clone());
            continue;
        }
        let inner = group.as_list(&file.keys)?;
        let indices: Vec<u32> = inner
            .get(SIDX)
            .map(|c| {
                c.value
                    .chunks_exact(4)
                    .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .collect()
            })
            .unwrap_or_default();

        let mut kept = ChunkList::new();
        let mut kept_indices: Vec<u32> = Vec::new();
        let mut seen = 0usize;
        for c in inner.iter() {
            if c.ty != NODE {
                continue; // KEYL and SIDX are rebuilt below
            }
            let at = indices.get(seen).copied().unwrap_or(u32::MAX);
            seen += 1;
            if matches(&Node::decode(c)?) {
                returning.push((at, c.clone()));
            } else {
                kept.push(c.clone());
                kept_indices.push(at);
            }
        }

        // A group that gave up everything disappears; one with values left
        // is written back with its own label and indices.
        if !kept.is_empty() {
            let slot = group.enc.slot;
            let mut g = ChunkList::new();
            g.push(Chunk::text(KEYL, &file.keys.label(slot)));
            g.push(Chunk::new(
                SIDX,
                kept_indices.iter().flat_map(|i| i.to_le_bytes()).collect(),
            ));
            for c in kept.iter() {
                g.push(c.clone());
            }
            rebuilt.push(Chunk::list(SECR, &g, &file.keys)?.sealed_to(slot));
        }
    }

    if returning.is_empty() {
        return Ok(0);
    }

    // Front to back, clamped to the list as it grows. Ascending order is
    // what makes the clamp correct: each value is placed after everything
    // that preceded it, so a payload that was emptied entirely comes back in
    // its original order rather than reversed by the clamp. An index from a
    // file written before SIDX existed is u32::MAX, which appends — the
    // honest answer when the position was never recorded.
    returning.sort_by_key(|(at, _)| *at);
    for (at, chunk) in &returning {
        let at = (*at as usize).min(data.0.len());
        data.0.insert(at, chunk.clone());
    }
    let count = returning.len();

    file.set_data(&data)?;
    while file.chunks.remove(SECR).is_some() {}
    for c in rebuilt {
        file.chunks.push(c);
    }
    Ok(count)
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

pub struct Emc;

impl Plugin for Emc {
    fn tag(&self) -> Tag {
        TAG
    }
    fn ext(&self) -> &'static str {
        "emc"
    }
    fn name(&self) -> &'static str {
        "config"
    }
    fn about(&self) -> &'static str {
        "self-validating, capability-scoped config with split-trust secrets (replaces .json/.yaml/.toml)"
    }
    fn imports(&self) -> &'static [&'static str] {
        &["json", "yaml", "yml", "toml"]
    }
    fn exports(&self) -> &'static [&'static str] {
        &["json", "yaml", "toml"]
    }
    fn schema_version(&self) -> u32 {
        SCHEMA_VERSION
    }

    fn import(&self, src: &Source) -> Result<Payload> {
        let text = src.text()?;
        let (value, datetimes) = match src.ext {
            "json" => (convert::from_json(text)?, Vec::new()),
            "yaml" | "yml" => (convert::from_yaml(text)?, Vec::new()),
            "toml" => convert::from_toml(text)?,
            other => return Err(Error::Other(format!("no .emc importer for .{other}"))),
        };

        let leaves = flatten(&value);
        let mut data = ChunkList::new();
        for n in &leaves {
            data.push(n.encode());
        }

        // Inference is the only source of rules a legacy file can offer, and
        // it is honest about its limits: types come from what is there, and
        // nothing is marked required, because one sample cannot distinguish
        // "always present" from "present this time".
        let mut schema = Schema::infer("config", &value);
        schema.version = SCHEMA_VERSION;
        for path in &datetimes {
            let generic = collapse_indices(path);
            match schema.fields.iter_mut().find(|f| f.path == generic) {
                Some(f) => f.ty = convert::DATETIME.into(),
                None => schema
                    .fields
                    .push(FieldRule::new(generic, convert::DATETIME)),
            }
        }

        Ok(Payload {
            summary: Some(summarize(&value, &leaves)),
            data,
            schema: Some(schema),
            // Absent, not empty: a converted file has made no capability
            // claim, and inventing a permissive one would be worse than
            // having none.
            caps: None,
            migrations: None,
        })
    }

    /// An empty configuration. Nothing is invented: a new `.emc` declares no
    /// keys, no capabilities and no rules, because a starter that guessed at
    /// any of them would be a claim the author never made.
    fn starter(&self, spec: &Starter) -> Result<(&'static str, Vec<u8>)> {
        spec.only("emc", &[])?;
        Ok(("json", b"{}".to_vec()))
    }

    fn export(&self, file: &WickFile, to: &str) -> Result<Vec<u8>> {
        let v = config(file)?;
        let locked = locked_paths(file);
        if !locked.is_empty() {
            // Writing a config that is silently missing its secrets is how a
            // deployment breaks at 3am. Refuse, and name what is missing.
            let names: Vec<String> = locked
                .iter()
                .map(|(s, l)| format!("slot {s} ({l})"))
                .collect();
            return Err(Error::Other(format!(
                "cannot export: {} is sealed and locked. Supply its passphrase, \
                 or use `hearth view` to see the public half",
                names.join(", ")
            )));
        }

        let s = match to {
            "json" => convert::to_json(&v)?,
            "yaml" => convert::to_yaml(&v)?,
            "toml" => {
                let dts: Vec<String> = file
                    .schema()?
                    .map(|s| {
                        s.fields
                            .iter()
                            .filter(|f| f.ty == convert::DATETIME)
                            .map(|f| f.path.clone())
                            .collect()
                    })
                    .unwrap_or_default();
                convert::to_toml(&v, &dts)?
            }
            other => return Err(Error::Other(format!(".emc cannot export to .{other}"))),
        };
        Ok(s.into_bytes())
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
        let schema = file.schema()?;
        let ns = nodes(file)?;
        let width = ns
            .iter()
            .map(|n| n.path.dotted().len())
            .max()
            .unwrap_or(0)
            .min(48);

        for n in ns.iter().take(opts.limit.unwrap_or(usize::MAX)) {
            let path = n.path.dotted();
            let unit = schema
                .as_ref()
                .and_then(|s| s.field(&collapse_indices(&path)))
                .and_then(|f| f.unit.clone())
                .map(|u| format!(" {u}"))
                .unwrap_or_default();
            writeln!(out, "{path:<width$}  {}{unit}", n.value.preview())?;
        }
        for (slot, label) in locked_paths(file) {
            writeln!(
                out,
                "\n\x1b[2m[sealed: slot {slot} ({label}) — passphrase required]\x1b[0m"
            )?;
        }
        if let Some(caps) = file.caps()? {
            writeln!(out, "\ncapabilities:")?;
            writeln!(out, "  network:    {}", caps.network)?;
            if !caps.filesystem.is_empty() {
                writeln!(out, "  filesystem: {}", caps.filesystem.join(", "))?;
            }
            if !caps.env_read.is_empty() {
                writeln!(out, "  env_read:   {}", caps.env_read.join(", "))?;
            }
            if let Some(mb) = caps.max_memory_mb {
                writeln!(out, "  memory:     {mb} MB")?;
            }
        }
        Ok(())
    }

    fn validate(&self, file: &WickFile) -> Result<Vec<Issue>> {
        let mut issues = Vec::new();
        let ns = nodes(file)?;
        let value = unflatten(&ns)?;

        if let Some(schema) = file.schema()? {
            if schema.kind != "config" {
                issues.push(Issue::error(
                    "SCHM",
                    format!(
                        "this is a .emc file but its schema describes '{}'",
                        schema.kind
                    ),
                ));
            }
            // Only check what is actually readable. A required field inside a
            // locked slot is present in the file; reporting it as missing
            // would be a false alarm caused by not holding a passphrase.
            if locked_paths(file).is_empty() {
                issues.extend(schema.check(&value));
            } else {
                for (path, v) in value.flatten() {
                    if let Some(rule) = schema.field(&collapse_indices(&path)) {
                        issues.extend(rule.check(v));
                    }
                }
                issues.push(Issue::note(
                    "",
                    "some values are sealed; required-field checks were skipped",
                ));
            }
        } else if let Some(slot) = file.sealed_slot(libwick::chunks::ChunkType::SCHM) {
            // Sealed and absent are different facts, and only one of them is
            // the reader's problem to solve.
            issues.push(Issue::note(
                "SCHM",
                format!("the schema is sealed to slot {slot}; unlock it to validate against it"),
            ));
        } else {
            issues.push(Issue::warning(
                "",
                "no embedded schema, so nothing can be validated against",
            ));
        }

        if let Some(caps) = file.caps()? {
            issues.extend(caps.lint());
        }

        // A duplicated path means two chunks claim the same key and the last
        // one silently wins — the kind of thing a hand-edited payload or a
        // bad migration produces.
        let mut seen = std::collections::HashSet::new();
        for n in &ns {
            let d = n.path.dotted();
            if !seen.insert(d.clone()) {
                issues.push(Issue::error(d, "path appears more than once"));
            }
        }
        Ok(issues)
    }

    fn diff(&self, a: &WickFile, b: &WickFile, _keys: &KeyRing) -> Result<Vec<Change>> {
        let (na, nb) = (nodes(a)?, nodes(b)?);
        let index = |ns: &[Node]| -> Vec<(String, Value)> {
            ns.iter()
                .map(|n| (n.path.dotted(), n.value.clone()))
                .collect()
        };
        let (ia, ib) = (index(&na), index(&nb));
        let mut out = Vec::new();

        for (path, old) in &ia {
            match ib.iter().find(|(p, _)| p == path) {
                None => out.push(Change::new(
                    ChangeKind::Removed,
                    path,
                    NODE,
                    format!("was {}", old.preview()),
                )),
                Some((_, new)) if new != old => out.push(Change::new(
                    ChangeKind::Modified,
                    path,
                    NODE,
                    if new.type_name() == old.type_name() {
                        format!("{} -> {}", old.preview(), new.preview())
                    } else {
                        // A type change is the one that breaks things
                        // downstream, so it is called out rather than shown
                        // as two values that happen to look different.
                        format!(
                            "{} -> {}  (type changed: {} -> {})",
                            old.preview(),
                            new.preview(),
                            old.type_name(),
                            new.type_name()
                        )
                    },
                )),
                Some(_) => {}
            }
        }
        for (path, new) in &ib {
            if !ia.iter().any(|(p, _)| p == path) {
                out.push(Change::new(
                    ChangeKind::Added,
                    path,
                    NODE,
                    format!("= {}", new.preview()),
                ));
            }
        }

        // The public half of a split-trust file diffs cleanly; the sealed
        // half must not be reported as absent when it is merely locked.
        for (slot, label) in locked_paths(b) {
            out.push(Change::new(
                ChangeKind::Modified,
                format!("[slot {slot}]"),
                SECR,
                format!("{label} is sealed and was not compared"),
            ));
        }
        Ok(out)
    }

    fn migrate_op(&self, op: &Op, data: &mut ChunkList) -> Result<Option<String>> {
        Ok(match op.op.as_str() {
            "rename_key" => {
                let from = op.str_arg("from")?;
                let to = op.str_arg("to")?;
                let mut n = 0;
                for c in data.0.iter_mut().filter(|c| c.ty == NODE) {
                    let mut node = Node::decode(c)?;
                    let d = node.path.dotted();
                    if d == from || d.starts_with(&format!("{from}.")) {
                        let tail = &d[from.len()..];
                        let new = format!("{to}{tail}");
                        node.path = parse_dotted(&new, &node.path);
                        *c = node.encode();
                        n += 1;
                    }
                }
                Some(format!("renamed {n} path(s) from '{from}' to '{to}'"))
            }
            "drop_key" => {
                let path = op.str_arg("path")?;
                let before = data.len();
                let mut keep = Vec::with_capacity(data.len());
                for c in data.0.iter() {
                    if c.ty == NODE {
                        let d = Node::decode(c)?.path.dotted();
                        if d == path || d.starts_with(&format!("{path}.")) {
                            continue;
                        }
                    }
                    keep.push(c.clone());
                }
                *data = ChunkList(keep);
                Some(format!(
                    "dropped {} node(s) under '{path}'",
                    before - data.len()
                ))
            }
            "set_default" => {
                let path = op.str_arg("path")?;
                let exists = data
                    .all(NODE)
                    .map(Node::decode)
                    .collect::<Result<Vec<_>>>()?
                    .iter()
                    .any(|n| n.path.dotted() == path);
                if exists {
                    Some(format!("'{path}' already set; default not applied"))
                } else {
                    let raw = op
                        .args
                        .get("value")
                        .ok_or_else(|| Error::Other("set_default needs a 'value'".into()))?;
                    let v = json_arg_to_value(raw);
                    data.push(
                        Node {
                            path: parse_dotted(path, &Path::default()),
                            value: v.clone(),
                        }
                        .encode(),
                    );
                    Some(format!("set '{path}' to its new default {}", v.preview()))
                }
            }
            _ => None,
        })
    }

    fn summarize(&self, data: &ChunkList) -> Result<Option<ChunkList>> {
        let ns: Vec<Node> = data.all(NODE).map(Node::decode).collect::<Result<_>>()?;
        let v = unflatten(&ns)?;
        Ok(Some(summarize(&v, &ns)))
    }
}

/// Rebuild segment kinds for a dotted path, reusing the kinds of an existing
/// path where they line up. A numeric segment is only an index if the path it
/// replaces had one there — otherwise a map key spelled "2" would silently
/// become a list slot.
fn parse_dotted(dotted: &str, like: &Path) -> Path {
    Path(
        dotted
            .split('.')
            .enumerate()
            .map(|(i, part)| match like.0.get(i) {
                Some(Seg::Index(_)) if part.bytes().all(|c| c.is_ascii_digit()) => {
                    Seg::Index(part.parse().unwrap_or(0))
                }
                _ => Seg::Key(part.to_string()),
            })
            .collect(),
    )
}

fn json_arg_to_value(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => Value::Int(i),
            None => Value::Float(n.as_f64().unwrap_or(0.0)),
        },
        serde_json::Value::String(s) => Value::Str(s.clone()),
        serde_json::Value::Array(a) => Value::List(a.iter().map(json_arg_to_value).collect()),
        serde_json::Value::Object(o) => Value::Map(
            o.iter()
                .map(|(k, v)| (k.clone(), json_arg_to_value(v)))
                .collect(),
        ),
    }
}

fn summarize(value: &Value, leaves: &[Node]) -> ChunkList {
    let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
    for n in leaves {
        *counts.entry(n.value.type_name()).or_default() += 1;
    }
    let top: Vec<String> = match value {
        Value::Map(m) => m.iter().map(|(k, _)| k.clone()).collect(),
        _ => Vec::new(),
    };
    let stat = serde_json::json!({
        "keys": leaves.len(),
        "depth": leaves.iter().map(|n| n.path.0.len()).max().unwrap_or(0),
        "types": counts,
        "top_level": top,
    });
    let mut summ = ChunkList::new();
    summ.push(Chunk::new(
        STAT,
        serde_json::to_vec(&stat).unwrap_or_default(),
    ));
    summ
}

fn render_summary(file: &WickFile, out: &mut dyn std::io::Write) -> Result<()> {
    let Some(summ) = file.summary()? else {
        return Err(Error::MissingChunk("SUMM"));
    };
    if let Some(stat) = summ.get(STAT) {
        let v: serde_json::Value = stat.as_json()?;
        writeln!(out, "{} settings, {} levels deep", v["keys"], v["depth"])?;
        if let Some(top) = v["top_level"].as_array() {
            let names: Vec<String> = top
                .iter()
                .map(|t| t.as_str().unwrap_or("").into())
                .collect();
            writeln!(out, "sections: {}", names.join(", "))?;
        }
    }
    Ok(())
}

/// Build a `.emc` `CAPS` declaration from a JSON document.
pub fn caps_from_json(src: &str) -> Result<Capabilities> {
    serde_json::from_str(src).map_err(Error::Json)
}

#[cfg(test)]
mod tests {
    use super::*;

    const JSON: &str = r#"{
  "name": "hearth",
  "database": {"host": "localhost", "port": 5432, "password": "hunter2"},
  "hosts": ["a", "b"],
  "features": {"2": "a key that looks like an index"},
  "plugins": [],
  "debug": false
}"#;

    fn build(src: &str, ext: &str) -> WickFile {
        let mut f = WickFile::new(TAG);
        let p = Emc
            .import(&Source::new(src.as_bytes(), "sample", ext))
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
    fn json_survives_the_full_round_trip() {
        let f = build(JSON, "json");
        let out = String::from_utf8(Emc.export(&f, "json").unwrap()).unwrap();
        assert_eq!(
            convert::from_json(&out).unwrap(),
            convert::from_json(JSON).unwrap()
        );
    }

    #[test]
    fn key_order_is_preserved_through_the_chunk_tree() {
        let f = build(JSON, "json");
        let out = String::from_utf8(Emc.export(&f, "json").unwrap()).unwrap();
        let order: Vec<&str> = ["name", "database", "hosts", "features", "plugins", "debug"]
            .into_iter()
            .collect();
        let positions: Vec<usize> = order
            .iter()
            .map(|k| out.find(&format!("\"{k}\"")).unwrap())
            .collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "{out}");
    }

    #[test]
    fn a_map_key_that_looks_like_an_index_stays_a_map_key() {
        let f = build(JSON, "json");
        let v = config(&f).unwrap();
        assert!(matches!(v.path("features"), Some(Value::Map(_))));
        assert_eq!(
            v.path("features.2"),
            Some(&Value::Str("a key that looks like an index".into()))
        );
    }

    #[test]
    fn empty_containers_are_not_dropped() {
        let f = build(JSON, "json");
        assert_eq!(
            config(&f).unwrap().path("plugins"),
            Some(&Value::List(vec![]))
        );
    }

    #[test]
    fn types_survive_the_round_trip() {
        let f = build(JSON, "json");
        let v = config(&f).unwrap();
        assert_eq!(v.path("database.port"), Some(&Value::Int(5432)));
        assert_eq!(v.path("debug"), Some(&Value::Bool(false)));
    }

    #[test]
    fn yaml_and_toml_import_and_cross_export() {
        let yaml = "name: hearth\nport: 8080\nnested:\n  flag: true\n";
        let f = build(yaml, "yaml");
        let as_toml = String::from_utf8(Emc.export(&f, "toml").unwrap()).unwrap();
        assert!(as_toml.contains("port = 8080"), "{as_toml}");
        let (back, _) = convert::from_toml(&as_toml).unwrap();
        assert_eq!(back.path("nested.flag"), Some(&Value::Bool(true)));
    }

    #[test]
    fn an_inferred_schema_validates_its_own_file() {
        let f = build(JSON, "json");
        let issues = Emc.validate(&f).unwrap();
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn a_type_change_is_reported_as_one() {
        let a = build(JSON, "json");
        let b = build(
            &JSON.replace("\"port\": 5432", "\"port\": \"5432\""),
            "json",
        );
        let d = Emc.diff(&a, &b, &KeyRing::empty()).unwrap();
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].path, "database.port");
        assert!(
            d[0].note.contains("type changed: int -> string"),
            "{}",
            d[0].note
        );
    }

    #[test]
    fn a_diff_names_the_setting_not_the_chunk() {
        let a = build(JSON, "json");
        let b = build(&JSON.replace("5432", "5433"), "json");
        let d = Emc.diff(&a, &b, &KeyRing::empty()).unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].path, "database.port");
        assert_eq!(d[0].note, "5432 -> 5433");
    }

    #[test]
    fn added_and_removed_settings_are_itemised() {
        let a = build(JSON, "json");
        let b = build(
            &JSON.replace("\"debug\": false", "\"verbose\": true"),
            "json",
        );
        let d = Emc.diff(&a, &b, &KeyRing::empty()).unwrap();
        assert_eq!(d.len(), 2);
        assert!(d
            .iter()
            .any(|c| c.kind == ChangeKind::Removed && c.path == "debug"));
        assert!(d
            .iter()
            .any(|c| c.kind == ChangeKind::Added && c.path == "verbose"));
    }

    #[test]
    fn split_trust_hides_only_the_sealed_half() {
        let dir = std::env::temp_dir().join(format!("emc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.emc");

        let mut f = build(JSON, "json");
        f.add_key_slot(1, "prod", "correct horse").unwrap();
        let moved = seal_paths(&mut f, 1, &["database.password".into()]).unwrap();
        assert_eq!(moved, 1);
        f.write(&path).unwrap();

        // Without the passphrase: public settings readable, secret invisible
        // but *announced*, and export refuses rather than writing a config
        // that is quietly missing a password.
        let locked = WickFile::read(&path).unwrap();
        let v = config(&locked).unwrap();
        assert_eq!(
            v.path("database.host"),
            Some(&Value::Str("localhost".into()))
        );
        assert_eq!(v.path("database.password"), None);
        assert_eq!(locked_paths(&locked), vec![(1, "prod".to_string())]);
        let err = match Emc.export(&locked, "json") {
            Err(e) => e.to_string(),
            Ok(_) => panic!("exported a config with a locked secret"),
        };
        assert!(err.contains("sealed"), "{err}");

        // With it: everything.
        let mut open = WickFile::read(&path).unwrap();
        open.unlock(1, "correct horse").unwrap();
        assert_eq!(
            config(&open).unwrap().path("database.password"),
            Some(&Value::Str("hunter2".into()))
        );
        assert!(Emc.export(&open, "json").is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capabilities_are_linted() {
        let mut f = build(JSON, "json");
        f.set_caps(&caps_from_json(r#"{"network": true, "filesystem": ["write:/"]}"#).unwrap())
            .unwrap();
        let issues = Emc.validate(&f).unwrap();
        assert!(
            issues
                .iter()
                .any(|i| i.message.contains("whole filesystem")),
            "{issues:?}"
        );
    }

    #[test]
    fn a_duplicated_path_is_caught() {
        let mut f = build(JSON, "json");
        let mut data = f.data().unwrap();
        let first = data.all(NODE).next().unwrap().clone();
        data.push(first);
        f.set_data(&data).unwrap();
        let issues = Emc.validate(&f).unwrap();
        assert!(
            issues.iter().any(|i| i.message.contains("more than once")),
            "{issues:?}"
        );
    }

    #[test]
    fn migration_operations_rewrite_paths_and_fill_defaults() {
        use libwick::migrate::{Rule, RuleSet};
        let f = build(JSON, "json");
        let mut data = f.data().unwrap();

        let rules = RuleSet::new().with(Rule {
            from: 1,
            to: 2,
            note: Some("db renamed and pooling added".into()),
            ops: vec![
                Op::new(
                    "rename_key",
                    serde_json::json!({"from": "database", "to": "db"}),
                ),
                Op::new(
                    "set_default",
                    serde_json::json!({"path": "db.pool", "value": 10}),
                ),
                Op::new("drop_key", serde_json::json!({"path": "debug"})),
            ],
        });
        let report =
            libwick::migrate::apply(&rules, &mut data, 1, 2, &mut |op, d| Emc.migrate_op(op, d))
                .unwrap();
        assert_eq!(report.steps.len(), 3);

        let ns: Vec<Node> = data
            .all(NODE)
            .map(Node::decode)
            .collect::<Result<_>>()
            .unwrap();
        let v = unflatten(&ns).unwrap();
        assert_eq!(v.path("db.port"), Some(&Value::Int(5432)));
        assert_eq!(v.path("db.pool"), Some(&Value::Int(10)));
        assert_eq!(v.path("database"), None);
        assert_eq!(v.path("debug"), None);
    }

    #[test]
    fn the_summary_answers_without_the_payload() {
        let f = build(JSON, "json");
        let mut buf = Vec::new();
        Emc.render(
            &f,
            &RenderOpts {
                summary: true,
                ..Default::default()
            },
            &mut buf,
        )
        .unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("settings"));
        assert!(s.contains("database"));
    }
}
