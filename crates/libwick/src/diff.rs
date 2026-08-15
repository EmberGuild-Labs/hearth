//! Structural diff over the chunk tree.
//!
//! This is the generic half of "semantically diffable". It knows nothing
//! about text, tables or images — only that a payload is a tree of chunks —
//! and it reports which chunks were added, removed, changed or moved. A
//! format plugin refines that into `port: 5432 -> 5433` or `tile (3,7)
//! repainted`; when a plugin has nothing better to say, this is what runs.
//!
//! Matching is by content hash first, then by position. Hash-first is what
//! makes a move report as a move: inserting a paragraph at the top of a
//! document shifts every following chunk, and a purely positional diff would
//! call that a rewrite of the entire file — exactly the line-noise the format
//! exists to avoid.

use crate::chunks::{ChunkList, ChunkType};
use crate::crypto::KeyRing;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChangeKind {
    Added,
    Removed,
    Modified,
    /// Same content, different position.
    Moved,
}

impl ChangeKind {
    pub fn sigil(self) -> char {
        match self {
            ChangeKind::Added => '+',
            ChangeKind::Removed => '-',
            ChangeKind::Modified => '~',
            ChangeKind::Moved => '>',
        }
    }
}

#[derive(Clone, Debug)]
pub struct Change {
    pub kind: ChangeKind,
    /// Address within the tree: `DATA/SECT[3]`.
    pub path: String,
    pub ty: ChunkType,
    /// Human-readable detail. Plugins fill this with meaning; the structural
    /// pass fills it with sizes.
    pub note: String,
}

impl Change {
    pub fn new(
        kind: ChangeKind,
        path: impl Into<String>,
        ty: ChunkType,
        note: impl Into<String>,
    ) -> Self {
        Change {
            kind,
            path: path.into(),
            ty,
            note: note.into(),
        }
    }
}

impl std::fmt::Display for Change {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.note.is_empty() {
            write!(f, "{} {}", self.kind.sigil(), self.path)
        } else {
            write!(f, "{} {}  {}", self.kind.sigil(), self.path, self.note)
        }
    }
}

/// Above this many chunks per side, the O(n·m) alignment is skipped in
/// favour of a positional walk. A million-cell table is the case that
/// matters: the exact answer is not worth ten seconds of DP, and a
/// positional diff of two row-group lists is still correct, just less
/// clever about insertions.
const LCS_LIMIT: usize = 2_000;

/// Diff two top-level chunk tables, recursing into nested lists.
///
/// The summary tier is skipped. `SUMM` is derived from `DATA`, so reporting
/// it alongside the change it was derived from tells the reader the same
/// thing twice — "the word count went from 3 to 4" is not a second edit.
/// A summary that has fallen out of step with its payload is a real problem,
/// but it is a validation problem, not a diff.
pub fn structural(a: &ChunkList, b: &ChunkList, keys: &KeyRing) -> Vec<Change> {
    structural_all(&without_derived(a), &without_derived(b), keys)
}

/// Every chunk, summary tier included. For inspecting a file's structure
/// rather than its content.
pub fn structural_all(a: &ChunkList, b: &ChunkList, keys: &KeyRing) -> Vec<Change> {
    let mut out = Vec::new();
    walk(a, b, "", keys, &mut out);
    out
}

fn without_derived(l: &ChunkList) -> ChunkList {
    ChunkList(
        l.iter()
            .filter(|c| c.ty != ChunkType::SUMM)
            .cloned()
            .collect(),
    )
}

fn walk(a: &ChunkList, b: &ChunkList, prefix: &str, keys: &KeyRing, out: &mut Vec<Change>) {
    let ah: Vec<[u8; 32]> = a.iter().map(fingerprint).collect();
    let bh: Vec<[u8; 32]> = b.iter().map(fingerprint).collect();

    let pairs = if a.len().max(b.len()) > LCS_LIMIT {
        positional(a.len(), b.len())
    } else {
        align(&ah, &bh)
    };

    // A chunk that the alignment dropped from one side and picked up on the
    // other, with the same content, was moved rather than deleted and
    // rewritten. The alignment cannot see this — a longest common
    // subsequence is monotonic by construction, so a genuine reorder always
    // falls out of it as one removal and one addition. Rejoining them here
    // is what lets `hearth diff` say "section 4 moved to the top" instead of
    // printing the whole section twice.
    let mut moved_from: std::collections::HashMap<usize, usize> = Default::default();
    let mut moved_to: std::collections::HashMap<usize, usize> = Default::default();
    {
        let mut spare: Vec<usize> = pairs
            .iter()
            .filter_map(|p| match p {
                Pair::OnlyB(j) => Some(*j),
                _ => None,
            })
            .collect();
        for p in &pairs {
            let Pair::OnlyA(i) = p else { continue };
            if let Some(pos) = spare.iter().position(|&j| bh[j] == ah[*i]) {
                let j = spare.remove(pos);
                moved_from.insert(*i, j);
                moved_to.insert(j, *i);
            }
        }
    }

    for pair in &pairs {
        let pair = *pair;
        match pair {
            Pair::Both(i, j, in_order) => {
                let (ca, cb) = (&a.0[i], &b.0[j]);
                let path = addr(prefix, cb.ty, j);
                if ah[i] == bh[j] {
                    // Identical content that the alignment could not keep in
                    // sequence is a genuine reorder. Identical content that
                    // merely shifted because something was inserted above it
                    // is not, and reporting it would recreate the line-noise
                    // this diff exists to avoid.
                    if !in_order {
                        out.push(Change::new(
                            ChangeKind::Moved,
                            path,
                            cb.ty,
                            format!("position {i} -> {j}"),
                        ));
                    }
                    continue;
                }
                // Different content in the same slot. If both sides are
                // themselves chunk lists, the interesting answer is one
                // level down.
                match (ca.as_list(keys), cb.as_list(keys)) {
                    (Ok(la), Ok(lb)) if is_container(ca.ty) && !la.is_empty() => {
                        walk(&la, &lb, &path, keys, out);
                    }
                    _ => out.push(Change::new(
                        ChangeKind::Modified,
                        path,
                        cb.ty,
                        format!("{} -> {} bytes", ca.value.len(), cb.value.len()),
                    )),
                }
            }
            Pair::OnlyA(i) => match moved_from.get(&i) {
                Some(&j) => out.push(Change::new(
                    ChangeKind::Moved,
                    addr(prefix, b.0[j].ty, j),
                    b.0[j].ty,
                    format!("position {i} -> {j}"),
                )),
                None => out.push(Change::new(
                    ChangeKind::Removed,
                    addr(prefix, a.0[i].ty, i),
                    a.0[i].ty,
                    format!("{} bytes", a.0[i].value.len()),
                )),
            },
            // Already reported from the removal side.
            Pair::OnlyB(j) if moved_to.contains_key(&j) => {}
            Pair::OnlyB(j) => out.push(Change::new(
                ChangeKind::Added,
                addr(prefix, b.0[j].ty, j),
                b.0[j].ty,
                format!("{} bytes", b.0[j].value.len()),
            )),
        }
    }
}

/// Recurse into these, because their values are chunk lists by definition.
/// Everything else is a leaf as far as the spine is concerned.
fn is_container(ty: ChunkType) -> bool {
    ty == ChunkType::DATA || ty == ChunkType::SUMM
}

fn addr(prefix: &str, ty: ChunkType, index: usize) -> String {
    if prefix.is_empty() {
        format!("{ty}[{index}]")
    } else {
        format!("{prefix}/{ty}[{index}]")
    }
}

fn fingerprint(c: &crate::chunks::Chunk) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&c.ty.0);
    h.update(&c.value);
    *h.finalize().as_bytes()
}

#[derive(Clone, Copy)]
enum Pair {
    /// Two chunks the alignment paired up. The flag records whether the pair
    /// came from the common subsequence — meaning both sides agree on its
    /// ordering — or from the positional fill-in afterwards.
    Both(usize, usize, bool),
    OnlyA(usize),
    OnlyB(usize),
}

fn positional(n: usize, m: usize) -> Vec<Pair> {
    let mut out = Vec::new();
    for i in 0..n.min(m) {
        out.push(Pair::Both(i, i, true));
    }
    for i in m..n {
        out.push(Pair::OnlyA(i));
    }
    for j in n..m {
        out.push(Pair::OnlyB(j));
    }
    out
}

/// Longest common subsequence on fingerprints, then pair up whatever is left
/// over positionally so that an edited chunk reports as one modification
/// rather than a delete and an insert.
fn align(a: &[[u8; 32]], b: &[[u8; 32]]) -> Vec<Pair> {
    let (n, m) = (a.len(), b.len());
    let mut dp = vec![0u32; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[at(i, j)] = if a[i] == b[j] {
                dp[at(i + 1, j + 1)] + 1
            } else {
                dp[at(i + 1, j)].max(dp[at(i, j + 1)])
            };
        }
    }

    // Walk the table, collecting matched pairs and the gaps between them.
    let mut out = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    let mut pending_a: Vec<usize> = Vec::new();
    let mut pending_b: Vec<usize> = Vec::new();

    let flush = |pa: &mut Vec<usize>, pb: &mut Vec<usize>, out: &mut Vec<Pair>| {
        let common = pa.len().min(pb.len());
        for k in 0..common {
            out.push(Pair::Both(pa[k], pb[k], false));
        }
        for &x in &pa[common..] {
            out.push(Pair::OnlyA(x));
        }
        for &y in &pb[common..] {
            out.push(Pair::OnlyB(y));
        }
        pa.clear();
        pb.clear();
    };

    while i < n && j < m {
        if a[i] == b[j] {
            flush(&mut pending_a, &mut pending_b, &mut out);
            out.push(Pair::Both(i, j, true));
            i += 1;
            j += 1;
        } else if dp[at(i + 1, j)] >= dp[at(i, j + 1)] {
            pending_a.push(i);
            i += 1;
        } else {
            pending_b.push(j);
            j += 1;
        }
    }
    pending_a.extend(i..n);
    pending_b.extend(j..m);
    flush(&mut pending_a, &mut pending_b, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunks::Chunk;

    fn sect(s: &str) -> Chunk {
        Chunk::text(ChunkType::new(b"SECT"), s)
    }

    fn data(parts: &[&str]) -> ChunkList {
        let keys = KeyRing::empty();
        let inner = ChunkList(parts.iter().map(|s| sect(s)).collect());
        ChunkList(vec![Chunk::list(ChunkType::DATA, &inner, &keys).unwrap()])
    }

    #[test]
    fn identical_trees_produce_nothing() {
        let keys = KeyRing::empty();
        let a = data(&["one", "two", "three"]);
        assert!(structural(&a, &a, &keys).is_empty());
    }

    #[test]
    fn an_edit_is_one_modification() {
        let keys = KeyRing::empty();
        let a = data(&["one", "two", "three"]);
        let b = data(&["one", "TWO", "three"]);
        let d = structural(&a, &b, &keys);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, ChangeKind::Modified);
        assert_eq!(d[0].path, "DATA[0]/SECT[1]");
    }

    #[test]
    fn an_insertion_does_not_rewrite_everything_after_it() {
        let keys = KeyRing::empty();
        let a = data(&["one", "two", "three"]);
        let b = data(&["zero", "one", "two", "three"]);
        let d = structural(&a, &b, &keys);
        assert_eq!(d.len(), 1, "expected one addition, got {d:?}");
        assert_eq!(d[0].kind, ChangeKind::Added);
    }

    #[test]
    fn a_reorder_reports_as_a_move() {
        let keys = KeyRing::empty();
        let a = data(&["one", "two", "three"]);
        let b = data(&["three", "one", "two"]);
        let d = structural(&a, &b, &keys);
        assert!(d.iter().any(|c| c.kind == ChangeKind::Moved));
        assert!(!d.iter().any(|c| c.kind == ChangeKind::Modified));
    }

    #[test]
    fn a_deletion_is_reported_once() {
        let keys = KeyRing::empty();
        let a = data(&["one", "two", "three"]);
        let b = data(&["one", "three"]);
        let d = structural(&a, &b, &keys);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, ChangeKind::Removed);
    }

    #[test]
    fn top_level_chunks_are_compared_too() {
        let keys = KeyRing::empty();
        let a = data(&["one"]);
        let mut b = data(&["one"]);
        b.push(Chunk::text(ChunkType::SCHM, "{}"));
        let d = structural(&a, &b, &keys);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].ty, ChunkType::SCHM);
        assert_eq!(d[0].kind, ChangeKind::Added);
    }
}
