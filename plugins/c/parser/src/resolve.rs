//! The cross-file half. Identity is `{file}::{name}`; **binding** is C's
//! linkage model: a call reaches its own file's symbol first (statics shadow
//! by the compiler's rule, and a file's own definition is nearest), then the
//! one global definition when the tree holds exactly one, then a unique
//! declaration when nothing defines the name here, then libc by name — and
//! is counted when several files define it, because which definition links
//! is build configuration a parser does not have. A header's declaration
//! merges into the definition wherever one exists: the definition wins the
//! node.

use crate::{Call, Edge, FileFacts, Node, Props, edge_at};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// The assembled result: facts, and an account of what could not be done.
#[derive(Debug, Default)]
pub struct Assembled {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub skipped: usize,
    pub notes: Vec<String>,
}

/// libc's everyday surface; a call to one is external by name, not noise —
/// `memcpy` tells a reader something real.
const LIBC: &[&str] = &[
    "printf",
    "fprintf",
    "sprintf",
    "snprintf",
    "vsnprintf",
    "vprintf",
    "scanf",
    "sscanf",
    "fscanf",
    "puts",
    "putchar",
    "getchar",
    "fgets",
    "fputs",
    "fopen",
    "fclose",
    "fread",
    "fwrite",
    "fseek",
    "ftell",
    "rewind",
    "fflush",
    "feof",
    "ferror",
    "remove",
    "rename",
    "tmpfile",
    "malloc",
    "calloc",
    "realloc",
    "free",
    "abort",
    "exit",
    "atexit",
    "system",
    "getenv",
    "atoi",
    "atol",
    "atof",
    "strtol",
    "strtoul",
    "strtod",
    "rand",
    "srand",
    "qsort",
    "bsearch",
    "abs",
    "labs",
    "div",
    "memcpy",
    "memmove",
    "memset",
    "memcmp",
    "memchr",
    "strcpy",
    "strncpy",
    "strcat",
    "strncat",
    "strcmp",
    "strncmp",
    "strchr",
    "strrchr",
    "strstr",
    "strlen",
    "strtok",
    "strerror",
    "strdup",
    "isalpha",
    "isdigit",
    "isalnum",
    "isspace",
    "isupper",
    "islower",
    "toupper",
    "tolower",
    "time",
    "clock",
    "difftime",
    "mktime",
    "strftime",
    "localtime",
    "gmtime",
    "setjmp",
    "longjmp",
    "signal",
    "raise",
    "assert",
    "errno",
    "open",
    "close",
    "read",
    "write",
    "lseek",
    "stat",
    "fstat",
    "unlink",
    "getpid",
    "fork",
    "execvp",
    "waitpid",
    "pipe",
    "dup2",
    "usleep",
    "sleep",
    "pthread_create",
    "pthread_join",
    "pthread_mutex_lock",
    "pthread_mutex_unlock",
    "pthread_mutex_init",
    "pthread_cond_wait",
    "pthread_cond_signal",
];

pub fn assemble(all: Vec<FileFacts>) -> Assembled {
    let mut out = Assembled::default();

    let Decls {
        suppressed,
        merged,
        own,
        defs,
        decls_only,
        multiply_defined,
    } = index_decls(&all, &mut out);

    // ---- nodes ------------------------------------------------------------
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for f in &all {
        for n in &f.nodes {
            if !suppressed.contains(&n.key) && seen.insert(n.key.clone()) {
                out.nodes.push(n.clone());
            }
        }
    }

    let mut edges = Edges::default();
    for f in &all {
        for e in &f.edges {
            edges.add(e.clone());
        }
    }

    let (mut external, missed_includes) = resolve_includes(&all, &mut out, &mut seen, &mut edges);
    let Calls {
        unresolved,
        external_calls,
        ambiguous,
        type_ref_edges,
        foreign_type_refs,
    } = resolve_calls(
        &all,
        &own,
        &defs,
        &decls_only,
        &suppressed,
        &mut seen,
        &mut external,
        &mut out,
        &mut edges,
    );

    stand_ins(&mut out, &mut seen, &external);

    // ---- the account ------------------------------------------------------
    if unresolved > 0 {
        out.notes.push(format!(
            "{unresolved} call(s) left unresolved: a function pointer, an \
             unexpanded macro, or a library this tree does not hold"
        ));
    }
    if ambiguous > 0 {
        out.notes.push(format!(
            "{ambiguous} call(s) to names defined in more than one file were \
             left unbound: which definition links is build configuration a \
             parser does not have"
        ));
    }
    if external_calls > 0 {
        out.notes.push(format!(
            "{external_calls} call(s) into the C library, recorded as external \
             nodes by name"
        ));
    }
    if missed_includes > 0 {
        out.notes.push(format!(
            "{missed_includes} include(s) named no file in this tree, or more \
             than one — build include paths are configuration a parser does \
             not have"
        ));
    }
    if merged > 0 {
        out.notes.push(format!(
            "{merged} declaration(s) merged into the definition another file \
             holds — the header pattern; the definition wins"
        ));
    }
    if multiply_defined > 0 {
        out.notes.push(format!(
            "{multiply_defined} name(s) defined in more than one file — kept \
             apart, one node per defining file"
        ));
    }
    if type_ref_edges > 0 || foreign_type_refs > 0 {
        out.notes.push(format!(
            "{type_ref_edges} type reference(s) recorded as `USES_TYPE` — what a \
             declaration's fields, parameters and return are typed by; \
             {foreign_type_refs} more name a type this tree does not declare \
             (a library's, or one several files define) and are left as text"
        ));
    }
    out
}

/// `a/b/../c` → `a/c`, without touching the filesystem.
fn normalize(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    out.join("/")
}

/// The edges an assemble has decided on, deduplicated by (src, type, dst):
/// the same fact asserted twice is one edge.
#[derive(Default)]
struct Edges {
    pending: Vec<Edge>,
    seen: BTreeSet<(String, String, String)>,
}

impl Edges {
    fn add(&mut self, e: Edge) {
        if self
            .seen
            .insert((e.src.clone(), e.ty.clone(), e.dst.clone()))
        {
            self.pending.push(e);
        }
    }
}

/// `#include "x.h"` resolves against the parsed set: same directory first,
/// then an unambiguous tail anywhere — include paths are build configuration
/// this parser does not have, so ambiguity is counted rather than guessed.
///
/// Returns the system headers to stand in for, and how many includes resolved
/// to nothing.
fn resolve_includes(
    all: &[FileFacts],
    out: &mut Assembled,
    seen: &mut BTreeSet<String>,
    edges: &mut Edges,
) -> (BTreeMap<String, &'static str>, usize) {
    let file_keys: BTreeMap<String, String> = all
        .iter()
        .filter(|f| !f.failed)
        .map(|f| (f.file.clone(), crate::file_key(&f.file)))
        .collect();
    let mut external: BTreeMap<String, &'static str> = BTreeMap::new();
    let mut missed_includes = 0usize;
    let resolve_include = |from: &str, target: &str| -> Option<String> {
        let dir = from.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let local = if dir.is_empty() {
            target.to_string()
        } else {
            format!("{dir}/{target}")
        };
        let normal = normalize(&local);
        if let Some(k) = file_keys.get(&normal) {
            return Some(k.clone());
        }
        let matches: Vec<&String> = file_keys
            .iter()
            .filter(|(p, _)| p.ends_with(target) || p.as_str() == target)
            .map(|(_, k)| k)
            .collect();
        match matches.len() {
            1 => Some(matches[0].clone()),
            _ => None,
        }
    };
    let mut includes_prop: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for f in all {
        let src_key = crate::file_key(&f.file);
        for (target, system, line) in &f.includes {
            if *system {
                let name = target.clone();
                if seen.insert(name.clone()) {
                    external.insert(name.clone(), "File");
                }
                edges.add(edge_at(&src_key, &name, "IMPORTS", *line));
                includes_prop.entry(src_key.clone()).or_default().push(name);
                continue;
            }
            match resolve_include(&f.file, target) {
                Some(k) => {
                    edges.add(edge_at(&src_key, &k, "IMPORTS", *line));
                    includes_prop.entry(src_key.clone()).or_default().push(k);
                }
                None => {
                    missed_includes += 1;
                    // Unresolvable stays as written — readable, just not a
                    // link.
                    includes_prop
                        .entry(src_key.clone())
                        .or_default()
                        .push(target.clone());
                }
            }
        }
    }
    // The File node's `includes`, in include order, each entry the key of
    // the file it names — the shape the dashboard resolves into links.
    for n in out.nodes.iter_mut() {
        if n.label == "File"
            && let Some(list) = includes_prop.get(&n.key)
        {
            n.props.insert(
                "includes".into(),
                serde_json::Value::String(list.join(", ")),
            );
        }
    }

    (external, missed_includes)
}

/// What resolving a tree's calls and type references settled, and what it
/// could not.
struct Calls {
    unresolved: usize,
    external_calls: usize,
    ambiguous: usize,
    type_ref_edges: usize,
    foreign_type_refs: usize,
}

/// Every call site, resolved nearest-first: the caller's own file, then the
/// one global definition, then a declaration — and counted when none of those
/// answers, rather than guessed at.
#[allow(clippy::too_many_arguments)]
fn resolve_calls(
    all: &[FileFacts],
    own: &BTreeMap<String, BTreeMap<String, String>>,
    defs: &BTreeMap<String, Vec<String>>,
    decls_only: &BTreeMap<String, Vec<String>>,
    suppressed: &BTreeSet<String>,
    seen: &mut BTreeSet<String>,
    external: &mut BTreeMap<String, &'static str>,
    out: &mut Assembled,
    edges: &mut Edges,
) -> Calls {
    let mut unresolved = 0usize;
    let mut external_calls = 0usize;
    let mut ambiguous = 0usize;
    for f in all {
        unresolved += f.opaque;
        let file_key = crate::file_key(&f.file);
        let own_names = own.get(&file_key);
        let hints: BTreeMap<(&str, &str), &str> = f
            .hints
            .iter()
            .map(|h| ((h.caller.as_str(), h.name.as_str()), h.target.as_str()))
            .collect();
        for c in &f.calls {
            // A call through a bound function pointer resolves to what the
            // body bound it to — one hop, first binding wins.
            let name: String = match hints.get(&(c.caller.as_str(), c.name.as_str())) {
                Some(target) => (*target).to_string(),
                None => {
                    if c.name.contains('.') {
                        // A member slot nothing in this body bound: a
                        // value's business after all.
                        unresolved += 1;
                        continue;
                    }
                    c.name.clone()
                }
            };
            let c = &Call {
                caller: c.caller.clone(),
                name,
                line: c.line,
            };
            // Nearest first: this file's own symbol, static or not — the
            // compiler's shadowing rule, and a definition's own file is
            // always its caller's best answer.
            if let Some(key) = own_names.and_then(|m| m.get(&c.name))
                && !suppressed.contains(key)
            {
                {
                    edges.add(edge_at(&c.caller, key, "CALLS", c.line));
                    continue;
                }
                // Our own prototype merged into a definition elsewhere —
                // fall through to the global lookup, which is where it went.
            }
            match defs.get(&c.name).map(Vec::as_slice) {
                Some([one]) => {
                    edges.add(edge_at(&c.caller, one, "CALLS", c.line));
                    continue;
                }
                Some(_) => {
                    // Several files define it; which one links is build
                    // configuration. Counted, never guessed.
                    ambiguous += 1;
                    continue;
                }
                None => {}
            }
            // Nothing defines it here: a unique declaration is still the
            // interface — the name is real, the body is elsewhere.
            if let Some(decl_keys) = decls_only.get(&c.name) {
                let distinct: BTreeSet<&String> = decl_keys.iter().collect();
                if distinct.len() == 1 {
                    edges.add(edge_at(&c.caller, decl_keys[0].as_str(), "CALLS", c.line));
                    continue;
                }
            }
            if LIBC.contains(&c.name.as_str()) {
                if !seen.contains(&c.name) {
                    external.insert(c.name.clone(), "Function");
                    seen.insert(c.name.clone());
                }
                external_calls += 1;
                edges.add(edge_at(&c.caller, &c.name, "CALLS", c.line));
                continue;
            }
            // A name nothing declares: an unexpanded macro, or a library
            // this tree does not hold. Counted, not guessed.
            unresolved += 1;
        }
    }
    fn_references(all, own, defs, suppressed, edges);
    // Published before the type pass, which appends to `out.edges` directly.
    out.edges = std::mem::take(&mut edges.pending);
    let (type_ref_edges, foreign_type_refs) = type_references(all, own, defs, decls_only, out);

    Calls {
        unresolved,
        external_calls,
        ambiguous,
        type_ref_edges,
        foreign_type_refs,
    }
}

/// Functions passed as values (C4, narrow): an argument naming an in-tree
/// FUNCTION — own file first, unique definition elsewhere — becomes a
/// `REFERENCES` edge. A variable argument resolves to nothing here (only
/// functions live in these maps); never a self-loop.
fn fn_references(
    all: &[FileFacts],
    own: &BTreeMap<String, BTreeMap<String, String>>,
    defs: &BTreeMap<String, Vec<String>>,
    suppressed: &BTreeSet<String>,
    edges: &mut Edges,
) {
    // Functions passed as values (C4, narrow): an argument naming an
    // in-tree FUNCTION — own file first, unique definition elsewhere —
    // becomes a REFERENCES edge. A variable argument resolves to nothing
    // here (only functions live in these maps); never a self-loop.
    let mut ref_seen: BTreeSet<(String, String)> = BTreeSet::new();
    for f in all {
        let own_names = own.get(f.file.as_str());
        for (caller, name, line) in &f.fn_refs {
            let target = own_names
                .and_then(|m| m.get(name))
                .filter(|k| !suppressed.contains(*k))
                .cloned()
                .or_else(|| match defs.get(name).map(Vec::as_slice) {
                    Some([one]) => Some(one.clone()),
                    _ => None,
                });
            let Some(target) = target else { continue };
            if target == *caller || !ref_seen.insert((caller.clone(), target.clone())) {
                continue;
            }
            let mut e = edge_at(caller, &target, "REFERENCES", *line);
            e.props
                .insert("_resolved_by".into(), Value::String("fn-ref".into()));
            e.props
                .insert("_confidence".into(), Value::String("high".into()));
            e.props.insert("_ref".into(), Value::String(name.clone()));
            edges.add(e);
        }
    }
}

/// A declared type is a dependency as surely as a call is, and C states it in
/// the one place it states anything about types: the declaration.
///
/// Binding follows the same nearest-first model as calls, because it is the
/// same question — which `foo` is this?
fn type_references(
    all: &[FileFacts],
    own: &BTreeMap<String, BTreeMap<String, String>>,
    defs: &BTreeMap<String, Vec<String>>,
    decls_only: &BTreeMap<String, Vec<String>>,
    out: &mut Assembled,
) -> (usize, usize) {
    // A declared type is a dependency as surely as a call is. C states this
    // in the one place it states anything about types: the declaration.
    //
    // Binding follows the same nearest-first model as calls, because it is
    // the same question — which `foo` is this? The caller's own file first
    // (a file-local type shadows a global of the same name), then the one
    // definition the tree holds, then a unique declaration; a name several
    // files define is counted, never guessed, since which one links is build
    // configuration a parser does not have. Primitives never get here: they
    // name no declarable type and the parser drops them at the source.
    let type_keys: BTreeSet<&str> = out
        .nodes
        .iter()
        .filter(|n| matches!(n.label.as_str(), "Struct" | "Union" | "Enum" | "TypeAlias"))
        .map(|n| n.key.as_str())
        .collect();
    let a_type = |key: &String| -> bool { type_keys.contains(key.as_str()) };
    let mut uses: BTreeMap<(String, String), (BTreeSet<String>, String)> = BTreeMap::new();
    let mut foreign_type_refs = 0usize;
    for f in all {
        if f.failed {
            continue;
        }
        let file_key = crate::file_key(&f.file);
        for r in &f.type_refs {
            let target = own
                .get(&file_key)
                .and_then(|m| m.get(&r.written))
                .filter(|k| a_type(k))
                .or_else(|| match defs.get(&r.written).map(Vec::as_slice) {
                    Some([one]) if a_type(one) => Some(one),
                    _ => None,
                })
                .or_else(|| match decls_only.get(&r.written).map(Vec::as_slice) {
                    Some([one]) if a_type(one) => Some(one),
                    _ => None,
                });
            let Some(target) = target else {
                foreign_type_refs += 1;
                continue;
            };
            // A type that mentions itself — `struct node { struct node *next; }`
            // — is a real shape and a useless edge.
            if *target == r.owner {
                continue;
            }
            let slot = uses
                .entry((r.owner.clone(), target.clone()))
                .or_insert_with(|| (BTreeSet::new(), r.name.clone()));
            slot.0.insert(r.role.clone());
        }
    }
    let type_ref_edges = uses.len();
    for ((owner, target), (roles, name)) in uses {
        let mut e = edge_at(&owner, &target, "USES_TYPE", 0);
        e.props.remove("line");
        e.props.insert(
            "role".into(),
            serde_json::json!({
                "$desc": "the type position this declaration names the type in",
                "$value": roles.iter().cloned().collect::<Vec<_>>().join(", "),
            }),
        );
        if !name.is_empty() {
            e.props.insert("name".into(), Value::String(name));
        }
        out.edges.push(e);
    }

    (type_ref_edges, foreign_type_refs)
}

/// What each file declares, by name, and what the tree declares across files.
///
/// Per file: its own symbols, static or not — the compiler's shadowing rule,
/// and a definition's own file is always its caller's best answer. Across
/// files: every non-static definition and declaration of each name.
struct Decls {
    /// Prototypes that merged into a definition elsewhere: the key exists,
    /// but the node for it must not be emitted twice.
    suppressed: BTreeSet<String>,
    merged: usize,
    own: BTreeMap<String, BTreeMap<String, String>>,
    defs: BTreeMap<String, Vec<String>>,
    decls_only: BTreeMap<String, Vec<String>>,
    /// Names more than one translation unit defines — the tree cannot say
    /// which a call meant, so it says so rather than choosing.
    multiply_defined: usize,
}

fn index_decls(all: &[FileFacts], out: &mut Assembled) -> Decls {
    let mut own: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut defs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut decls_only: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for f in all {
        if f.failed {
            out.skipped += 1;
            continue;
        }
        let file_key = crate::file_key(&f.file);
        let per_file = own.entry(file_key).or_default();
        for d in &f.decls {
            per_file
                .entry(d.name.clone())
                .or_insert_with(|| d.key.clone());
            if !d.is_static {
                if d.is_definition {
                    defs.entry(d.name.clone()).or_default().push(d.key.clone());
                } else {
                    decls_only
                        .entry(d.name.clone())
                        .or_default()
                        .push(d.key.clone());
                }
            }
        }
    }
    let multiply_defined = defs.values().filter(|v| v.len() > 1).count();

    // A declaration is merged away when any file defines the name: the
    // definition wins the node, and the prototype's remaining contribution
    // is its file's IMPORTS edge. Keyed by node key, so a static is never
    // merged into someone else's definition.
    let mut suppressed: BTreeSet<String> = BTreeSet::new();
    let mut merged = 0usize;
    for f in all {
        for d in &f.decls {
            if !d.is_static && !d.is_definition && defs.contains_key(&d.name) {
                suppressed.insert(d.key.clone());
                merged += 1;
            }
        }
    }

    Decls {
        suppressed,
        merged,
        own,
        defs,
        decls_only,
        multiply_defined,
    }
}

/// Nodes for what the tree names but does not hold: the endpoint of an edge
/// nothing defined, and the system headers an include reached for. Recorded
/// as stand-ins rather than dropped, so "this code uses that" survives.
fn stand_ins(
    out: &mut Assembled,
    seen: &mut BTreeSet<String>,
    external: &BTreeMap<String, &'static str>,
) {
    let mut implied: BTreeSet<String> = BTreeSet::new();
    for e in &out.edges {
        for key in [&e.src, &e.dst] {
            if !seen.contains(key.as_str()) && !external.contains_key(key.as_str()) {
                implied.insert(key.clone());
            }
        }
    }
    for key in implied {
        seen.insert(key.clone());
        out.nodes.push(Node {
            key,
            label: "Type".into(),
            extra_labels: Vec::new(),
            props: Props::new(),
        });
    }
    for (key, label) in external {
        out.nodes.push(Node {
            key: key.clone(),
            label: (*label).into(),
            extra_labels: vec!["External".into()],
            props: Props::new(),
        });
    }
}
