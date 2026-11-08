//! Tests against **real repositories**, built by the `git` binary.
//!
//! Nothing here is a captured fixture. A hand-written `.git` would only prove
//! this code agrees with itself, and the whole risk in reading an object store
//! is disagreeing with git about a format git owns — so every test builds a
//! repository, does something to it, and reads what git actually wrote.
//!
//! Commit dates are set explicitly, one second apart. Two commits in the same
//! second are ordered by nothing in particular, and `max_commits` keeps the
//! newest — a property that cannot be tested against a clock that does not
//! move.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::*;

/// The plugin's grant, over a real directory: paths relative to the git
/// directory, and no way out of it. The same shape the component wires to the
/// host's `list`/`read`.
struct DiskFiles {
    root: PathBuf,
}

impl Files for DiskFiles {
    fn list(&self, suffix: &str) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        walk(&self.root, &self.root, &mut out)?;
        out.retain(|p| suffix.is_empty() || p.ends_with(suffix));
        out.sort(); // sorted is part of the host's contract
        Ok(out)
    }

    fn read(&self, path: &str) -> Result<Vec<u8>, String> {
        std::fs::read(self.root.join(path)).map_err(|e| format!("{path}: {e}"))
    }
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<(), String> {
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, out)?;
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

/// A throwaway repository.
struct Repo {
    dir: PathBuf,
    /// Seconds added to a fixed base for the next commit, so history has an
    /// order that does not depend on how fast the test ran.
    clock: std::cell::Cell<i64>,
}

/// 2026-01-01T00:00:00Z, near enough. Any fixed instant would do; a fixed one
/// is what makes the assertions on rendered dates possible at all.
const BASE: i64 = 1_767_225_600;

impl Repo {
    fn new(name: &str) -> Repo {
        let dir = std::env::temp_dir().join(format!(
            "drsg-git-parser-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("making a temp directory");
        let repo = Repo {
            dir,
            clock: std::cell::Cell::new(0),
        };
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["config", "user.name", "Ada Lovelace"]);
        repo.git(&["config", "user.email", "ada@example.com"]);
        // Nothing here signs, and a developer's own `commit.gpgsign = true`
        // would otherwise make these tests wait on a passphrase prompt.
        repo.git(&["config", "commit.gpgsign", "false"]);
        repo
    }

    fn git(&self, args: &[&str]) -> String {
        let when = BASE + self.clock.get();
        let stamp = format!("{when} +0000");
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.dir)
            .args(args)
            .env("GIT_AUTHOR_DATE", &stamp)
            .env("GIT_COMMITTER_DATE", &stamp)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", &self.dir)
            .output()
            .expect("running git — these tests need it on PATH");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// One commit, one second after the last.
    fn commit(&self, file: &str, contents: &str, message: &str) -> String {
        self.clock.set(self.clock.get() + 1);
        std::fs::write(self.dir.join(file), contents).expect("writing a file");
        self.git(&["add", file]);
        self.git(&["commit", "-q", "-m", message]);
        self.git(&["rev-parse", "HEAD"])
    }

    fn files(&self) -> DiskFiles {
        DiskFiles {
            root: self.dir.join(".git"),
        }
    }

    fn read(&self) -> History {
        read_history(&self.files(), &Options::default()).expect("reading the history")
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ---- reading a repository ------------------------------------------------

fn node<'a>(h: &'a History, key: &str) -> &'a Node {
    h.nodes
        .iter()
        .find(|n| n.key == key)
        .unwrap_or_else(|| panic!("no node `{key}` in {:?}", keys(h)))
}

fn keys(h: &History) -> Vec<&str> {
    h.nodes.iter().map(|n| n.key.as_str()).collect()
}

fn value<'a>(n: &'a Node, prop: &str) -> &'a Value {
    &n.props
        .get(prop)
        .unwrap_or_else(|| panic!("no property `{prop}` on {}", n.key))["$value"]
}

fn edges<'a>(h: &'a History, ty: &str) -> Vec<&'a Edge> {
    h.edges.iter().filter(|e| e.ty == ty).collect()
}

#[test]
fn a_linear_history_becomes_commits_a_branch_and_parent_edges() {
    let repo = Repo::new("linear");
    let first = repo.commit("a.txt", "one", "root");
    let second = repo.commit("a.txt", "two", "second\n\nwith a body");
    let history = repo.read();

    assert_eq!(history.summary.commits, 2);
    assert_eq!(history.summary.branches, 1);
    assert_eq!(history.summary.head, "main");

    let root = node(&history, &commit_key(&first));
    assert_eq!(root.label, "Commit");
    assert!(root.extra_labels.is_empty(), "one parent is not a merge");
    assert_eq!(value(root, "summary"), "root");
    assert_eq!(value(root, "author_name"), "Ada Lovelace");
    assert_eq!(value(root, "author_email"), "ada@example.com");
    assert_eq!(value(root, "parents"), 0);
    assert_eq!(
        value(root, "authored_at"),
        "2026-01-01T00:00:01+00:00",
        "the date git was given, rendered in the author's own zone"
    );

    let tip = node(&history, &commit_key(&second));
    assert_eq!(value(tip, "body"), "with a body");

    let parents = edges(&history, "PARENT");
    assert_eq!(parents.len(), 1);
    assert_eq!(parents[0].src, commit_key(&second));
    assert_eq!(parents[0].dst, commit_key(&first));
    assert_eq!(parents[0].props["order"]["$value"], 1);

    let branch = node(&history, &branch_key("refs/heads/main"));
    assert_eq!(value(branch, "name"), "main");
    assert_eq!(value(branch, "is_head"), true);
    assert_eq!(value(branch, "tip"), second.as_str());
    let tips = edges(&history, "TIP");
    assert_eq!(tips.len(), 1);
    assert_eq!(tips[0].dst, commit_key(&second));
}

#[test]
fn a_merge_is_labelled_as_one_and_its_parents_are_ordered() {
    let repo = Repo::new("merge");
    let root = repo.commit("a.txt", "one", "root");
    repo.git(&["checkout", "-q", "-b", "feature"]);
    let feature = repo.commit("f.txt", "f", "feat");
    repo.git(&["checkout", "-q", "main"]);
    let main = repo.commit("a.txt", "two", "on main");
    repo.git(&["merge", "-q", "--no-ff", "feature", "-m", "merge feature"]);
    let merge = repo.git(&["rev-parse", "HEAD"]);

    let history = repo.read();
    assert_eq!(history.summary.commits, 4);
    assert_eq!(history.summary.merges, 1);
    assert_eq!(history.summary.branches, 2);

    let node = node(&history, &commit_key(&merge));
    assert_eq!(node.extra_labels, vec!["Merge".to_string()]);
    assert_eq!(value(node, "is_merge"), true);
    assert_eq!(value(node, "parents"), 2);

    let mut ordered: Vec<(i64, &str)> = edges(&history, "PARENT")
        .into_iter()
        .filter(|e| e.src == commit_key(&merge))
        .map(|e| (e.props["order"]["$value"].as_i64().unwrap(), e.dst.as_str()))
        .collect();
    ordered.sort();
    assert_eq!(
        ordered,
        vec![
            (1, commit_key(&main).as_str()),
            (2, commit_key(&feature).as_str())
        ],
        "first parent is the branch the merge was made on"
    );
    let _ = root;
}

#[test]
fn a_rebase_is_reconstructed_and_what_it_replaced_is_kept() {
    let repo = Repo::new("rebase");
    repo.commit("a.txt", "one", "root");
    repo.git(&["checkout", "-q", "-b", "feature"]);
    let old_first = repo.commit("f.txt", "f1", "feat one");
    let old_tip = repo.commit("f.txt", "f2", "feat two");
    repo.git(&["checkout", "-q", "main"]);
    let onto = repo.commit("a.txt", "two", "on main");
    repo.git(&["checkout", "-q", "feature"]);
    repo.git(&["rebase", "-q", "main"]);
    let result = repo.git(&["rev-parse", "HEAD"]);

    let history = repo.read();
    assert_eq!(history.summary.rebases, 1, "notes: {:?}", history.notes);

    let rebase = history
        .nodes
        .iter()
        .find(|n| n.label == "Rebase")
        .expect("a Rebase node");
    assert_eq!(value(rebase, "ref"), "refs/heads/feature");
    assert_eq!(value(rebase, "branch"), "feature");
    assert_eq!(value(rebase, "completed"), true);
    assert_eq!(value(rebase, "interactive"), false);
    assert_eq!(value(rebase, "onto"), onto.as_str());
    assert_eq!(
        value(rebase, "replaced"),
        old_tip.as_str(),
        "the tip the branch had before the replay"
    );
    assert_eq!(value(rebase, "result"), result.as_str());
    assert_eq!(value(rebase, "steps"), 2);

    let from = |ty: &str| -> Vec<&str> {
        edges(&history, ty)
            .into_iter()
            .filter(|e| e.src == rebase.key)
            .map(|e| e.dst.as_str())
            .collect()
    };
    assert_eq!(from("ONTO"), vec![commit_key(&onto)]);
    assert_eq!(from("REPLACED"), vec![commit_key(&old_tip)]);
    assert_eq!(from("RESULT"), vec![commit_key(&result)]);
    assert_eq!(from("PRODUCED").len(), 2, "one per replayed commit");
    assert_eq!(from("ON"), vec![branch_key("refs/heads/feature")]);

    // The commits the rebase wrote away are still in the object store, and a
    // graph that dropped them could not say what the rebase changed.
    let abandoned = node(&history, &commit_key(&old_tip));
    assert_eq!(
        value(abandoned, "reachable"),
        false,
        "no branch leads here any more"
    );
    assert_eq!(
        value(node(&history, &commit_key(&result)), "reachable"),
        true
    );
    assert!(history.summary.unreachable >= 2, "{:?}", history.summary);
    let _ = old_first;

    assert!(
        history
            .notes
            .iter()
            .any(|n| n.contains("expires") && n.contains("reflog")),
        "the reflog's limits are stated, not assumed: {:?}",
        history.notes
    );
}

#[test]
fn tags_are_read_annotated_and_lightweight_alike() {
    let repo = Repo::new("tags");
    let first = repo.commit("a.txt", "one", "root");
    repo.git(&["tag", "-a", "v1", "-m", "the first release"]);
    repo.git(&["tag", "light"]);

    let history = repo.read();
    assert_eq!(history.summary.tags, 2);

    let annotated = node(&history, &tag_key("refs/tags/v1"));
    assert_eq!(value(annotated, "annotated"), true);
    assert_eq!(value(annotated, "target"), first.as_str());
    assert_eq!(value(annotated, "message"), "the first release");
    assert_eq!(value(annotated, "tagger_name"), "Ada Lovelace");
    assert_ne!(
        value(annotated, "object"),
        first.as_str(),
        "an annotated tag is its own object"
    );

    let light = node(&history, &tag_key("refs/tags/light"));
    assert_eq!(value(light, "annotated"), false);
    assert_eq!(value(light, "target"), first.as_str());

    let targets: Vec<&str> = edges(&history, "TAGS")
        .iter()
        .map(|e| e.dst.as_str())
        .collect();
    assert_eq!(targets, vec![commit_key(&first), commit_key(&first)]);
}

/// The test this whole object-store implementation exists for: after `git gc`
/// there are no loose objects left, every commit is a packed — usually
/// *deltified* — entry, and the graph must come out identical.
#[test]
fn a_packed_repository_reads_identically_to_a_loose_one() {
    let repo = Repo::new("packed");
    for i in 0..12 {
        // Contents that resemble each other, so the packer really does store
        // these as deltas rather than as whole objects.
        let body = (0..40)
            .map(|n| format!("line {n} of {i}\n"))
            .collect::<String>();
        repo.commit("a.txt", &body, &format!("change {i}"));
    }
    repo.git(&["tag", "-a", "v1", "-m", "release"]);
    let loose = repo.read();

    repo.git(&["gc", "-q", "--aggressive", "--prune=now"]);
    let files = repo.files();
    let listing = files.list("").unwrap();
    assert!(
        listing.iter().any(|p| p.ends_with(".pack")),
        "gc should have packed this repository"
    );
    assert!(
        listing.iter().any(|p| p == "packed-refs"),
        "gc should have packed the refs"
    );
    let packed = read_history(&files, &Options::default()).expect("reading a packed repository");

    assert_eq!(packed.summary.commits, loose.summary.commits);
    assert_eq!(
        packed
            .notes
            .iter()
            .filter(|n| n.contains("unreadable"))
            .count(),
        0
    );
    let by_key = |h: &History| -> BTreeMap<String, Value> {
        h.nodes
            .iter()
            .map(|n| (n.key.clone(), Value::Object(n.props.clone())))
            .collect()
    };
    assert_eq!(
        by_key(&packed),
        by_key(&loose),
        "a packed object says exactly what the loose one said"
    );
    assert_eq!(packed.edges.len(), loose.edges.len());
}

#[test]
fn the_commit_ceiling_keeps_the_newest_and_says_so() {
    let repo = Repo::new("ceiling");
    let mut shas = Vec::new();
    for i in 0..6 {
        shas.push(repo.commit("a.txt", &format!("{i}"), &format!("change {i}")));
    }
    let opts = Options {
        max_commits: 3,
        ..Default::default()
    };
    let history = read_history(&repo.files(), &opts).unwrap();

    assert_eq!(history.summary.commits, 3);
    for newest in &shas[3..] {
        assert!(
            history.nodes.iter().any(|n| n.key == commit_key(newest)),
            "the newest three are the ones kept"
        );
    }
    assert!(
        history.nodes.iter().all(|n| n.key != commit_key(&shas[0])),
        "the oldest is not"
    );
    assert!(
        history.notes.iter().any(|n| n.contains("max_commits")),
        "a truncated graph explains itself: {:?}",
        history.notes
    );
    assert_eq!(
        edges(&history, "PARENT").len(),
        2,
        "the edge off the oldest kept commit leads outside, and is dropped"
    );
    assert!(
        history
            .notes
            .iter()
            .any(|n| n.contains("outside this graph")),
        "an edge left out is counted, not silently missing: {:?}",
        history.notes
    );
}

#[test]
fn an_empty_repository_is_a_state_rather_than_a_failure() {
    let repo = Repo::new("empty");
    let history = repo.read();
    assert!(history.nodes.is_empty());
    assert_eq!(history.summary.commits, 0);
    assert_eq!(history.summary.head, "main (no commits yet)");
    assert!(
        history.notes.iter().any(|n| n.contains("no commits yet")),
        "{:?}",
        history.notes
    );
}

#[test]
fn a_detached_head_is_read_and_named() {
    let repo = Repo::new("detached");
    let first = repo.commit("a.txt", "one", "root");
    repo.commit("a.txt", "two", "second");
    repo.git(&["checkout", "-q", &first]);

    let history = repo.read();
    assert_eq!(history.summary.head, format!("detached at {}", &first[..7]));
    assert_eq!(
        value(node(&history, &branch_key("refs/heads/main")), "is_head"),
        false
    );
}

#[test]
fn turning_the_reflog_off_leaves_reachability_unclaimed() {
    let repo = Repo::new("noreflog");
    repo.commit("a.txt", "one", "root");
    let opts = Options {
        reflog: false,
        ..Default::default()
    };
    let history = read_history(&repo.files(), &opts).unwrap();
    assert_eq!(history.summary.rebases, 0);
    assert!(
        history.nodes[0].props.get("reachable").is_none(),
        "a question this run did not ask is left unanswered rather than guessed"
    );
}

// ---- the parsers on their own -------------------------------------------

#[test]
fn options_are_read_and_a_typo_is_refused() {
    let opts = Options::from_settings(&[
        ("max_commits".into(), "50".into()),
        ("tags".into(), "no".into()),
    ])
    .unwrap();
    assert_eq!(opts.max_commits, 50);
    assert!(!opts.tags);
    assert!(opts.reflog, "an untouched setting keeps its default");

    let err = Options::from_settings(&[("max_commit".into(), "50".into())]).unwrap_err();
    assert!(
        err.contains("max_commits"),
        "the error names what was meant: {err}"
    );
}

#[test]
fn a_signature_keeps_its_own_timezone() {
    let sig = object::Sig::parse("Ada Lovelace <ada@example.com> 1767225600 -0430");
    assert_eq!(sig.name, "Ada Lovelace");
    assert_eq!(sig.email, "ada@example.com");
    assert_eq!(sig.ts, 1_767_225_600);
    assert_eq!(sig.tz, -270);
    assert_eq!(
        sig.iso8601(),
        "2025-12-31T19:30:00-04:30",
        "the same instant, as the author's clock had it"
    );
}

/// A commit carrying a PGP signature is still a commit. Its continuation lines
/// are not headers, and reading them as headers is how a parser invents a
/// field called `-----BEGIN`.
#[test]
fn a_signed_commit_parses_past_its_signature() {
    let raw = concat!(
        "tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n",
        "parent 0000000000000000000000000000000000000001\n",
        "author Ada <ada@example.com> 1767225600 +0000\n",
        "committer Ada <ada@example.com> 1767225600 +0000\n",
        "gpgsig -----BEGIN PGP SIGNATURE-----\n",
        " \n",
        " iQEzBAABCAAdFiEE\n",
        " -----END PGP SIGNATURE-----\n",
        "\n",
        "the subject\n\nand the body\n"
    );
    let commit = object::parse_commit("abc", raw.as_bytes());
    assert_eq!(commit.parents.len(), 1);
    assert_eq!(commit.summary, "the subject");
    assert_eq!(commit.body, "and the body");
    assert_eq!(commit.author.name, "Ada");
}

#[test]
fn packed_refs_peel_their_annotated_tags() {
    let text = "# pack-refs with: peeled fully-peeled sorted \n\
                aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/main\n\
                bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb refs/tags/v1\n\
                ^cccccccccccccccccccccccccccccccccccccccc\n";
    let refs = super::refs::parse_packed_refs(text);
    assert_eq!(refs.len(), 2);
    assert_eq!(refs[0].name, "refs/heads/main");
    assert_eq!(refs[0].peeled, None);
    assert_eq!(refs[1].name, "refs/tags/v1");
    assert_eq!(
        refs[1].peeled.as_deref(),
        Some("cccccccccccccccccccccccccccccccccccccccc"),
        "the `^` line is the commit the tag object points at"
    );
}

/// The reflog spans whatever versions of git have touched a repository, and
/// the pre-2.26 backend wrote different words for the same operation.
#[test]
fn the_retired_rebase_backends_messages_are_read_too() {
    let entry = |old: &str, new: &str, message: &str| reflog::LogEntry {
        refname: "HEAD".into(),
        old: old.repeat(40),
        new: new.repeat(40),
        who: object::Sig {
            name: "Ada".into(),
            email: "ada@example.com".into(),
            ts: 1_767_225_600,
            tz: 0,
        },
        message: message.into(),
    };
    let logs = BTreeMap::from([(
        "HEAD".to_string(),
        vec![
            entry("a", "b", "rebase: checkout main"),
            entry("b", "c", "rebase: a replayed change"),
            entry("c", "d", "rebase finished: returning to refs/heads/topic"),
        ],
    )]);

    let runs = reflog::find_rebases(&logs);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].refname, "refs/heads/topic");
    assert_eq!(runs[0].onto, "b".repeat(40));
    assert_eq!(runs[0].replaced, "a".repeat(40));
    assert_eq!(runs[0].result, "d".repeat(40));
    assert_eq!(runs[0].produced.len(), 1);
    assert!(runs[0].completed);
}

/// A rebase that was abandoned still wrote commits, and saying so is more use
/// than leaving the run out.
#[test]
fn an_abandoned_rebase_is_kept_and_marked_incomplete() {
    let repo = Repo::new("abandoned");
    repo.commit("a.txt", "one", "root");
    repo.git(&["checkout", "-q", "-b", "feature"]);
    repo.commit("f.txt", "f", "feat");
    repo.git(&["checkout", "-q", "main"]);
    repo.commit("a.txt", "conflicting", "on main");
    repo.git(&["checkout", "-q", "feature"]);
    repo.commit("a.txt", "also conflicting", "conflict here");
    // Rebasing across a conflict stops mid-replay; aborting returns the branch
    // and leaves a run with no `finish`.
    let conflicted = Command::new("git")
        .arg("-C")
        .arg(&repo.dir)
        .args(["rebase", "main"])
        .env("HOME", &repo.dir)
        .output()
        .expect("running git");
    assert!(!conflicted.status.success(), "this rebase should conflict");
    repo.git(&["rebase", "--abort"]);

    let history = repo.read();
    let rebase = history
        .nodes
        .iter()
        .find(|n| n.label == "Rebase")
        .expect("the abandoned run is still recorded");
    assert_eq!(value(rebase, "completed"), false);
}

// ---- the paths a commit changed (ROADMAP §11) ----------------------------

/// `files` as a list of strings, for comparing against a literal.
fn changed<'a>(h: &'a History, sha: &str) -> Vec<&'a str> {
    value(node(h, &format!("commit:{sha}")), "files")
        .as_array()
        .expect("`files` is a list")
        .iter()
        .map(|v| v.as_str().expect("a path is a string"))
        .collect()
}

/// A commit records what it changed, and nothing it left alone.
///
/// The root commit is the one case with no parent to diff against, so it
/// reports its whole tree; every commit after it reports only its own change,
/// which is what makes the list worth storing.
#[test]
fn a_commit_records_the_paths_it_changed() {
    let repo = Repo::new("files-linear");
    std::fs::create_dir_all(repo.dir.join("src/deep")).unwrap();
    std::fs::write(repo.dir.join("src/deep/a.rs"), "a").unwrap();
    std::fs::write(repo.dir.join("README.md"), "r").unwrap();
    repo.clock.set(1);
    repo.git(&["add", "."]);
    repo.git(&["commit", "-q", "-m", "root"]);
    let root = repo.git(&["rev-parse", "HEAD"]);

    // A nested file, to show the path is repository-relative and the walk
    // recurses rather than listing a directory.
    repo.clock.set(2);
    std::fs::write(repo.dir.join("src/deep/a.rs"), "a2").unwrap();
    repo.git(&["add", "src/deep/a.rs"]);
    repo.git(&["commit", "-q", "-m", "edit the nested file"]);
    let edit = repo.git(&["rev-parse", "HEAD"]);

    let h = repo.read();
    assert_eq!(
        changed(&h, &root),
        vec!["README.md", "src/deep/a.rs"],
        "a root commit has no parent, so its whole tree is what it added"
    );
    assert_eq!(
        changed(&h, &edit),
        vec!["src/deep/a.rs"],
        "only the file that changed — README.md sits in an untouched tree"
    );
    assert_eq!(value(node(&h, &format!("commit:{edit}")), "files_count"), 1);
    // The honesty flag is absent when the list is complete, which is almost
    // always, so it does not become noise on every commit.
    assert!(
        !node(&h, &format!("commit:{edit}"))
            .props
            .contains_key("files_partial"),
        "a complete list says nothing about being partial"
    );
}

/// A delete, and a rename.
///
/// Git stores no rename — it stores a tree — so a rename is honestly two
/// paths: the name that left and the name that arrived. Recorded that way
/// rather than guessed at with a similarity score, which is a choice git's own
/// porcelain makes at display time and not something the object store knows.
#[test]
fn a_delete_and_a_rename_name_every_path_involved() {
    let repo = Repo::new("files-moves");
    repo.commit("keep.txt", "k", "root");
    repo.commit("gone.txt", "g", "add the one to remove");
    repo.clock.set(3);
    repo.git(&["rm", "-q", "gone.txt"]);
    repo.git(&["commit", "-q", "-m", "remove it"]);
    let removed = repo.git(&["rev-parse", "HEAD"]);

    repo.clock.set(4);
    repo.git(&["mv", "keep.txt", "moved.txt"]);
    repo.git(&["commit", "-q", "-m", "move it"]);
    let moved = repo.git(&["rev-parse", "HEAD"]);

    let h = repo.read();
    assert_eq!(changed(&h, &removed), vec!["gone.txt"]);
    assert_eq!(
        changed(&h, &moved),
        vec!["keep.txt", "moved.txt"],
        "a rename is a path that left and a path that arrived"
    );
}

/// An ordinary merge lists nothing; the commits it brought in carry the
/// changes. A merge that resolved something lists what it resolved.
///
/// This is git's `TREESAME` rule, and the reason for it is practical: a merge
/// that re-listed its branch's files would make every file look touched by
/// every merge, which is the opposite of what the property is for.
#[test]
fn a_merge_lists_only_what_differs_from_every_parent() {
    let repo = Repo::new("files-merge");
    repo.commit("base.txt", "base\n", "root");
    repo.git(&["checkout", "-q", "-b", "side"]);
    let on_side = repo.commit("side.txt", "s", "a file only the side adds");
    repo.git(&["checkout", "-q", "main"]);
    repo.commit("main.txt", "m", "a file only main adds");

    // No conflict: the two sides touched different files.
    repo.clock.set(9);
    repo.git(&["merge", "-q", "--no-ff", "-m", "clean merge", "side"]);
    let clean = repo.git(&["rev-parse", "HEAD"]);

    let h = repo.read();
    assert_eq!(
        changed(&h, &clean),
        Vec::<&str>::new(),
        "a clean merge added nothing of its own"
    );
    assert_eq!(
        changed(&h, &on_side),
        vec!["side.txt"],
        "the side commit is what actually added the file"
    );

    // Now one that has to resolve: both sides change the same line, and the
    // resolution is content that exists in neither parent.
    let conflict = Repo::new("files-merge-conflict");
    conflict.commit("f.txt", "original\n", "root");
    conflict.git(&["checkout", "-q", "-b", "side"]);
    conflict.commit("f.txt", "side\n", "side's version");
    conflict.git(&["checkout", "-q", "main"]);
    conflict.commit("f.txt", "main\n", "main's version");
    conflict.clock.set(9);
    let merge = Command::new("git")
        .arg("-C")
        .arg(&conflict.dir)
        .args(["merge", "--no-ff", "-m", "resolve"])
        .arg("side")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", &conflict.dir)
        .output()
        .expect("running git");
    assert!(
        !merge.status.success(),
        "the test's premise: this merge conflicts"
    );
    std::fs::write(conflict.dir.join("f.txt"), "resolved\n").unwrap();
    conflict.git(&["add", "f.txt"]);
    conflict.git(&["commit", "-q", "--no-edit", "-m", "resolve"]);
    let resolved = conflict.git(&["rev-parse", "HEAD"]);

    let h = conflict.read();
    assert_eq!(
        changed(&h, &resolved),
        vec!["f.txt"],
        "the resolution is the merge's own content, in neither parent"
    );
}

/// A packed repository answers identically — the diff reads trees through the
/// same object database, so delta-compressed trees must come back whole.
#[test]
fn the_paths_are_the_same_once_the_objects_are_packed() {
    let repo = Repo::new("files-packed");
    repo.commit("a.txt", "1", "root");
    let second = repo.commit("b.txt", "2", "second");
    let loose = repo.read();

    repo.git(&["gc", "-q", "--aggressive", "--prune=now"]);
    assert!(
        !repo
            .files()
            .list(".pack")
            .expect("listing the git directory")
            .is_empty(),
        "the test's premise: git packed the objects"
    );

    let packed = repo.read();
    assert_eq!(changed(&loose, &second), changed(&packed, &second));
    assert_eq!(changed(&packed, &second), vec!["b.txt"]);
}
