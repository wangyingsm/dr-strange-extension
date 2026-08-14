//! The cross-file half. Identity is `{file}::{name}`; **binding** is C's
//! linkage model: a call reaches its own file's symbol first (statics shadow
//! by the compiler's rule, and a file's own definition is nearest), then the
//! one global definition when the tree holds exactly one, then a unique
//! declaration when nothing defines the name here, then libc by name — and
//! is counted when several files define it, because which definition links
//! is build configuration a parser does not have. A header's declaration
//! merges into the definition wherever one exists: the definition wins the
//! node.

use crate::{Edge, FileFacts, Node, Props, edge_at};
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

    // ---- indexes ----------------------------------------------------------
    // Per file: what it declares, by name. Across files: every non-static
    // definition and declaration of each name.
    let mut own: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut defs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut decls_only: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for f in &all {
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
    for f in &all {
        for d in &f.decls {
            if !d.is_static && !d.is_definition && defs.contains_key(&d.name) {
                suppressed.insert(d.key.clone());
                merged += 1;
            }
        }
    }

    // ---- nodes ------------------------------------------------------------
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for f in &all {
        for n in &f.nodes {
            if !suppressed.contains(&n.key) && seen.insert(n.key.clone()) {
                out.nodes.push(n.clone());
            }
        }
    }

    let mut edge_set: BTreeSet<(String, String, String)> = BTreeSet::new();
    let mut pending: Vec<Edge> = Vec::new();
    let add_edge =
        |pending: &mut Vec<Edge>, set: &mut BTreeSet<(String, String, String)>, e: Edge| {
            if set.insert((e.src.clone(), e.ty.clone(), e.dst.clone())) {
                pending.push(e);
            }
        };
    for f in &all {
        for e in &f.edges {
            if e.ty == "CONTAINS" && suppressed.contains(&e.dst) {
                continue;
            }
            add_edge(&mut pending, &mut edge_set, e.clone());
        }
    }

    // ---- includes ---------------------------------------------------------
    // `#include "x.h"` resolves against the parsed set: same directory
    // first, then an unambiguous tail anywhere — include paths are build
    // configuration this parser does not have, so ambiguity is counted.
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
    for f in &all {
        let src_key = crate::file_key(&f.file);
        for (target, system, line) in &f.includes {
            if *system {
                let name = target.clone();
                if seen.insert(name.clone()) {
                    external.insert(name.clone(), "File");
                }
                add_edge(
                    &mut pending,
                    &mut edge_set,
                    edge_at(&src_key, &name, "IMPORTS", *line),
                );
                includes_prop.entry(src_key.clone()).or_default().push(name);
                continue;
            }
            match resolve_include(&f.file, target) {
                Some(k) => {
                    add_edge(
                        &mut pending,
                        &mut edge_set,
                        edge_at(&src_key, &k, "IMPORTS", *line),
                    );
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

    // ---- calls ------------------------------------------------------------
    let mut unresolved = 0usize;
    let mut external_calls = 0usize;
    let mut ambiguous = 0usize;
    for f in &all {
        unresolved += f.opaque;
        let file_key = crate::file_key(&f.file);
        let own_names = own.get(&file_key);
        for c in &f.calls {
            // Nearest first: this file's own symbol, static or not — the
            // compiler's shadowing rule, and a definition's own file is
            // always its caller's best answer.
            if let Some(key) = own_names.and_then(|m| m.get(&c.name))
                && !suppressed.contains(key)
            {
                {
                    add_edge(
                        &mut pending,
                        &mut edge_set,
                        edge_at(&c.caller, key, "CALLS", c.line),
                    );
                    continue;
                }
                // Our own prototype merged into a definition elsewhere —
                // fall through to the global lookup, which is where it went.
            }
            match defs.get(&c.name).map(Vec::as_slice) {
                Some([one]) => {
                    add_edge(
                        &mut pending,
                        &mut edge_set,
                        edge_at(&c.caller, one, "CALLS", c.line),
                    );
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
                    add_edge(
                        &mut pending,
                        &mut edge_set,
                        edge_at(&c.caller, decl_keys[0].as_str(), "CALLS", c.line),
                    );
                    continue;
                }
            }
            if LIBC.contains(&c.name.as_str()) {
                if !seen.contains(&c.name) {
                    external.insert(c.name.clone(), "Function");
                    seen.insert(c.name.clone());
                }
                external_calls += 1;
                add_edge(
                    &mut pending,
                    &mut edge_set,
                    edge_at(&c.caller, &c.name, "CALLS", c.line),
                );
                continue;
            }
            // A name nothing declares: an unexpanded macro, or a library
            // this tree does not hold. Counted, not guessed.
            unresolved += 1;
        }
    }
    out.edges = pending;

    // ---- implied and external nodes --------------------------------------
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
    for (key, label) in &external {
        out.nodes.push(Node {
            key: key.clone(),
            label: (*label).into(),
            extra_labels: vec!["External".into()],
            props: Props::new(),
        });
    }

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
