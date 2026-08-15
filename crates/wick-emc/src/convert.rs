//! Legacy config formats in and out of [`libwick::Value`].
//!
//! Three source languages, one internal representation. The conversion is
//! where fidelity is won or lost, so a few decisions are worth stating:
//!
//! **Key order is preserved.** JSON, YAML and TOML are all order-preserving
//! as written, and a config file that comes back alphabetised has been
//! damaged even though every value survived. `serde_json` and `toml` are both
//! built here with their order-preserving map, and `Value::Map` is an ordered
//! vector rather than a map for the same reason.
//!
//! **Integers stay integers.** JSON has one number type and JavaScript has
//! one number type, but a port number is not 8080.0 and a 64-bit id does not
//! survive a trip through an f64. Whole numbers that fit in an `i64` are
//! imported as integers.
//!
//! **TOML datetimes survive as a declared type.** There is no datetime in the
//! internal value model — adding one would push a single source language's
//! type system into every format in the family — so a datetime is carried as
//! a string whose `SCHM` field type is `datetime`. Exporting back to TOML
//! reads that declaration and restores the datetime. This is the embedded
//! schema doing real work rather than only describing.

use libwick::error::{Error, Result};
use libwick::value::{format_float, Value};

/// Field type name used for values that were TOML datetimes.
pub const DATETIME: &str = "datetime";

// ---- JSON -----------------------------------------------------------------

pub fn from_json(src: &str) -> Result<Value> {
    let v: serde_json::Value =
        serde_json::from_str(src).map_err(|e| Error::Other(format!("invalid JSON: {e}")))?;
    Ok(json_to_value(&v))
}

fn json_to_value(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => Value::Int(i),
            None => Value::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        serde_json::Value::String(s) => Value::Str(s.clone()),
        serde_json::Value::Array(a) => Value::List(a.iter().map(json_to_value).collect()),
        serde_json::Value::Object(o) => Value::Map(
            o.iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect(),
        ),
    }
}

pub fn to_json(v: &Value) -> Result<String> {
    serde_json::to_string_pretty(&value_to_json(v))
        .map(|s| s + "\n")
        .map_err(Error::Json)
}

fn value_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(i) => serde_json::Value::from(*i),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            // JSON cannot express NaN or infinity; a string is the only
            // honest option and is at least visible to whoever reads it.
            .unwrap_or_else(|| serde_json::Value::String(format_float(*f))),
        Value::Str(s) => serde_json::Value::String(s.clone()),
        Value::Bytes(b) => serde_json::Value::String(libwick::hex::encode(b)),
        Value::List(l) => serde_json::Value::Array(l.iter().map(value_to_json).collect()),
        Value::Map(m) => serde_json::Value::Object(
            m.iter()
                .map(|(k, v)| (k.clone(), value_to_json(v)))
                .collect(),
        ),
    }
}

// ---- YAML -----------------------------------------------------------------

pub fn from_yaml(src: &str) -> Result<Value> {
    let v: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(src).map_err(|e| Error::Other(format!("invalid YAML: {e}")))?;
    yaml_to_value(&v)
}

fn yaml_to_value(v: &serde_yaml_ng::Value) -> Result<Value> {
    use serde_yaml_ng::Value as Y;
    Ok(match v {
        Y::Null => Value::Null,
        Y::Bool(b) => Value::Bool(*b),
        Y::Number(n) => match n.as_i64() {
            Some(i) => Value::Int(i),
            None => Value::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        Y::String(s) => Value::Str(s.clone()),
        Y::Sequence(s) => Value::List(s.iter().map(yaml_to_value).collect::<Result<_>>()?),
        Y::Mapping(m) => {
            let mut out = Vec::with_capacity(m.len());
            for (k, v) in m {
                // YAML permits any node as a key. Nothing else in the family
                // does, and silently stringifying a complex key would make
                // the round-trip lossy in a way nobody would notice.
                let key = match k {
                    Y::String(s) => s.clone(),
                    Y::Number(n) => n.to_string(),
                    Y::Bool(b) => b.to_string(),
                    other => {
                        return Err(Error::Other(format!(
                            "YAML mapping key {other:?} is not a scalar; \
                             .emc keys must be strings"
                        )))
                    }
                };
                out.push((key, yaml_to_value(v)?));
            }
            Value::Map(out)
        }
        Y::Tagged(t) => yaml_to_value(&t.value)?,
    })
}

pub fn to_yaml(v: &Value) -> Result<String> {
    serde_yaml_ng::to_string(&value_to_yaml(v))
        .map_err(|e| Error::Other(format!("could not write YAML: {e}")))
}

fn value_to_yaml(v: &Value) -> serde_yaml_ng::Value {
    use serde_yaml_ng::Value as Y;
    match v {
        Value::Null => Y::Null,
        Value::Bool(b) => Y::Bool(*b),
        Value::Int(i) => Y::Number((*i).into()),
        Value::Float(f) => Y::Number((*f).into()),
        Value::Str(s) => Y::String(s.clone()),
        Value::Bytes(b) => Y::String(libwick::hex::encode(b)),
        Value::List(l) => Y::Sequence(l.iter().map(value_to_yaml).collect()),
        Value::Map(m) => Y::Mapping(
            m.iter()
                .map(|(k, v)| (Y::String(k.clone()), value_to_yaml(v)))
                .collect(),
        ),
    }
}

// ---- TOML -----------------------------------------------------------------

/// Returns the value and the dotted paths that held datetimes, so the
/// importer can record their real type in `SCHM`.
pub fn from_toml(src: &str) -> Result<(Value, Vec<String>)> {
    let v: toml::Value =
        toml::from_str(src).map_err(|e| Error::Other(format!("invalid TOML: {e}")))?;
    let mut datetimes = Vec::new();
    let value = toml_to_value(&v, String::new(), &mut datetimes);
    Ok((value, datetimes))
}

fn toml_to_value(v: &toml::Value, path: String, datetimes: &mut Vec<String>) -> Value {
    let child = |p: &str, k: &str| {
        if p.is_empty() {
            k.to_string()
        } else {
            format!("{p}.{k}")
        }
    };
    match v {
        toml::Value::String(s) => Value::Str(s.clone()),
        toml::Value::Integer(i) => Value::Int(*i),
        toml::Value::Float(f) => Value::Float(*f),
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Datetime(d) => {
            datetimes.push(path);
            Value::Str(d.to_string())
        }
        toml::Value::Array(a) => Value::List(
            a.iter()
                .enumerate()
                .map(|(i, x)| toml_to_value(x, child(&path, &i.to_string()), datetimes))
                .collect(),
        ),
        toml::Value::Table(t) => Value::Map(
            t.iter()
                .map(|(k, x)| (k.clone(), toml_to_value(x, child(&path, k), datetimes)))
                .collect(),
        ),
    }
}

/// `datetime_paths` comes from the embedded schema; strings at those paths
/// are restored as TOML datetimes rather than quoted.
pub fn to_toml(v: &Value, datetime_paths: &[String]) -> Result<String> {
    let t = value_to_toml(v, String::new(), datetime_paths)?;
    // TOML requires a table at the top level. A config that is a bare list or
    // scalar is legal JSON and legal YAML, and there is no honest way to
    // write it as TOML, so say so rather than inventing a wrapper key.
    if !matches!(t, toml::Value::Table(_)) {
        return Err(Error::Other(format!(
            "TOML requires a table at the top level; this config is a {}",
            v.type_name()
        )));
    }
    toml::to_string_pretty(&t).map_err(|e| Error::Other(format!("could not write TOML: {e}")))
}

fn value_to_toml(v: &Value, path: String, dts: &[String]) -> Result<toml::Value> {
    let child = |p: &str, k: &str| {
        if p.is_empty() {
            k.to_string()
        } else {
            format!("{p}.{k}")
        }
    };
    Ok(match v {
        // TOML has no null. Dropping the key would change the config's
        // meaning, so this is an error the user has to resolve.
        Value::Null => {
            return Err(Error::Other(format!(
                "TOML has no null; '{path}' cannot be exported"
            )))
        }
        Value::Bool(b) => toml::Value::Boolean(*b),
        Value::Int(i) => toml::Value::Integer(*i),
        Value::Float(f) => toml::Value::Float(*f),
        Value::Str(s) => {
            let generic = libwick::schema::collapse_indices(&path);
            if dts.iter().any(|d| *d == path || *d == generic) {
                match s.parse::<toml::value::Datetime>() {
                    Ok(d) => toml::Value::Datetime(d),
                    Err(_) => toml::Value::String(s.clone()),
                }
            } else {
                toml::Value::String(s.clone())
            }
        }
        Value::Bytes(b) => toml::Value::String(libwick::hex::encode(b)),
        Value::List(l) => toml::Value::Array(
            l.iter()
                .enumerate()
                .map(|(i, x)| value_to_toml(x, child(&path, &i.to_string()), dts))
                .collect::<Result<_>>()?,
        ),
        Value::Map(m) => {
            let mut t = toml::map::Map::new();
            for (k, x) in m {
                t.insert(k.clone(), value_to_toml(x, child(&path, k), dts)?);
            }
            toml::Value::Table(t)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_preserves_key_order_and_integer_types() {
        let v = from_json(r#"{"zebra":1,"apple":2,"port":8080,"ratio":0.5}"#).unwrap();
        let keys: Vec<_> = match &v {
            Value::Map(m) => m.iter().map(|(k, _)| k.as_str()).collect(),
            _ => panic!(),
        };
        assert_eq!(keys, vec!["zebra", "apple", "port", "ratio"]);
        assert_eq!(v.path("port"), Some(&Value::Int(8080)));
        assert_eq!(v.path("ratio"), Some(&Value::Float(0.5)));
    }

    #[test]
    fn json_round_trips() {
        let src = r#"{"a":[1,2,{"b":true}],"c":null,"d":"text"}"#;
        let v = from_json(src).unwrap();
        assert_eq!(from_json(&to_json(&v).unwrap()).unwrap(), v);
    }

    #[test]
    fn yaml_round_trips() {
        let src = "name: hearth\nport: 8080\nhosts:\n  - a\n  - b\nnested:\n  on: true\n";
        let v = from_yaml(src).unwrap();
        assert_eq!(v.path("port"), Some(&Value::Int(8080)));
        assert_eq!(from_yaml(&to_yaml(&v).unwrap()).unwrap(), v);
    }

    #[test]
    fn yaml_complex_keys_are_refused_rather_than_mangled() {
        let err = from_yaml("? [a, b]\n: value\n").unwrap_err().to_string();
        assert!(err.contains("not a scalar"), "{err}");
    }

    #[test]
    fn toml_datetimes_survive_via_the_schema() {
        let src = "name = \"x\"\nreleased = 2026-08-14T10:22:00Z\n";
        let (v, dts) = from_toml(src).unwrap();
        assert_eq!(dts, vec!["released"]);
        let out = to_toml(&v, &dts).unwrap();
        assert!(out.contains("released = 2026-08-14T10:22:00Z"), "{out}");
        // Without the schema hint the same value comes back quoted, which is
        // exactly the silent type change the declaration prevents.
        assert!(to_toml(&v, &[]).unwrap().contains("released = \""));
    }

    #[test]
    fn toml_refuses_what_it_cannot_express() {
        let v = Value::Map(vec![("x".into(), Value::Null)]);
        assert!(to_toml(&v, &[])
            .unwrap_err()
            .to_string()
            .contains("no null"));

        let list = Value::List(vec![Value::Int(1)]);
        assert!(to_toml(&list, &[])
            .unwrap_err()
            .to_string()
            .contains("table at the top level"));
    }

    #[test]
    fn malformed_input_names_the_language() {
        assert!(from_json("{oops")
            .unwrap_err()
            .to_string()
            .contains("invalid JSON"));
        assert!(from_toml("= 1")
            .unwrap_err()
            .to_string()
            .contains("invalid TOML"));
    }
}
