//! The cross-file half. C's linker model, applied to facts: a definition
//! wins the node over a header's declaration of the same key, a call binds
//! to this file's statics first (the compiler's own shadowing rule), then
//! the flat global namespace; `#include "x.h"` resolves against the parsed
//! file set the way a build's include paths would.

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
    // The flat global namespace: name → key, definitions preferred over
    // declarations, first definition wins (one-definition is the program's
    // own rule; a violation is counted, not adjudicated).
    let mut global: BTreeMap<String, (String, bool)> = BTreeMap::new();
    // file stem → its statics' names → keys.
    let mut statics: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    // file path (as parsed) → file key, for include resolution.
    let mut file_keys: BTreeMap<String, String> = BTreeMap::new();
    let mut redefined = 0usize;

    for f in &all {
        if f.failed {
            out.skipped += 1;
            continue;
        }
        file_keys.insert(f.file.clone(), crate::file_key(&f.file));
        for d in &f.decls {
            if d.is_static {
                statics
                    .entry(f.stem.clone())
                    .or_default()
                    .entry(d.name.clone())
                    .or_insert_with(|| d.key.clone());
                continue;
            }
            match global.get(&d.name) {
                None => {
                    global.insert(d.name.clone(), (d.key.clone(), d.is_definition));
                }
                Some((_, had_def)) => {
                    if d.is_definition {
                        if *had_def {
                            redefined += 1; // two definitions — counted
                        } else {
                            global.insert(d.name.clone(), (d.key.clone(), true));
                        }
                    }
                }
            }
        }
    }

    // ---- nodes: definitions beat declarations, first seen wins ----------
    // Pick per key: any definition's node over any declaration's.
    let mut best: BTreeMap<String, &Node> = BTreeMap::new();
    let mut is_def: BTreeMap<String, bool> = BTreeMap::new();
    let mut contains: BTreeMap<String, &Edge> = BTreeMap::new();
    let mut merged = 0usize;
    for f in &all {
        let defs: BTreeSet<&str> = f
            .decls
            .iter()
            .filter(|d| d.is_definition)
            .map(|d| d.key.as_str())
            .collect();
        for n in &f.nodes {
            let this_is_def = defs.contains(n.key.as_str()) || n.label == "File";
            match is_def.get(&n.key) {
                None => {
                    best.insert(n.key.clone(), n);
                    is_def.insert(n.key.clone(), this_is_def);
                    if let Some(e) = f
                        .edges
                        .iter()
                        .find(|e| e.ty == "CONTAINS" && e.dst == n.key)
                    {
                        contains.insert(n.key.clone(), e);
                    }
                }
                Some(false) if this_is_def => {
                    best.insert(n.key.clone(), n);
                    is_def.insert(n.key.clone(), true);
                    if let Some(e) = f
                        .edges
                        .iter()
                        .find(|e| e.ty == "CONTAINS" && e.dst == n.key)
                    {
                        contains.insert(n.key.clone(), e);
                    }
                    merged += 1;
                }
                Some(_) => merged += 1,
            }
        }
    }
    let mut seen: BTreeSet<String> = BTreeSet::new();
    // Keep file order stable: walk facts again, emit each key once, using
    // the chosen node.
    for f in &all {
        for n in &f.nodes {
            if seen.insert(n.key.clone())
                && let Some(chosen) = best.get(&n.key)
            {
                out.nodes.push((*chosen).clone());
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
    // CONTAINS from the chosen side; everything else as written.
    for e in contains.values() {
        add_edge(&mut pending, &mut edge_set, (*e).clone());
    }
    for f in &all {
        for e in &f.edges {
            if e.ty != "CONTAINS" {
                add_edge(&mut pending, &mut edge_set, e.clone());
            }
        }
    }

    // ---- includes ---------------------------------------------------------
    // `#include "x.h"` resolves against the parsed set: exact tail match,
    // shortest surviving candidate — the way an include path would find it.
    // `<system>` headers are external by declaration.
    let mut external: BTreeMap<String, &'static str> = BTreeMap::new();
    let mut missed_includes = 0usize;
    let resolve_include = |from: &str, target: &str| -> Option<String> {
        // Same directory first.
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
        // Then anywhere in the tree by suffix — include paths are build
        // configuration this parser does not have; an unambiguous tail
        // match is certain enough, ambiguity is not.
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
                continue;
            }
            match resolve_include(&f.file, target) {
                Some(k) => add_edge(
                    &mut pending,
                    &mut edge_set,
                    edge_at(&src_key, &k, "IMPORTS", *line),
                ),
                None => missed_includes += 1,
            }
        }
    }

    // ---- calls ------------------------------------------------------------
    let mut unresolved = 0usize;
    let mut external_calls = 0usize;
    for f in &all {
        unresolved += f.opaque;
        let own_statics = statics.get(&f.stem);
        for c in &f.calls {
            // This file's statics shadow the global name — the compiler's
            // own rule.
            if let Some(key) = own_statics.and_then(|s| s.get(&c.name)) {
                add_edge(
                    &mut pending,
                    &mut edge_set,
                    edge_at(&c.caller, key, "CALLS", c.line),
                );
                continue;
            }
            if let Some((key, _)) = global.get(&c.name) {
                add_edge(
                    &mut pending,
                    &mut edge_set,
                    edge_at(&c.caller, key, "CALLS", c.line),
                );
                continue;
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
            if !seen.contains(key.as_str())
                && !external.contains_key(key.as_str())
                && !best.contains_key(key.as_str())
            {
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
            "{merged} declaration(s) merged across files — headers declaring \
             what a source defines; the definition wins"
        ));
    }
    if redefined > 0 {
        out.notes.push(format!(
            "{redefined} name(s) defined in more than one file — one-definition \
             is the program's rule to keep; the first seen is kept here"
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
