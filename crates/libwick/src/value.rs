//! A small self-describing value type, shared by the config and tabular
//! formats.
//!
//! `.emc` and `.emx` both need to carry "a typed scalar, or a structure of
//! them" through the chunk tree. They do not need JSON's parser or TOML's
//! date handling, and using `serde_json::Value` as the on-disk representation
//! would bake one legacy format's type system into the spine — JSON has no
//! integers, no bytes, and no ordered maps, and losing any of those is how a
//! converter corrupts a file quietly.
//!
//! Maps preserve insertion order. A config file that comes back with its keys
//! alphabetised has been damaged even though every value survived, and an
//! order-losing map would make every diff report spurious moves.

use crate::error::{Error, Result};

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    /// Ordered key-value pairs.
    Map(Vec<(String, Value)>),
}

impl Value {
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Str(_) => "string",
            Value::Bytes(_) => "bytes",
            Value::List(_) => "list",
            Value::Map(_) => "map",
        }
    }

    pub fn is_scalar(&self) -> bool {
        !matches!(self, Value::List(_) | Value::Map(_))
    }

    /// One-line rendering, for diffs and summaries. Long strings are elided
    /// because a semantic diff is unreadable if one changed value floods it.
    pub fn preview(&self) -> String {
        const MAX: usize = 60;
        let s = match self {
            Value::Null => "null".to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => format_float(*f),
            Value::Str(s) => format!("{s:?}"),
            Value::Bytes(b) => format!("<{} bytes>", b.len()),
            Value::List(v) => format!("[{} items]", v.len()),
            Value::Map(m) => format!("{{{} keys}}", m.len()),
        };
        if s.chars().count() > MAX {
            let head: String = s.chars().take(MAX - 1).collect();
            format!("{head}…")
        } else {
            s
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Map(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Look a dotted path up: `database.pool.max`.
    pub fn path(&self, path: &str) -> Option<&Value> {
        let mut cur = self;
        for part in path.split('.') {
            cur = cur.get(part)?;
        }
        Some(cur)
    }

    /// Flatten to `(dotted path, scalar)` pairs, depth first, in order.
    /// List elements get numeric path segments: `hosts.0`, `hosts.1`.
    ///
    /// This is what makes a config diff readable: comparing two flat lists of
    /// paths reports `database.port: 5432 -> 5433`, where comparing two trees
    /// would report that `database` changed and leave the user to look.
    pub fn flatten(&self) -> Vec<(String, &Value)> {
        let mut out = Vec::new();
        self.flatten_into(String::new(), &mut out);
        out
    }

    fn flatten_into<'a>(&'a self, prefix: String, out: &mut Vec<(String, &'a Value)>) {
        let child = |prefix: &str, key: &str| {
            if prefix.is_empty() {
                key.to_string()
            } else {
                format!("{prefix}.{key}")
            }
        };
        match self {
            Value::Map(m) if !m.is_empty() => {
                for (k, v) in m {
                    v.flatten_into(child(&prefix, k), out);
                }
            }
            Value::List(items) if !items.is_empty() => {
                for (i, v) in items.iter().enumerate() {
                    v.flatten_into(child(&prefix, &i.to_string()), out);
                }
            }
            // An empty container is a value in its own right: dropping it
            // would turn `plugins: []` into a missing key on round-trip.
            _ => out.push((prefix, self)),
        }
    }

    // ---- binary encoding -------------------------------------------------
    //
    // Tag byte, then payload. Lengths are u32 little-endian; a single config
    // value larger than 4 GiB is not a use case worth eight bytes on every
    // string in every file.

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    /// Append the encoding to an existing buffer, for callers that prefix a
    /// value with something of their own.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        fn len_prefixed(out: &mut Vec<u8>, b: &[u8]) {
            out.extend_from_slice(&(b.len() as u32).to_le_bytes());
            out.extend_from_slice(b);
        }
        match self {
            Value::Null => out.push(0),
            Value::Bool(b) => {
                out.push(1);
                out.push(*b as u8);
            }
            Value::Int(i) => {
                out.push(2);
                out.extend_from_slice(&i.to_le_bytes());
            }
            Value::Float(f) => {
                out.push(3);
                out.extend_from_slice(&f.to_le_bytes());
            }
            Value::Str(s) => {
                out.push(4);
                len_prefixed(out, s.as_bytes());
            }
            Value::Bytes(b) => {
                out.push(5);
                len_prefixed(out, b);
            }
            Value::List(items) => {
                out.push(6);
                out.extend_from_slice(&(items.len() as u32).to_le_bytes());
                for v in items {
                    v.encode_into(out);
                }
            }
            Value::Map(pairs) => {
                out.push(7);
                out.extend_from_slice(&(pairs.len() as u32).to_le_bytes());
                for (k, v) in pairs {
                    len_prefixed(out, k.as_bytes());
                    v.encode_into(out);
                }
            }
        }
    }

    pub fn decode(bytes: &[u8]) -> Result<Value> {
        let mut cur = 0usize;
        let v = Value::decode_at(bytes, &mut cur)?;
        if cur != bytes.len() {
            return Err(Error::Other(format!(
                "value has {} trailing bytes",
                bytes.len() - cur
            )));
        }
        Ok(v)
    }

    fn decode_at(b: &[u8], i: &mut usize) -> Result<Value> {
        fn take<'a>(b: &'a [u8], i: &mut usize, n: usize) -> Result<&'a [u8]> {
            let end = i.checked_add(n).ok_or(Error::Truncated("value"))?;
            if end > b.len() {
                return Err(Error::Truncated("value"));
            }
            let s = &b[*i..end];
            *i = end;
            Ok(s)
        }
        fn u32_at(b: &[u8], i: &mut usize) -> Result<usize> {
            Ok(u32::from_le_bytes(take(b, i, 4)?.try_into().unwrap()) as usize)
        }
        fn string_at(b: &[u8], i: &mut usize) -> Result<String> {
            let n = u32_at(b, i)?;
            String::from_utf8(take(b, i, n)?.to_vec())
                .map_err(|_| Error::Other("value contains invalid UTF-8".into()))
        }

        let tag = *take(b, i, 1)?.first().unwrap();
        Ok(match tag {
            0 => Value::Null,
            1 => Value::Bool(take(b, i, 1)?[0] != 0),
            2 => Value::Int(i64::from_le_bytes(take(b, i, 8)?.try_into().unwrap())),
            3 => Value::Float(f64::from_le_bytes(take(b, i, 8)?.try_into().unwrap())),
            4 => Value::Str(string_at(b, i)?),
            5 => {
                let n = u32_at(b, i)?;
                Value::Bytes(take(b, i, n)?.to_vec())
            }
            6 => {
                let n = u32_at(b, i)?;
                let mut items = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    items.push(Value::decode_at(b, i)?);
                }
                Value::List(items)
            }
            7 => {
                let n = u32_at(b, i)?;
                let mut pairs = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    let k = string_at(b, i)?;
                    pairs.push((k, Value::decode_at(b, i)?));
                }
                Value::Map(pairs)
            }
            other => return Err(Error::Other(format!("unknown value tag {other}"))),
        })
    }
}

/// Shortest representation that round-trips, so `1.0` does not become
/// `1` and lose its type on the way back out to JSON.
pub fn format_float(f: f64) -> String {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e15 {
        format!("{f:.1}")
    } else {
        format!("{f}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Value {
        Value::Map(vec![
            ("name".into(), Value::Str("hearth".into())),
            ("port".into(), Value::Int(8080)),
            ("ratio".into(), Value::Float(0.75)),
            ("debug".into(), Value::Bool(false)),
            ("missing".into(), Value::Null),
            (
                "hosts".into(),
                Value::List(vec![Value::Str("a".into()), Value::Str("b".into())]),
            ),
            ("plugins".into(), Value::List(vec![])),
            (
                "db".into(),
                Value::Map(vec![("pool".into(), Value::Int(10))]),
            ),
        ])
    }

    #[test]
    fn binary_round_trip() {
        let v = sample();
        assert_eq!(Value::decode(&v.encode()).unwrap(), v);
    }

    #[test]
    fn map_order_survives() {
        let v = sample();
        let back = Value::decode(&v.encode()).unwrap();
        let keys: Vec<_> = match &back {
            Value::Map(m) => m.iter().map(|(k, _)| k.as_str()).collect(),
            _ => panic!(),
        };
        assert_eq!(keys[0], "name");
        assert_eq!(keys[1], "port");
        assert_eq!(keys.last().unwrap(), &"db");
    }

    #[test]
    fn flatten_produces_dotted_paths() {
        let v = sample();
        let flat = v.flatten();
        let paths: Vec<_> = flat.iter().map(|(p, _)| p.as_str()).collect();
        assert!(paths.contains(&"db.pool"));
        assert!(paths.contains(&"hosts.0"));
        assert!(paths.contains(&"hosts.1"));
        // An empty list is kept as a value, not dropped.
        assert!(paths.contains(&"plugins"));
    }

    #[test]
    fn path_lookup() {
        let v = sample();
        assert_eq!(v.path("db.pool"), Some(&Value::Int(10)));
        assert_eq!(v.path("db.nope"), None);
    }

    #[test]
    fn truncated_input_is_rejected() {
        let b = sample().encode();
        assert!(Value::decode(&b[..b.len() / 2]).is_err());
    }
}
