//! Tree objects, and the paths one commit changed.
//!
//! The other object readers here work on text. A tree is binary — a run of
//! `<mode> <name>\0<20-byte oid>` — and it is the only object whose *shape*
//! has to be understood rather than read, which is why it sits apart.
//!
//! ## What makes this affordable
//!
//! A diff of two trees recurses **only where a subtree's oid differs**. Git
//! names a tree by the hash of its contents, so an unchanged directory is one
//! oid comparison no matter how many files it holds: the cost of a commit is
//! the size of what it changed, not the size of the repository. That, and
//! [`Trees`] keeping each tree parsed for the whole pass, is what lets a file
//! list be built for a history of any length.
//!
//! ## Merges
//!
//! A path is recorded for a merge only when it differs from **every** parent.
//! That is git's own rule (`TREESAME`), and it answers the question the
//! property exists for: an ordinary merge brings in changes that its own side
//! commits already account for, so attributing them to the merge as well would
//! make every file look touched by every merge. What survives the rule is the
//! conflict resolution — content that appears in no parent and so is genuinely
//! the merge's own.

use std::collections::{BTreeMap, BTreeSet};

use crate::Files;
use crate::odb::{Kind, Odb};

/// How much one commit's diff may spend, so a pathological tree cannot hang a
/// digest or grow a property without bound.
///
/// Saturating throughout: a budget that wrapped would turn into an enormous
/// one, which is the opposite of a limit.
struct Budget {
    /// Objects still allowed to be read.
    reads: u32,
    /// Paths still allowed to be recorded.
    paths: u32,
    /// Whether either ran out, so a caller can say the list is partial rather
    /// than let it read as complete.
    spent: bool,
}

impl Budget {
    fn new() -> Budget {
        Budget {
            reads: 50_000,
            paths: 4_096,
            spent: false,
        }
    }

    /// Takes one read, or reports that there is none left.
    fn read(&mut self) -> bool {
        match self.reads {
            0 => {
                self.spent = true;
                false
            }
            n => {
                self.reads = n.saturating_sub(1);
                true
            }
        }
    }

    /// Takes one path, or reports that there is none left.
    fn path(&mut self) -> bool {
        match self.paths {
            0 => {
                self.spent = true;
                false
            }
            n => {
                self.paths = n.saturating_sub(1);
                true
            }
        }
    }
}

/// A directory nested deeper than this is not followed. Far past anything a
/// repository does on purpose, and it bounds the recursion regardless of what
/// the object store claims.
const MAX_DEPTH: u32 = 64;

/// Past this many trees the cache starts over. A few thousand entries is a
/// handful of megabytes, cheaper than re-reading them and small enough not to
/// matter in a sandbox.
const MAX_CACHED: usize = 4_096;

/// One entry of a tree: what it points at, and whether that is another tree.
#[derive(Clone)]
struct Entry {
    oid: String,
    is_tree: bool,
}

/// A tree object's entries by name.
///
/// Keyed rather than kept in file order: git's own ordering puts a directory
/// where its name *with* a trailing slash would sort, so two trees walked side
/// by side cannot be assumed to step in step. Comparing by name sidesteps the
/// question — and a tree is small, so the map costs nothing worth saving.
fn parse_tree(data: &[u8]) -> BTreeMap<String, Entry> {
    let mut out = BTreeMap::new();
    let mut rest = data;
    while !rest.is_empty() {
        let Some(space) = rest.iter().position(|b| *b == b' ') else {
            break;
        };
        let mode = &rest[..space];
        let after = &rest[space.saturating_add(1)..];
        let Some(nul) = after.iter().position(|b| *b == 0) else {
            break;
        };
        let name = String::from_utf8_lossy(&after[..nul]).into_owned();
        let oid_at = nul.saturating_add(1);
        // A truncated final entry is a corrupt object, not a parse to guess at.
        if after.len() < oid_at.saturating_add(20) {
            break;
        }
        let oid = crate::odb::oid_to_hex(&after[oid_at..oid_at.saturating_add(20)]);
        // `40000` is a directory. Every other mode — a file, a symlink, or a
        // submodule's gitlink — is a leaf as far as a path list is concerned.
        let is_tree = mode == b"40000";
        out.insert(name, Entry { oid, is_tree });
        rest = &after[oid_at.saturating_add(20)..];
    }
    out
}

/// Trees kept parsed, across every commit of one pass.
///
/// A history reads each tree at least twice — once as a commit's own, once as
/// its child's parent — and a root tree once more for every child a merge
/// gives it. Inflating and parsing the same bytes each time is most of what
/// this pass would cost, and it is pure repetition: an oid names one immutable
/// object, so a tree parsed once is correct for the whole walk.
///
/// Bounded, and cleared rather than evicted one at a time. Entries are worth
/// the same as each other within a pass, so the hit rate is what matters and
/// an LRU's bookkeeping would buy nothing a repository could notice.
pub struct Trees {
    parsed: BTreeMap<String, BTreeMap<String, Entry>>,
}

impl Trees {
    /// An empty cache.
    pub fn new() -> Trees {
        Trees {
            parsed: BTreeMap::new(),
        }
    }
}

impl Default for Trees {
    fn default() -> Trees {
        Trees::new()
    }
}

/// What one commit changed: repository-relative paths, and whether the list
/// had to stop short.
pub struct Changed {
    /// The paths, sorted and without duplicates.
    pub paths: Vec<String>,
    /// Whether a budget or the depth cap cut the walk, so the list is a subset
    /// of what the commit really touched. Recorded rather than dropped: a
    /// caller that cannot tell a short list from a complete one would answer
    /// "this commit did not touch that file" when it does not know.
    pub partial: bool,
}

/// The object store, the host, and what one pass has already learned.
///
/// A struct rather than a long parameter list: the walk and the diff recurse,
/// and threading five borrowed things plus a prefix and a depth through each
/// call is how a signature becomes unreadable.
pub struct Walker<'a> {
    odb: &'a mut Odb,
    files: &'a dyn Files,
    trees: Trees,
    budget: Budget,
}

impl<'a> Walker<'a> {
    /// A walker over one repository, with an empty cache.
    pub fn new(odb: &'a mut Odb, files: &'a dyn Files) -> Walker<'a> {
        Walker {
            odb,
            files,
            trees: Trees::new(),
            budget: Budget::new(),
        }
    }

    /// Reads a tree, or an empty one when it cannot be read.
    ///
    /// An absent object is treated as empty rather than fatal: a shallow clone
    /// or a partial fetch genuinely does not have every tree, and refusing to
    /// describe the history that *is* present would be the worse answer.
    fn read(&mut self, oid: Option<&str>) -> BTreeMap<String, Entry> {
        let Some(oid) = oid else {
            return BTreeMap::new();
        };
        if let Some(hit) = self.trees.parsed.get(oid) {
            return hit.clone();
        }
        if !self.budget.read() {
            return BTreeMap::new();
        }
        let parsed = match self.odb.read(self.files, oid) {
            Some(object) if object.kind == Kind::Tree => parse_tree(&object.data),
            _ => BTreeMap::new(),
        };
        if self.trees.parsed.len() >= MAX_CACHED {
            self.trees.parsed.clear();
        }
        self.trees.parsed.insert(oid.to_string(), parsed.clone());
        parsed
    }

    /// Every path under one tree — what an added or deleted directory
    /// contributes.
    fn walk(&mut self, oid: Option<&str>, prefix: &str, depth: u32, out: &mut BTreeSet<String>) {
        if depth >= MAX_DEPTH {
            self.budget.spent = true;
            return;
        }
        for (name, entry) in self.read(oid) {
            let path = join(prefix, &name);
            match entry.is_tree {
                true => self.walk(Some(&entry.oid), &path, depth.saturating_add(1), out),
                false => {
                    if self.budget.path() {
                        out.insert(path);
                    }
                }
            }
        }
    }

    /// The paths that differ between two trees, either of which may be absent.
    fn diff(
        &mut self,
        sides: (Option<&str>, Option<&str>),
        at: (&str, u32),
        out: &mut BTreeSet<String>,
    ) {
        let (from, to) = sides;
        let (prefix, depth) = at;
        // Identical trees are identical contents — the comparison that makes
        // this cheap, and it holds at the root as much as inside.
        if from == to {
            return;
        }
        if depth >= MAX_DEPTH {
            self.budget.spent = true;
            return;
        }
        let old = self.read(from);
        let new = self.read(to);
        let names: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
        let names: Vec<String> = names.into_iter().cloned().collect();
        for name in names {
            let path = join(prefix, &name);
            let next = depth.saturating_add(1);
            match (old.get(&name), new.get(&name)) {
                // Both sides have it, as the same object: nothing below moved.
                (Some(a), Some(b)) if a.oid == b.oid && a.is_tree == b.is_tree => {}
                (Some(a), Some(b)) if a.is_tree && b.is_tree => {
                    let (a, b) = (a.oid.clone(), b.oid.clone());
                    self.diff((Some(&a), Some(&b)), (&path, next), out);
                }
                // A path that was a file and is now a directory, or the
                // reverse. Both sides are listed: the file changed, and so did
                // everything the directory holds.
                (Some(a), Some(b)) => {
                    for side in [a.clone(), b.clone()] {
                        self.leaf_or_tree(&side, &path, next, out);
                    }
                }
                (Some(one), None) | (None, Some(one)) => {
                    let one = one.clone();
                    self.leaf_or_tree(&one, &path, next, out);
                }
                (None, None) => {}
            }
        }
    }

    /// Record a leaf, or everything under a subtree.
    fn leaf_or_tree(&mut self, entry: &Entry, path: &str, depth: u32, out: &mut BTreeSet<String>) {
        match entry.is_tree {
            true => self.walk(Some(&entry.oid), path, depth, out),
            false => {
                if self.budget.path() {
                    out.insert(path.to_string());
                }
            }
        }
    }

    /// The paths a commit changed, against its parents.
    ///
    /// `parents` are the parents' tree oids, in the commit's own parent order
    /// — empty for a root commit, which therefore reports its whole tree. With
    /// one parent this is that diff. With more it is the intersection of the
    /// diffs against every parent, which is git's `TREESAME` rule; the module
    /// docs say why a merge is read that way.
    pub fn changed_paths(&mut self, tree: &str, parents: &[String]) -> Changed {
        // Per commit: one enormous commit must not leave the rest of the
        // history unread, and the cache is deliberately *not* reset with it.
        self.budget = Budget::new();
        let mut sets: Vec<BTreeSet<String>> = Vec::new();
        match parents.is_empty() {
            true => {
                let mut all = BTreeSet::new();
                self.walk(Some(tree), "", 0, &mut all);
                sets.push(all);
            }
            false => {
                for parent in parents {
                    let mut one = BTreeSet::new();
                    self.diff((Some(parent.as_str()), Some(tree)), ("", 0), &mut one);
                    sets.push(one);
                }
            }
        }
        let paths = match sets.split_first() {
            Some((first, rest)) => rest.iter().fold(first.clone(), |acc, next| {
                acc.intersection(next).cloned().collect()
            }),
            None => BTreeSet::new(),
        };
        Changed {
            paths: paths.into_iter().collect(),
            partial: self.budget.spent,
        }
    }
}

/// `prefix/name`, and `name` at the root.
fn join(prefix: &str, name: &str) -> String {
    match prefix.is_empty() {
        true => name.to_string(),
        false => format!("{prefix}/{name}"),
    }
}
