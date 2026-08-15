//! The `SCHM` chunk: a file's validation rules, carried inside the file.
//!
//! The point of embedding a schema rather than referencing one is that an
//! external `schema.json` drifts. It gets edited without the data, or the
//! data gets copied somewhere the schema is not, and the first anyone hears
//! about it is a production incident. A `SCHM` chunk travels with the bytes
//! it describes and is covered by the same content hash.
//!
//! The rule language is deliberately small — types, requiredness, ranges,
//! enumerations and units. It is not JSON Schema and does not try to be:
//! a schema that can express arbitrary logic is a schema no other tool can
//! fully implement, which defeats the purpose of shipping it inside the file.
//! Anything beyond these rules belongs in the format plugin's own validator.

use crate::chunks::{Chunk, ChunkType};
use crate::error::Result;
use crate::value::Value;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// The file violates its own declared rules.
    Error,
    /// Legal, but probably not what anyone meant.
    Warning,
    /// Worth knowing while looking at the file.
    Note,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Issue {
    pub severity: Severity,
    /// Where in the payload: a dotted key path, a column name, a section
    /// index — whatever the format addresses things by.
    pub path: String,
    pub message: String,
}

impl Issue {
    pub fn error(path: impl Into<String>, message: impl Into<String>) -> Self {
        Issue {
            severity: Severity::Error,
            path: path.into(),
            message: message.into(),
        }
    }

    pub fn warning(path: impl Into<String>, message: impl Into<String>) -> Self {
        Issue {
            severity: Severity::Warning,
            path: path.into(),
            message: message.into(),
        }
    }

    pub fn note(path: impl Into<String>, message: impl Into<String>) -> Self {
        Issue {
            severity: Severity::Note,
            path: path.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Issue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.path.is_empty() {
            write!(f, "{}: {}", self.severity.label(), self.message)
        } else {
            write!(
                f,
                "{}: {}: {}",
                self.severity.label(),
                self.path,
                self.message
            )
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FieldRule {
    /// Dotted path for config, column name for tabular data.
    pub path: String,
    /// One of the `Value` type names, or `any`.
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub required: bool,
    /// Physical or currency unit, e.g. `m/s`, `USD`. Only `.emx` enforces
    /// arithmetic on these, but any format may declare them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    /// Free-text note carried into `hearth view` output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

impl FieldRule {
    pub fn new(path: impl Into<String>, ty: &str) -> Self {
        FieldRule {
            path: path.into(),
            ty: ty.to_string(),
            required: false,
            unit: None,
            allowed: None,
            min: None,
            max: None,
            doc: None,
        }
    }

    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    pub fn with_unit(mut self, u: &str) -> Self {
        self.unit = Some(u.to_string());
        self
    }

    fn accepts_type(&self, v: &Value) -> bool {
        if self.ty == "any" {
            return true;
        }
        // An integer where a float is expected is fine; the reverse is not,
        // because rounding is a decision the file should have to state.
        matches!(
            (self.ty.as_str(), v),
            ("null", Value::Null)
                | ("bool", Value::Bool(_))
                | ("int", Value::Int(_))
                | ("float", Value::Float(_))
                | ("float", Value::Int(_))
                | ("string", Value::Str(_))
                | ("bytes", Value::Bytes(_))
                | ("list", Value::List(_))
                | ("map", Value::Map(_))
        )
    }

    pub fn check(&self, v: &Value) -> Vec<Issue> {
        let mut out = Vec::new();
        if !self.accepts_type(v) {
            out.push(Issue::error(
                &self.path,
                format!("declared {}, found {}", self.ty, v.type_name()),
            ));
            return out;
        }
        let numeric = match v {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        };
        if let (Some(n), Some(min)) = (numeric, self.min) {
            if n < min {
                out.push(Issue::error(
                    &self.path,
                    format!("{n} is below minimum {min}"),
                ));
            }
        }
        if let (Some(n), Some(max)) = (numeric, self.max) {
            if n > max {
                out.push(Issue::error(
                    &self.path,
                    format!("{n} is above maximum {max}"),
                ));
            }
        }
        if let (Some(allowed), Value::Str(s)) = (&self.allowed, v) {
            if !allowed.contains(s) {
                out.push(Issue::error(
                    &self.path,
                    format!("{s:?} is not one of {}", allowed.join(", ")),
                ));
            }
        }
        out
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Schema {
    /// Which payload this describes: `config`, `table`, `text`, `document`,
    /// `image`. Checked against the file's format tag so a `.emx` cannot
    /// carry a config schema and claim to validate.
    pub kind: String,
    /// The payload schema's own version, independent of the Wick spec
    /// version in the header. This is what `MIGR` rules move between.
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default)]
    pub fields: Vec<FieldRule>,
    /// Rules only one plugin understands. Kept opaque here so the spine
    /// never needs to know about, say, image colour spaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,
}

fn one() -> u32 {
    1
}

impl Schema {
    pub fn new(kind: &str) -> Self {
        Schema {
            kind: kind.to_string(),
            version: 1,
            fields: Vec::new(),
            extra: None,
        }
    }

    pub fn field(&self, path: &str) -> Option<&FieldRule> {
        self.fields.iter().find(|f| f.path == path)
    }

    /// Build a schema from a value that is assumed correct. Inference is
    /// how a converted legacy file gets a schema at all — a `.json` has no
    /// rules to import — and it is honest about that: every inferred field
    /// is optional, because one sample cannot tell required from present.
    pub fn infer(kind: &str, v: &Value) -> Schema {
        let mut s = Schema::new(kind);
        for (path, val) in v.flatten() {
            if path.is_empty() {
                continue;
            }
            // List indices are collapsed: `hosts.0` and `hosts.1` describe
            // one field, not two, and a schema pinned to today's element
            // count would fail the moment someone adds a host.
            let generic = collapse_indices(&path);
            if s.fields.iter().any(|f| f.path == generic) {
                continue;
            }
            s.fields.push(FieldRule::new(generic, val.type_name()));
        }
        s
    }

    pub fn check(&self, v: &Value) -> Vec<Issue> {
        let mut out = Vec::new();
        let flat = v.flatten();

        for (path, val) in &flat {
            let generic = collapse_indices(path);
            if let Some(rule) = self.field(&generic) {
                out.extend(rule.check(val));
            }
        }
        for rule in &self.fields {
            if rule.required && !flat.iter().any(|(p, _)| collapse_indices(p) == rule.path) {
                out.push(Issue::error(&rule.path, "required field is missing"));
            }
        }
        out
    }

    pub fn decode(chunk: &Chunk) -> Result<Self> {
        Ok(serde_json::from_slice(&chunk.value)?)
    }

    pub fn encode(&self) -> Result<Chunk> {
        Ok(Chunk::new(ChunkType::SCHM, serde_json::to_vec(self)?))
    }
}

/// `hosts.0.name` -> `hosts.*.name`.
pub fn collapse_indices(path: &str) -> String {
    path.split('.')
        .map(|p| {
            if !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit()) {
                "*"
            } else {
                p
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Value {
        Value::Map(vec![
            ("port".into(), Value::Int(8080)),
            ("name".into(), Value::Str("hearth".into())),
            (
                "hosts".into(),
                Value::List(vec![Value::Str("a".into()), Value::Str("b".into())]),
            ),
        ])
    }

    #[test]
    fn inference_collapses_list_indices() {
        let s = Schema::infer("config", &config());
        assert!(s.field("hosts.*").is_some());
        assert!(s.field("hosts.0").is_none());
        assert_eq!(s.field("port").unwrap().ty, "int");
    }

    #[test]
    fn an_inferred_schema_validates_its_own_source() {
        let v = config();
        assert!(Schema::infer("config", &v).check(&v).is_empty());
    }

    #[test]
    fn type_changes_are_errors() {
        let mut s = Schema::infer("config", &config());
        s.fields.push(FieldRule::new("port", "int"));
        let broken = Value::Map(vec![("port".into(), Value::Str("8080".into()))]);
        let issues = s.check(&broken);
        assert!(issues.iter().any(|i| i.severity == Severity::Error));
        assert!(issues[0].message.contains("declared int, found string"));
    }

    #[test]
    fn ranges_and_enumerations_are_enforced() {
        let mut s = Schema::new("config");
        let mut r = FieldRule::new("port", "int");
        r.min = Some(1.0);
        r.max = Some(65535.0);
        s.fields.push(r);
        let mut e = FieldRule::new("mode", "string");
        e.allowed = Some(vec!["dev".into(), "prod".into()]);
        s.fields.push(e);

        let v = Value::Map(vec![
            ("port".into(), Value::Int(99999)),
            ("mode".into(), Value::Str("staging".into())),
        ]);
        let issues = s.check(&v);
        assert_eq!(issues.len(), 2);
        assert!(issues[0].message.contains("above maximum"));
        assert!(issues[1].message.contains("is not one of"));
    }

    #[test]
    fn required_fields_are_reported_when_absent() {
        let mut s = Schema::new("config");
        s.fields.push(FieldRule::new("token", "string").required());
        let issues = s.check(&Value::Map(vec![]));
        assert_eq!(issues.len(), 1);
        assert!(issues[0].message.contains("required"));
    }

    #[test]
    fn integers_satisfy_float_fields_but_not_the_reverse() {
        let r = FieldRule::new("x", "float");
        assert!(r.check(&Value::Int(3)).is_empty());
        let r = FieldRule::new("x", "int");
        assert!(!r.check(&Value::Float(3.5)).is_empty());
    }
}
