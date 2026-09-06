//! A repository's history → graph facts, read straight out of `.git`.
//!
//! Every other drsg plugin reads *source*: a tree of files, as they are now.
//! This one reads the second source of truth sitting beside that tree — who
//! changed what, in what order, on which branch, and which of those commits
//! were later rewritten. None of it is visible to a walk of the working tree,
//! and all of it is exact: a commit's parents are not inferred, they are
//! written down.
//!
//! ## What it produces
//!
//! | Node | Key | What it is |
//! |---|---|---|
//! | `Commit` (also `Merge`) | `commit:<sha>` | one commit; `Merge` when it has two or more parents |
//! | `Branch` (also `Remote`) | `branch:<refname>` | a branch, local or remote-tracking |
//! | `Tag` | `tag:<refname>` | a tag, annotated or lightweight |
//! | `Rebase` | `rebase:<ref>@<time>` | one replay, reconstructed from the reflog |
//!
//! | Edge | Between | Says |
//! |---|---|---|
//! | `PARENT` (`order`) | commit → commit | the whole merge structure: `order = 1` is the line the commit was made on, the rest is what a merge brought in |
//! | `TIP` | branch → commit | where the branch points |
//! | `TAGS` | tag → commit | what the tag names |
//! | `ONTO` | rebase → commit | what the replay was based on |
//! | `REPLACED` | rebase → commit | the tip it rewrote away |
//! | `PRODUCED` (`step`, `kind`) | rebase → commit | each commit it wrote |
//! | `RESULT` | rebase → commit | the tip the branch ended on |
//! | `ON` | rebase → branch | the branch it ran on |
//!
//! ## The one thing that cannot be exact
//!
//! **Rebases are not in the commit graph at all.** A rebase writes new commits
//! and moves a ref; nothing in any object records that one commit replaced
//! another. The only record is the reflog, which is local to a single clone
//! and expires — so a rebase older than `gc.reflogExpire` (90 days by default)
//! left no trace anywhere. Everything under `Rebase` is qualified by that, and
//! it is said in the report rather than left for a reader to discover.
//!
//! The reflog is also why commits no branch can reach are kept: the commits a
//! rebase abandoned are still in the object store, and a graph showing the
//! rewritten branch without what it replaced would answer "what did this
//! rebase change?" with silence. Those commits carry `reachable: false`.

use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub mod object;
pub mod odb;
pub mod reflog;
pub mod refs;

#[cfg(test)]
mod tests;

use object::{Commit, parse_commit, parse_tag};
use odb::{Kind, Odb};
use refs::{Head, RawRef};

/// What the parser reads through — the plugin contract's host, as one small
/// trait so the tests can hand in a plain directory.
///
/// The paths are relative to the **git directory**: `HEAD`, `refs/heads/main`,
/// `objects/pack/pack-….idx`. That is the whole grant this plugin is given —
/// tighter than the tree every other plugin sees, because history is all it
/// reads and the working tree is none of its business.
pub trait Files {
    /// Readable paths ending with `suffix` (`""` for all), sorted.
    fn list(&self, suffix: &str) -> Result<Vec<String>, String>;
    fn read(&self, path: &str) -> Result<Vec<u8>, String>;
}

/// A property map: JSON object entries, exactly as the contract carries them.
pub type Props = serde_json::Map<String, Value>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub key: String,
    pub label: String,
    pub extra_labels: Vec<String>,
    pub props: Props,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub src: String,
    pub dst: String,
    pub ty: String,
    pub props: Props,
}

/// The facts, and an account of what could not be read.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct History {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    /// Objects a ref or a reflog named that the object store no longer holds.
    pub skipped: usize,
    pub notes: Vec<String>,
    pub summary: Summary,
}

/// The counts a caller reports back to a human.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub commits: usize,
    pub merges: usize,
    /// Commits no branch or tag can reach — what a rebase or a reset left
    /// behind, found through the reflog and kept.
    pub unreachable: usize,
    pub branches: usize,
    pub remote_branches: usize,
    pub tags: usize,
    pub rebases: usize,
    /// Where `HEAD` is, in words.
    pub head: String,
}

/// `[plugins.git]` — what to read, and how much of it.
#[derive(Debug, Clone)]
pub struct Options {
    /// Ceiling on commits, newest first; `0` reads the whole history.
    ///
    /// A ceiling exists because history has no natural size: a mature kernel
    /// tree is over a million commits, and quietly spending an hour on one
    /// would be a worse default than a stated limit. When it bites, the report
    /// says so and names the setting — a truncated graph must never look like
    /// a complete one.
    pub max_commits: usize,
    /// Read the reflog: rebases, and the commits they abandoned.
    pub reflog: bool,
    /// Include remote-tracking branches.
    pub remotes: bool,
    /// Include tags.
    pub tags: bool,
    /// Keep each commit message's body, not just its first line.
    pub body: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            max_commits: 20_000,
            reflog: true,
            remotes: true,
            tags: true,
            body: true,
        }
    }
}

impl Options {
    /// Read `[plugins.git]`, as the host hands every plugin its own settings.
    ///
    /// An unknown key is an error rather than a shrug: a misspelled
    /// `max_commit` that silently kept the default would surface much later as
    /// a truncated graph nobody asked for.
    pub fn from_settings(settings: &[(String, String)]) -> Result<Options, String> {
        let mut opts = Options::default();
        for (key, value) in settings {
            match key.as_str() {
                "max_commits" => {
                    opts.max_commits = value.trim().parse().map_err(|_| {
                        format!("[plugins.git] max_commits = `{value}`: not a number")
                    })?
                }
                "reflog" => opts.reflog = flag(key, value)?,
                "remotes" => opts.remotes = flag(key, value)?,
                "tags" => opts.tags = flag(key, value)?,
                "body" => opts.body = flag(key, value)?,
                other => {
                    return Err(format!(
                        "[plugins.git] has no setting `{other}` (known: max_commits, \
                         reflog, remotes, tags, body)"
                    ));
                }
            }
        }
        Ok(opts)
    }
}

fn flag(key: &str, value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        other => Err(format!(
            "[plugins.git] {key} = `{other}`: expected true or false"
        )),
    }
}

// ---- the read ------------------------------------------------------------

pub fn commit_key(oid: &str) -> String {
    format!("commit:{oid}")
}

pub fn branch_key(refname: &str) -> String {
    format!("branch:{refname}")
}

pub fn tag_key(refname: &str) -> String {
    format!("tag:{refname}")
}

/// Read a git directory's history.
///
/// One listing, then everything else by name: refs and reflogs are small text
/// files, and commits are walked from the tips rather than by enumerating the
/// object store — so a repository with a hundred thousand loose objects costs
/// what its *history* costs, not what its content does.
pub fn read_history(files: &dyn Files, opts: &Options) -> Result<History, String> {
    let listing = files.list("")?;
    let mut out = History::default();

    let all_refs = refs::read_refs(files, &listing);
    let head = refs::read_head(files, &all_refs);
    let logs = if opts.reflog {
        reflog::read_logs(files, &listing)
    } else {
        BTreeMap::new()
    };

    let mut odb = Odb::open(files, &listing);

    let (branches, tags) = collect_refs(files, &mut odb, &all_refs, opts);

    // ---- which commits to read -------------------------------------------
    // The tips of everything: refs first, so a truncated read keeps what a
    // branch can still reach in preference to what only a reflog remembers.
    let mut tips: Vec<String> = branches.iter().map(|(r, _)| r.oid.clone()).collect();
    tips.extend(tags.iter().map(|(_, target, _)| target.clone()));
    if let Some(Head::Detached(oid)) = &head {
        tips.push(oid.clone());
    }
    let ref_tips: BTreeSet<String> = tips.iter().cloned().collect();
    for entries in logs.values() {
        for e in entries {
            tips.push(e.old.clone());
            tips.push(e.new.clone());
        }
    }

    let walk = walk_commits(files, &mut odb, &tips, opts.max_commits);
    out.skipped = walk.missing.len();
    out.notes.append(&mut odb.notes);

    if walk.truncated {
        out.notes.push(format!(
            "read the newest {} commit(s) — the ceiling `[plugins.git] max_commits` \
             sets. Anything older is not in this plane, and the `PARENT` edges \
             leading to it were dropped",
            opts.max_commits
        ));
    }
    if !walk.missing.is_empty() {
        out.notes.push(format!(
            "{} commit(s) a ref or a reflog names are not in the object store — \
             garbage-collected after a rewrite, or borrowed from a repository \
             this plugin cannot reach",
            walk.missing.len()
        ));
    }

    // Reachability is a question about the graph just read, not another walk
    // of the object store: everything needed to answer it is in hand.
    let reachable = reachable_from(&walk, &ref_tips);

    // Edges whose far end is not in this graph: a parent older than the
    // ceiling, a branch tip beyond it, a reflog entry naming a commit the
    // object store no longer holds. Counted, because an absent `TIP` would
    // otherwise look like a branch pointing at nothing on purpose.
    let mut dropped = 0usize;

    emit_commits(&mut out, &walk, &reachable, opts, &mut dropped);
    emit_refs(&mut out, &branches, &tags, &walk, &head, &mut dropped);
    if opts.reflog {
        emit_rebases(&mut out, &logs, &branches, &walk, &mut dropped);
    }

    if dropped > 0 {
        out.notes.push(format!(
            "{dropped} edge(s) name a commit outside this graph and were left \
             out — beyond the commit ceiling, or gone from the object store"
        ));
    }

    out.summary.head = match &head {
        Some(Head::OnBranch(name)) => name.trim_start_matches("refs/heads/").to_string(),
        Some(Head::Detached(oid)) => format!("detached at {}", short(oid)),
        Some(Head::Unborn(name)) => {
            format!(
                "{} (no commits yet)",
                name.trim_start_matches("refs/heads/")
            )
        }
        None => String::new(),
    };
    if walk.order.is_empty() {
        out.notes
            .push("this repository has no commits yet".to_string());
    }
    Ok(out)
}

/// A branch ref, and whether it is remote-tracking.
type Branches = Vec<(RawRef, bool)>;
/// A tag ref, the commit it peels to, and the tag object when it is annotated.
type Tags = Vec<(RawRef, String, Option<object::Tag>)>;

/// The refs worth keeping, with annotated tags peeled to the commit they name.
fn collect_refs(
    files: &dyn Files,
    odb: &mut Odb,
    all_refs: &[RawRef],
    opts: &Options,
) -> (Branches, Tags) {
    let mut branches: Vec<(RawRef, bool)> = Vec::new();
    let mut tags: Vec<(RawRef, String, Option<object::Tag>)> = Vec::new();
    for r in all_refs {
        if let Some(name) = r.name.strip_prefix("refs/tags/") {
            let _ = name;
            if !opts.tags {
                continue;
            }
            let (target, tag) = peel(files, odb, r);
            tags.push((r.clone(), target, tag));
        } else if r.name.starts_with("refs/remotes/") {
            if opts.remotes {
                branches.push((r.clone(), true));
            }
        } else if r.name.starts_with("refs/heads/") {
            branches.push((r.clone(), false));
        }
    }

    (branches, tags)
}

/// One node per commit, and a `PARENT` edge per parent that is in this graph.
fn emit_commits(
    out: &mut History,
    walk: &Walk,
    reachable: &BTreeSet<String>,
    opts: &Options,
    dropped: &mut usize,
) {
    for oid in &walk.order {
        let commit = &walk.commits[oid];
        let is_merge = commit.parents.len() > 1;
        if is_merge {
            out.summary.merges += 1;
        }
        let seen = reachable.contains(oid);
        if !seen {
            out.summary.unreachable += 1;
        }
        out.nodes.push(commit_node(commit, opts, seen));
        for (i, parent) in commit.parents.iter().enumerate() {
            if !walk.selected.contains(parent) {
                *dropped += 1;
                continue;
            }
            out.edges.push(Edge {
                src: commit_key(oid),
                dst: commit_key(parent),
                ty: "PARENT".into(),
                props: props([(
                    "order",
                    described(
                        "which parent this is, counting from 1 — the first is the \
                         line the commit was made on, the rest are what a merge \
                         brought in",
                        json!(i as i64 + 1),
                    ),
                )]),
            });
        }
    }
    out.summary.commits = walk.order.len();
}

/// Branch and tag nodes, each pointing at its commit when that commit was read.
fn emit_refs(
    out: &mut History,
    branches: &[(RawRef, bool)],
    tags: &[(RawRef, String, Option<object::Tag>)],
    walk: &Walk,
    head: &Option<Head>,
    dropped: &mut usize,
) {
    for (r, remote) in branches {
        if *remote {
            out.summary.remote_branches += 1;
        } else {
            out.summary.branches += 1;
        }
        let is_head = matches!(&head, Some(Head::OnBranch(name)) if name == &r.name);
        out.nodes.push(branch_node(r, *remote, is_head));
        if walk.selected.contains(&r.oid) {
            out.edges
                .push(edge(branch_key(&r.name), "TIP", commit_key(&r.oid)));
        } else {
            *dropped += 1;
        }
    }
    for (r, target, tag) in tags {
        out.summary.tags += 1;
        out.nodes.push(tag_node(r, target, tag.as_ref()));
        if walk.selected.contains(target) {
            out.edges
                .push(edge(tag_key(&r.name), "TAGS", commit_key(target)));
        } else {
            *dropped += 1;
        }
    }
}

/// The rebases the reflog remembers, and the caveat that goes with them.
fn emit_rebases(
    out: &mut History,
    logs: &BTreeMap<String, Vec<reflog::LogEntry>>,
    branches: &[(RawRef, bool)],
    walk: &Walk,
    dropped: &mut usize,
) {
    let runs = reflog::find_rebases(logs);
    out.summary.rebases = runs.len();
    let known: BTreeSet<&str> = branches.iter().map(|(r, _)| r.name.as_str()).collect();
    let mut used: BTreeSet<String> = BTreeSet::new();
    for run in &runs {
        // Two rebases of one branch within the same second would key
        // alike; the second gets a suffix rather than overwriting the
        // first.
        let base = format!(
            "rebase:{}@{}",
            if run.refname.is_empty() {
                "HEAD"
            } else {
                &run.refname
            },
            if run.completed {
                &run.finished_at
            } else {
                &run.started_at
            }
        );
        let mut key = base.clone();
        let mut n = 1;
        while !used.insert(key.clone()) {
            n += 1;
            key = format!("{base}#{n}");
        }
        out.nodes.push(rebase_node(&key, run));

        let mut to = |oid: &str, ty: &str| {
            if walk.selected.contains(oid) {
                out.edges.push(edge(key.clone(), ty, commit_key(oid)));
            } else if !oid.is_empty() {
                *dropped += 1;
            }
        };
        to(&run.onto, "ONTO");
        to(&run.replaced, "REPLACED");
        to(&run.result, "RESULT");
        for (i, step) in run.produced.iter().enumerate() {
            if !walk.selected.contains(&step.oid) {
                *dropped += 1;
                continue;
            }
            out.edges.push(Edge {
                src: key.clone(),
                dst: commit_key(&step.oid),
                ty: "PRODUCED".into(),
                props: props([
                    (
                        "step",
                        described(
                            "position in the replay, counting from 1",
                            json!(i as i64 + 1),
                        ),
                    ),
                    (
                        "kind",
                        described(
                            "what the reflog called this step: pick, squash, fixup, reword…",
                            json!(step.kind),
                        ),
                    ),
                ]),
            });
        }
        if known.contains(run.refname.as_str()) {
            out.edges
                .push(edge(key.clone(), "ON", branch_key(&run.refname)));
        }
    }
    out.notes.push(if logs.is_empty() {
        "this clone has no reflog, so no rebase can be seen — a fresh clone, \
         a bare repository, or `core.logAllRefUpdates` turned off"
            .to_string()
    } else {
        format!(
            "{} rebase(s) reconstructed from the reflog, which is local to this \
             clone and expires (gc.reflogExpire, 90 days by default) — a rewrite \
             older than that left no record anywhere and is not in this graph",
            runs.len()
        )
    });
}

/// An annotated tag peeled to the commit it names, and the tag object itself.
/// A tag of a tag is followed; anything deeper is left where it is rather than
/// chased forever.
fn peel(files: &dyn Files, odb: &mut Odb, r: &RawRef) -> (String, Option<object::Tag>) {
    if let Some(peeled) = &r.peeled {
        // `packed-refs` already did the work, but the tag object still holds
        // the message and the tagger.
        let tag = odb
            .read(files, &r.oid)
            .filter(|o| o.kind == Kind::Tag)
            .map(|o| parse_tag(&o.data));
        return (peeled.clone(), tag);
    }
    let mut oid = r.oid.clone();
    let mut outer = None;
    for _ in 0..3 {
        let Some(object) = odb.read(files, &oid) else {
            break;
        };
        if object.kind != Kind::Tag {
            break;
        }
        let tag = parse_tag(&object.data);
        oid = tag.object.clone();
        outer.get_or_insert(tag);
    }
    (oid, outer)
}

/// What one walk read.
struct Walk {
    /// Every commit read, including ones beyond the ceiling that were reached
    /// but not kept.
    commits: BTreeMap<String, Commit>,
    /// The commits kept, newest first.
    order: Vec<String>,
    selected: BTreeSet<String>,
    /// Oids something named that the object store does not hold.
    missing: BTreeSet<String>,
    truncated: bool,
}

/// Walk from the tips, newest first, to at most `max` commits.
///
/// Newest-first because that is what a ceiling should keep: a truncated
/// history missing last week would be useless, one missing 2011 usually is
/// not. The frontier is ordered by committer date, which is the same order
/// `git log` shows and the same thing a reader means by "newest".
fn walk_commits(files: &dyn Files, odb: &mut Odb, tips: &[String], max: usize) -> Walk {
    let mut walk = Walk {
        commits: BTreeMap::new(),
        order: Vec::new(),
        selected: BTreeSet::new(),
        missing: BTreeSet::new(),
        truncated: false,
    };
    // `(committer date, oid)` — `BinaryHeap` is a max-heap, so the newest
    // unvisited commit is always next.
    let mut frontier: BinaryHeap<(i64, String)> = BinaryHeap::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    let offer = |oid: &str,
                 walk: &mut Walk,
                 seen: &mut BTreeSet<String>,
                 frontier: &mut BinaryHeap<(i64, String)>,
                 odb: &mut Odb| {
        // The all-zero oid is git's "nothing was here": the `old` of a ref's
        // very first entry. Not missing — never existed.
        if oid.bytes().all(|b| b == b'0') || !seen.insert(oid.to_string()) {
            return;
        }
        match odb.read(files, oid).filter(|o| o.kind == Kind::Commit) {
            Some(object) => {
                let commit = parse_commit(oid, &object.data);
                frontier.push((commit.committer.ts, oid.to_string()));
                walk.commits.insert(oid.to_string(), commit);
            }
            None => {
                walk.missing.insert(oid.to_string());
            }
        }
    };

    for tip in tips {
        offer(tip, &mut walk, &mut seen, &mut frontier, odb);
    }
    while let Some((_, oid)) = frontier.pop() {
        if max > 0 && walk.order.len() >= max {
            walk.truncated = true;
            break;
        }
        walk.order.push(oid.clone());
        walk.selected.insert(oid.clone());
        let parents = walk.commits[&oid].parents.clone();
        for parent in parents {
            offer(&parent, &mut walk, &mut seen, &mut frontier, odb);
        }
    }
    walk
}

/// The commits a branch or a tag can still reach. Everything else was read
/// only because the reflog remembered it.
fn reachable_from(walk: &Walk, tips: &BTreeSet<String>) -> BTreeSet<String> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<&str> = tips
        .iter()
        .filter(|t| walk.selected.contains(*t))
        .map(|t| t.as_str())
        .collect();
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid.to_string()) {
            continue;
        }
        let Some(commit) = walk.commits.get(oid) else {
            continue;
        };
        for parent in &commit.parents {
            if walk.selected.contains(parent) && !seen.contains(parent) {
                stack.push(parent);
            }
        }
    }
    seen
}

// ---- facts ---------------------------------------------------------------

fn short(oid: &str) -> &str {
    &oid[..7.min(oid.len())]
}

/// `{"$desc": …, "$value": …}` — a property that explains itself to whoever
/// reads the graph later.
fn described(desc: &str, value: Value) -> Value {
    json!({ "$desc": desc, "$value": value })
}

fn props<const N: usize>(entries: [(&str, Value); N]) -> Props {
    entries
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

fn edge(src: String, ty: &str, dst: String) -> Edge {
    Edge {
        src,
        dst,
        ty: ty.to_string(),
        props: Props::new(),
    }
}

fn commit_node(c: &Commit, opts: &Options, reachable: bool) -> Node {
    let mut p = Props::new();
    let mut set = |k: &str, desc: &str, v: Value| {
        p.insert(k.to_string(), described(desc, v));
    };
    set("sha", "the commit's full object name", json!(c.oid));
    set(
        "short",
        "its abbreviated form, as git prints it",
        json!(short(&c.oid)),
    );
    set(
        "summary",
        "the commit message's first line",
        json!(c.summary),
    );
    if opts.body && !c.body.is_empty() {
        set(
            "body",
            "the commit message below its first line",
            json!(c.body),
        );
    }
    set("author_name", "who wrote the change", json!(c.author.name));
    set("author_email", "the author's email", json!(c.author.email));
    set(
        "authored_at",
        "when the change was written, in the author's own timezone",
        json!(c.author.iso8601()),
    );
    set(
        "authored_ts",
        "when the change was written, unix seconds — the orderable form",
        json!(c.author.ts),
    );
    set(
        "committer_name",
        "who committed it",
        json!(c.committer.name),
    );
    set(
        "committer_email",
        "the committer's email",
        json!(c.committer.email),
    );
    set(
        "committed_at",
        "when it was committed, in the committer's own timezone",
        json!(c.committer.iso8601()),
    );
    set(
        "committed_ts",
        "when it was committed, unix seconds — the orderable form",
        json!(c.committer.ts),
    );
    set(
        "parents",
        "how many parents it has; two or more is a merge",
        json!(c.parents.len() as i64),
    );
    set(
        "is_merge",
        "whether it joins two or more lines of history",
        json!(c.parents.len() > 1),
    );
    if opts.reflog {
        set(
            "reachable",
            "whether any branch or tag still leads here; false means a rewrite \
             left this commit behind and only the reflog remembers it",
            json!(reachable),
        );
    }
    Node {
        key: commit_key(&c.oid),
        label: "Commit".into(),
        // A merge is not a different kind of thing from a commit — it is a
        // commit with more history behind it — so it is a second label rather
        // than a different first one, and `Commit` still finds every commit.
        extra_labels: if c.parents.len() > 1 {
            vec!["Merge".into()]
        } else {
            Vec::new()
        },
        props: p,
    }
}

fn branch_node(r: &RawRef, remote: bool, is_head: bool) -> Node {
    let short_name = r
        .name
        .trim_start_matches("refs/heads/")
        .trim_start_matches("refs/remotes/")
        .to_string();
    let mut p = Props::new();
    let mut set = |k: &str, desc: &str, v: Value| {
        p.insert(k.to_string(), described(desc, v));
    };
    set("name", "the branch's short name", json!(short_name));
    set("ref", "its full refname", json!(r.name));
    set(
        "remote",
        "whether this is a remote-tracking branch rather than a local one",
        json!(remote),
    );
    set(
        "is_head",
        "whether this is the checked-out branch",
        json!(is_head),
    );
    set("tip", "the commit it points at", json!(r.oid));
    Node {
        key: branch_key(&r.name),
        label: "Branch".into(),
        extra_labels: if remote {
            vec!["Remote".into()]
        } else {
            Vec::new()
        },
        props: p,
    }
}

fn tag_node(r: &RawRef, target: &str, tag: Option<&object::Tag>) -> Node {
    let mut p = Props::new();
    let mut set = |k: &str, desc: &str, v: Value| {
        p.insert(k.to_string(), described(desc, v));
    };
    set(
        "name",
        "the tag's short name",
        json!(r.name.trim_start_matches("refs/tags/")),
    );
    set("ref", "its full refname", json!(r.name));
    set(
        "annotated",
        "whether this is an annotated tag — its own object, with a message and \
         a tagger — rather than a lightweight pointer at a commit",
        json!(tag.is_some()),
    );
    set("target", "the commit it names", json!(target));
    if let Some(tag) = tag {
        set("object", "the tag object's own name", json!(r.oid));
        set("message", "what the tagger wrote", json!(tag.message));
        set("tagger_name", "who made the tag", json!(tag.tagger.name));
        set("tagged_at", "when it was made", json!(tag.tagger.iso8601()));
        set(
            "tagged_ts",
            "when it was made, unix seconds",
            json!(tag.tagger.ts),
        );
    }
    Node {
        key: tag_key(&r.name),
        label: "Tag".into(),
        extra_labels: Vec::new(),
        props: p,
    }
}

fn rebase_node(key: &str, run: &reflog::Rebase) -> Node {
    let mut p = Props::new();
    let mut set = |k: &str, desc: &str, v: Value| {
        p.insert(k.to_string(), described(desc, v));
    };
    set("ref", "the branch that was rewritten", json!(run.refname));
    set(
        "branch",
        "that branch's short name",
        json!(run.refname.trim_start_matches("refs/heads/")),
    );
    set(
        "interactive",
        "whether it ran as `rebase -i`",
        json!(run.interactive),
    );
    set(
        "onto",
        "the commit the replay was based on",
        json!(run.onto),
    );
    set("replaced", "the tip it rewrote away", json!(run.replaced));
    set("result", "the tip the branch ended on", json!(run.result));
    set(
        "steps",
        "how many commits the replay wrote",
        json!(run.produced.len() as i64),
    );
    set(
        "completed",
        "whether a `finish` entry closed the run; false means it was aborted or \
         is still in progress",
        json!(run.completed),
    );
    set("started_at", "when the replay began", json!(run.started_at));
    set(
        "finished_at",
        "when the branch moved",
        json!(run.finished_at),
    );
    set(
        "_from_reflog",
        "read from this clone's reflog, which is local and expires — the commit \
         graph records no rebase at all",
        json!(true),
    );
    Node {
        key: key.to_string(),
        label: "Rebase".into(),
        extra_labels: Vec::new(),
        props: p,
    }
}
