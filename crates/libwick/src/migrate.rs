//! The `MIGR` chunk: a file's own upgrade path.
//!
//! The problem this solves is that a reading tool otherwise has to know every
//! historical version of every schema it might encounter, forever. That
//! knowledge lives in the tool, ages badly, and is exactly what nobody
//! maintains. Putting the upgrade rules in the file inverts it: a file
//! written in 2026 still knows how to present itself to a reader written in
//! 2031, and the reader only has to know how to *apply* rules.
//!
//! Rules are a declarative transform table, never code. A migration step
//! renames a chunk, drops one, inserts one, or hands a named operation to
//! the format plugin. It cannot loop, branch, read the filesystem, or do
//! anything else that would make opening an untrusted file interesting.
//!
//! The version being migrated is the *payload schema* version from `SCHM`,
//! not the Wick spec version in the header. Those move independently: a
//! `.emc` file's config schema can reach version 5 while the container it
//! sits in is still Wick v1.0.

use crate::chunks::{Chunk, ChunkList, ChunkType};
use crate::error::{Error, Result};
use crate::header::Version;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Op {
    pub op: String,
    /// Operation arguments. Kept as free-form JSON so a plugin can define
    /// operations the spine has never heard of.
    #[serde(flatten)]
    pub args: serde_json::Map<String, serde_json::Value>,
}

impl Op {
    pub fn new(op: &str, args: serde_json::Value) -> Self {
        Op {
            op: op.to_string(),
            args: match args {
                serde_json::Value::Object(m) => m,
                _ => Default::default(),
            },
        }
    }

    pub fn str_arg(&self, name: &str) -> Result<&str> {
        self.args.get(name).and_then(|v| v.as_str()).ok_or_else(|| {
            Error::Other(format!(
                "migration op '{}' needs a string '{name}'",
                self.op
            ))
        })
    }

    pub fn opt_str(&self, name: &str) -> Option<&str> {
        self.args.get(name).and_then(|v| v.as_str())
    }
}

/// One hop: everything needed to move a payload from one schema version to
/// the next.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rule {
    pub from: u32,
    pub to: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub ops: Vec<Op>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RuleSet {
    #[serde(default)]
    pub rules: Vec<Rule>,
    /// The Wick spec version these rules were written against, for the
    /// record. Rules do not change container layout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<String>,
}

impl RuleSet {
    pub fn new() -> Self {
        RuleSet::default()
    }

    pub fn with(mut self, rule: Rule) -> Self {
        self.rules.push(rule);
        self
    }

    pub fn encode(&self) -> Result<Chunk> {
        Ok(Chunk::new(ChunkType::MIGR, serde_json::to_vec(self)?))
    }

    /// Ordered hops from `from` to `to`, or an error naming the gap.
    ///
    /// Breadth-first rather than a straight walk because nothing forbids two
    /// paths — a format that shipped 1→2→3 and later added a direct 1→3
    /// should take the short one.
    pub fn plan(&self, from: u32, to: u32) -> Result<Vec<&Rule>> {
        if from == to {
            return Ok(Vec::new());
        }
        let mut queue = std::collections::VecDeque::from([from]);
        let mut came_from: std::collections::HashMap<u32, &Rule> = Default::default();
        let mut seen = std::collections::HashSet::from([from]);

        while let Some(v) = queue.pop_front() {
            for r in self.rules.iter().filter(|r| r.from == v) {
                if !seen.insert(r.to) {
                    continue;
                }
                came_from.insert(r.to, r);
                if r.to == to {
                    let mut path = Vec::new();
                    let mut cur = to;
                    while cur != from {
                        let rule = came_from[&cur];
                        path.push(rule);
                        cur = rule.from;
                    }
                    path.reverse();
                    return Ok(path);
                }
                queue.push_back(r.to);
            }
        }
        Err(Error::NoMigrationPath {
            from: Version::new(from.min(255) as u8, 0),
            to: Version::new(to.min(255) as u8, 0),
        })
    }

    /// The newest version any rule can reach from `from`.
    pub fn latest_from(&self, from: u32) -> u32 {
        let mut best = from;
        let mut changed = true;
        while changed {
            changed = false;
            for r in &self.rules {
                if r.from <= best && r.to > best {
                    best = r.to;
                    changed = true;
                }
            }
        }
        best
    }
}

#[derive(Debug, Default)]
pub struct Report {
    pub from: u32,
    pub to: u32,
    /// One line per operation applied, for the CLI to print.
    pub steps: Vec<String>,
}

/// Apply a plan to a payload tree.
///
/// `custom` is the plugin's hook. It is called for every operation the spine
/// does not recognise and returns whether it handled it; an operation nobody
/// handles is an error, never a silent no-op, because a migration that
/// half-runs leaves a file claiming a version it does not have.
pub fn apply(
    rules: &RuleSet,
    data: &mut ChunkList,
    from: u32,
    to: u32,
    custom: &mut dyn FnMut(&Op, &mut ChunkList) -> Result<Option<String>>,
) -> Result<Report> {
    let plan = rules.plan(from, to)?;
    let mut report = Report {
        from,
        to,
        steps: Vec::new(),
    };
    for rule in plan {
        for op in &rule.ops {
            let line = match apply_builtin(op, data)? {
                Some(line) => line,
                None => custom(op, data)?.ok_or_else(|| {
                    Error::Other(format!(
                        "migration v{}->v{} uses operation '{}', which neither the spine \
                         nor this format's plugin implements",
                        rule.from, rule.to, op.op
                    ))
                })?,
            };
            report
                .steps
                .push(format!("v{} -> v{}: {line}", rule.from, rule.to));
        }
    }
    Ok(report)
}

/// The operations every format gets for free. Deliberately few: anything
/// that needs to understand what a chunk *means* belongs to the plugin.
fn apply_builtin(op: &Op, data: &mut ChunkList) -> Result<Option<String>> {
    Ok(match op.op.as_str() {
        "rename_chunk" => {
            let from = ChunkType::parse(op.str_arg("from")?)?;
            let to = ChunkType::parse(op.str_arg("to")?)?;
            let mut n = 0;
            for c in data.0.iter_mut() {
                if c.ty == from {
                    c.ty = to;
                    n += 1;
                }
            }
            Some(format!("renamed {n} {from} chunk(s) to {to}"))
        }
        "drop_chunk" => {
            let ty = ChunkType::parse(op.str_arg("type")?)?;
            let before = data.len();
            data.0.retain(|c| c.ty != ty);
            Some(format!("dropped {} {ty} chunk(s)", before - data.len()))
        }
        "add_chunk" => {
            let ty = ChunkType::parse(op.str_arg("type")?)?;
            let text = op.opt_str("text").unwrap_or_default();
            data.push(Chunk::text(ty, text));
            Some(format!("added a {ty} chunk"))
        }
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ruleset() -> RuleSet {
        RuleSet::new()
            .with(Rule {
                from: 1,
                to: 2,
                note: Some("sections became paragraphs".into()),
                ops: vec![Op::new(
                    "rename_chunk",
                    json!({"from": "SECT", "to": "PARA"}),
                )],
            })
            .with(Rule {
                from: 2,
                to: 3,
                note: None,
                ops: vec![Op::new("drop_chunk", json!({"type": "TEMP"}))],
            })
    }

    fn payload() -> ChunkList {
        ChunkList(vec![
            Chunk::text(ChunkType::new(b"SECT"), "one"),
            Chunk::text(ChunkType::new(b"SECT"), "two"),
            Chunk::text(ChunkType::new(b"TEMP"), "scratch"),
        ])
    }

    fn no_custom(op: &Op, _d: &mut ChunkList) -> Result<Option<String>> {
        let _ = op;
        Ok(None)
    }

    #[test]
    fn plans_a_multi_step_upgrade() {
        let rs = ruleset();
        let plan = rs.plan(1, 3).unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].to, 2);
        assert_eq!(plan[1].to, 3);
    }

    #[test]
    fn prefers_a_direct_route_when_one_exists() {
        let rs = ruleset().with(Rule {
            from: 1,
            to: 3,
            note: None,
            ops: vec![],
        });
        assert_eq!(rs.plan(1, 3).unwrap().len(), 1);
    }

    #[test]
    fn applies_every_step_in_order() {
        let mut d = payload();
        let r = apply(&ruleset(), &mut d, 1, 3, &mut no_custom).unwrap();
        assert_eq!(r.steps.len(), 2);
        assert_eq!(d.all(ChunkType::new(b"PARA")).count(), 2);
        assert_eq!(d.all(ChunkType::new(b"SECT")).count(), 0);
        assert_eq!(d.all(ChunkType::new(b"TEMP")).count(), 0);
    }

    #[test]
    fn a_missing_path_is_an_error_not_a_no_op() {
        let mut d = payload();
        assert!(matches!(
            apply(&ruleset(), &mut d, 1, 9, &mut no_custom),
            Err(Error::NoMigrationPath { .. })
        ));
        // The payload is untouched when no plan exists.
        assert_eq!(d.all(ChunkType::new(b"SECT")).count(), 2);
    }

    #[test]
    fn an_unknown_operation_fails_loudly() {
        let rs = RuleSet::new().with(Rule {
            from: 1,
            to: 2,
            note: None,
            ops: vec![Op::new("reticulate_splines", json!({}))],
        });
        let mut d = payload();
        let err = apply(&rs, &mut d, 1, 2, &mut no_custom).unwrap_err();
        assert!(err.to_string().contains("reticulate_splines"));
    }

    #[test]
    fn a_plugin_can_supply_its_own_operations() {
        let rs = RuleSet::new().with(Rule {
            from: 1,
            to: 2,
            note: None,
            ops: vec![Op::new("uppercase_all", json!({}))],
        });
        let mut d = payload();
        let mut custom = |op: &Op, data: &mut ChunkList| -> Result<Option<String>> {
            if op.op != "uppercase_all" {
                return Ok(None);
            }
            for c in data.0.iter_mut() {
                c.value = c.as_str()?.to_uppercase().into_bytes();
            }
            Ok(Some("uppercased every chunk".into()))
        };
        apply(&rs, &mut d, 1, 2, &mut custom).unwrap();
        assert_eq!(d.0[0].as_str().unwrap(), "ONE");
    }

    #[test]
    fn latest_from_follows_the_chain() {
        assert_eq!(ruleset().latest_from(1), 3);
        assert_eq!(ruleset().latest_from(2), 3);
        assert_eq!(ruleset().latest_from(3), 3);
    }
}
