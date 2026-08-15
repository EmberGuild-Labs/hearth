//! The `CAPS` chunk: what a file is allowed to ask a runtime for.
//!
//! Relevant to `.emc` today and to any future format a runtime *interprets*
//! rather than merely displays. The rule from the spec is that this is
//! declarative, not advisory: a Wick-aware runtime must refuse to grant more
//! than the file declares. The file does not ask nicely, and it cannot ask
//! for more later.
//!
//! Hearth executes nothing, so it validates and reports rather than enforces.
//! [`Grant`] is the enforcement primitive a runtime that *does* execute
//! something is expected to build on, and it is here rather than in that
//! runtime so that every consumer applies the same path normalisation — a
//! capability check that each caller reimplements is a capability check with
//! a different bug in each caller.

use crate::chunks::{Chunk, ChunkType};
use crate::error::{Error, Result};
use crate::schema::Issue;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access {
    Read,
    Write,
}

impl Access {
    pub fn label(self) -> &'static str {
        match self {
            Access::Read => "read",
            Access::Write => "write",
        }
    }
}

/// One entry of the `filesystem` list: `read:./data`, `write:./out`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsRule {
    pub access: Access,
    pub prefix: PathBuf,
}

impl FsRule {
    pub fn parse(s: &str) -> Result<Self> {
        let (mode, path) = s.split_once(':').ok_or_else(|| {
            Error::Other(format!(
                "filesystem rule {s:?} is not 'read:path' or 'write:path'"
            ))
        })?;
        let access = match mode {
            "read" => Access::Read,
            "write" => Access::Write,
            other => {
                return Err(Error::Other(format!(
                    "filesystem rule {s:?} has unknown mode {other:?}"
                )))
            }
        };
        Ok(FsRule {
            access,
            prefix: normalise(Path::new(path)),
        })
    }

    pub fn to_string_form(&self) -> String {
        format!("{}:{}", self.access.label(), self.prefix.display())
    }

    /// A write grant implies the ability to read what was written; a read
    /// grant never implies a write.
    fn satisfies(&self, want: Access, path: &Path) -> bool {
        let permitted =
            self.access == want || (self.access == Access::Write && want == Access::Read);
        permitted && path.starts_with(&self.prefix)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Capabilities {
    #[serde(default)]
    pub network: bool,
    #[serde(default)]
    pub filesystem: Vec<String>,
    #[serde(default)]
    pub env_read: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_memory_mb: Option<u64>,
    /// Anything a specific runtime cares about and the spine does not.
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Capabilities {
    /// The safe default: a file that declares nothing gets nothing.
    pub fn none() -> Self {
        Capabilities::default()
    }

    pub fn rules(&self) -> Result<Vec<FsRule>> {
        self.filesystem.iter().map(|s| FsRule::parse(s)).collect()
    }

    pub fn decode(chunk: &Chunk) -> Result<Self> {
        Ok(serde_json::from_slice(&chunk.value)?)
    }

    pub fn encode(&self) -> Result<Chunk> {
        Ok(Chunk::new(ChunkType::CAPS, serde_json::to_vec(self)?))
    }

    /// Check the declaration itself, before anyone acts on it. A malformed
    /// or suspiciously broad capability header is worth surfacing at
    /// validate time rather than at the moment a runtime denies something.
    pub fn lint(&self) -> Vec<Issue> {
        let mut out = Vec::new();
        for s in &self.filesystem {
            match FsRule::parse(s) {
                Err(e) => out.push(Issue::error("filesystem", e.to_string())),
                Ok(rule) => {
                    if rule.prefix.as_os_str().is_empty() || rule.prefix == Path::new("/") {
                        out.push(Issue::warning(
                            "filesystem",
                            format!(
                                "{s:?} grants {} access to the whole filesystem",
                                rule.access.label()
                            ),
                        ));
                    }
                    // normalise() cannot resolve a leading `..` without
                    // knowing the working directory, so a prefix that
                    // escapes upward is unbounded in practice.
                    if rule
                        .prefix
                        .components()
                        .next()
                        .is_some_and(|c| c == Component::ParentDir)
                    {
                        out.push(Issue::warning(
                            "filesystem",
                            format!("{s:?} points outside the working directory"),
                        ));
                    }
                }
            }
        }
        if self.env_read.iter().any(|e| e == "*") {
            out.push(Issue::warning(
                "env_read",
                "'*' exports the whole environment, including anything secret in it",
            ));
        }
        if self.network && self.filesystem.iter().any(|f| f.starts_with("read:")) {
            out.push(Issue::note(
                "network",
                "network plus filesystem read is the shape of an exfiltration path; \
                 worth confirming both are needed",
            ));
        }
        out
    }

    /// Everything this declares that `policy` does not permit. Empty means
    /// the runtime can grant the request in full.
    pub fn exceeds(&self, policy: &Capabilities) -> Vec<Issue> {
        let mut out = Vec::new();
        if self.network && !policy.network {
            out.push(Issue::error(
                "network",
                "file wants network, policy forbids it",
            ));
        }
        let allowed = policy.rules().unwrap_or_default();
        for want in self.rules().unwrap_or_default() {
            if !allowed
                .iter()
                .any(|a| a.satisfies(want.access, &want.prefix))
            {
                out.push(Issue::error(
                    "filesystem",
                    format!("policy does not permit {}", want.to_string_form()),
                ));
            }
        }
        for e in &self.env_read {
            if !policy.env_read.iter().any(|p| p == "*" || p == e) {
                out.push(Issue::error(
                    "env_read",
                    format!("policy does not permit reading ${e}"),
                ));
            }
        }
        if let (Some(want), Some(cap)) = (self.max_memory_mb, policy.max_memory_mb) {
            if want > cap {
                out.push(Issue::error(
                    "max_memory_mb",
                    format!("file wants {want} MB, policy caps at {cap} MB"),
                ));
            }
        }
        out
    }

    /// Turn a declaration into something a runtime can ask questions of.
    pub fn grant(&self) -> Result<Grant> {
        Ok(Grant {
            network: self.network,
            rules: self.rules()?,
            env_read: self.env_read.clone(),
            max_memory_mb: self.max_memory_mb,
        })
    }
}

/// The enforcement side. Every question is asked of the grant, never of the
/// declaration, so there is exactly one place path normalisation happens.
#[derive(Clone, Debug)]
pub struct Grant {
    network: bool,
    rules: Vec<FsRule>,
    env_read: Vec<String>,
    max_memory_mb: Option<u64>,
}

impl Grant {
    pub fn denied() -> Self {
        Grant {
            network: false,
            rules: Vec::new(),
            env_read: Vec::new(),
            max_memory_mb: Some(0),
        }
    }

    pub fn allows_network(&self) -> bool {
        self.network
    }

    pub fn allows(&self, access: Access, path: impl AsRef<Path>) -> bool {
        let p = normalise(path.as_ref());
        self.rules.iter().any(|r| r.satisfies(access, &p))
    }

    pub fn allows_env(&self, name: &str) -> bool {
        self.env_read.iter().any(|e| e == name)
    }

    pub fn memory_limit_mb(&self) -> Option<u64> {
        self.max_memory_mb
    }
}

/// Collapse `.` and resolve `..` lexically, without touching the filesystem.
///
/// Lexical is the right choice here: resolving symlinks would make the answer
/// depend on the state of the disk at check time, and a capability check that
/// changes its answer between the check and the use is the classic TOCTOU
/// bug. A runtime that needs symlink safety opens with `O_NOFOLLOW` instead.
fn normalise(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps() -> Capabilities {
        Capabilities {
            network: false,
            filesystem: vec!["read:./data".into(), "write:./out".into()],
            env_read: vec!["HOME".into()],
            max_memory_mb: Some(256),
            extra: Default::default(),
        }
    }

    #[test]
    fn grants_are_prefix_scoped() {
        let g = caps().grant().unwrap();
        assert!(g.allows(Access::Read, "data/rows.csv"));
        assert!(g.allows(Access::Read, "./data/nested/deep.csv"));
        assert!(!g.allows(Access::Read, "secrets/key.pem"));
        assert!(!g.allows(Access::Write, "data/rows.csv"));
        assert!(g.allows(Access::Write, "out/report.emd"));
    }

    #[test]
    fn traversal_cannot_escape_a_prefix() {
        let g = caps().grant().unwrap();
        assert!(!g.allows(Access::Read, "data/../../etc/passwd"));
        assert!(g.allows(Access::Read, "data/sub/../rows.csv"));
    }

    #[test]
    fn a_write_grant_implies_read_but_not_the_reverse() {
        let g = caps().grant().unwrap();
        assert!(g.allows(Access::Read, "out/report.emd"));
        assert!(!g.allows(Access::Write, "data/rows.csv"));
    }

    #[test]
    fn nothing_is_granted_by_default() {
        let g = Capabilities::none().grant().unwrap();
        assert!(!g.allows_network());
        assert!(!g.allows(Access::Read, "anything"));
        assert!(!g.allows_env("HOME"));
    }

    #[test]
    fn policy_violations_are_itemised() {
        let file = Capabilities {
            network: true,
            filesystem: vec!["write:/".into()],
            env_read: vec!["AWS_SECRET_ACCESS_KEY".into()],
            max_memory_mb: Some(4096),
            extra: Default::default(),
        };
        let policy = caps();
        let v = file.exceeds(&policy);
        assert_eq!(v.len(), 4);
        assert!(v.iter().any(|i| i.path == "network"));
        assert!(v.iter().any(|i| i.path == "max_memory_mb"));
    }

    #[test]
    fn lint_flags_a_root_grant() {
        let wide = Capabilities {
            filesystem: vec!["write:/".into()],
            ..Default::default()
        };
        assert!(wide
            .lint()
            .iter()
            .any(|i| i.message.contains("whole filesystem")));
    }

    #[test]
    fn survives_a_chunk_round_trip() {
        let c = caps();
        let back = Capabilities::decode(&c.encode().unwrap()).unwrap();
        assert_eq!(back.filesystem, c.filesystem);
        assert_eq!(back.max_memory_mb, Some(256));
    }
}
