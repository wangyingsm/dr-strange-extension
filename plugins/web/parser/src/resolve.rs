//! The cross-file half: file references resolved against the parsed set,
//! classes and custom properties bound nearest-first — a page's own
//! `<style>` before the stylesheets, matching how the browser's own cascade
//! makes page-local rules feel local — then the one unique definition when
//! the tree holds exactly one, else counted, never guessed. Inline-script
//! calls bind within their page: an inline script's world is its page.

use crate::{Edge, FileFacts, Node, Props, RefKind, edge_at, is_minified};
use std::collections::{BTreeMap, BTreeSet};

/// The assembled result: facts, and an account of what could not be done.
#[derive(Debug, Default)]
pub struct Assembled {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub skipped: usize,
    pub notes: Vec<String>,
}

pub fn assemble(all: Vec<FileFacts>) -> Assembled {
    let mut out = Assembled::default();

    // ---- indexes ----------------------------------------------------------
    // name → defining keys, per kind; and per-file maps for nearest-first.
    let mut files: BTreeSet<String> = BTreeSet::new();
    // Definitions split by provenance: a `.min.css` is a build artifact of
    // its readable sibling, not a second opinion — minified definitions
    // yield to source ones, and only tie-break among themselves when no
    // source defines the name at all.
    let mut classes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut classes_min: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut vars: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut vars_min: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut own_classes: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut own_vars: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut own_fns: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for f in &all {
        if f.failed {
            out.skipped += 1;
            continue;
        }
        files.insert(f.file.clone());
        let minified = is_minified(&f.file);
        for d in &f.classes {
            let sink = if minified {
                &mut classes_min
            } else {
                &mut classes
            };
            sink.entry(d.name.clone()).or_default().push(d.key.clone());
            own_classes
                .entry(f.file.clone())
                .or_default()
                .entry(d.name.clone())
                .or_insert_with(|| d.key.clone());
        }
        for d in &f.vars {
            let sink = if minified { &mut vars_min } else { &mut vars };
            sink.entry(d.name.clone()).or_default().push(d.key.clone());
            own_vars
                .entry(f.file.clone())
                .or_default()
                .entry(d.name.clone())
                .or_insert_with(|| d.key.clone());
        }
        for d in &f.functions {
            own_fns
                .entry(f.file.clone())
                .or_default()
                .entry(d.name.clone())
                .or_insert_with(|| d.key.clone());
        }
    }

    // ---- nodes, first seen wins ------------------------------------------
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut merged = 0usize;
    for f in &all {
        for n in &f.nodes {
            if seen.insert(n.key.clone()) {
                out.nodes.push(n.clone());
            } else {
                merged += 1;
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
            add_edge(&mut pending, &mut edge_set, e.clone());
        }
    }

    // ---- reference resolution ---------------------------------------------
    // A relative href resolves against the parsed set — same-directory
    // paths, `../`, and a `#fragment` tail; queries are stripped. External
    // URLs (scheme or //) are counted as such, not guessed at.
    let resolve_path = |from: &str, target: &str| -> PathResolution {
        let raw = target.split(['?']).next().unwrap_or(target);
        let (path_part, fragment) = match raw.split_once('#') {
            Some((p, frag)) => (p, Some(frag)),
            None => (raw, None),
        };
        if path_part.contains("://") || path_part.starts_with("//") {
            return PathResolution::External;
        }
        if path_part.is_empty() {
            // `#section` — this file.
            return match fragment {
                Some(frag) => PathResolution::Fragment(from.to_string(), frag.to_string()),
                None => PathResolution::Miss,
            };
        }
        let dir = from.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let joined = normalize(&if path_part.starts_with('/') {
            path_part.trim_start_matches('/').to_string()
        } else if dir.is_empty() {
            path_part.to_string()
        } else {
            format!("{dir}/{path_part}")
        });
        if files.contains(&joined) {
            return match fragment {
                Some(frag) => PathResolution::Fragment(joined, frag.to_string()),
                None => PathResolution::File(joined),
            };
        }
        PathResolution::Miss
    };

    let mut unresolved = 0usize;
    let mut external_links = 0usize;
    let mut missed_files = 0usize;
    let mut external: BTreeSet<String> = BTreeSet::new();
    for f in &all {
        for r in &f.refs {
            match r.kind {
                RefKind::Import => match resolve_path(&f.file, &r.target) {
                    PathResolution::File(k) | PathResolution::Fragment(k, _) => add_edge(
                        &mut pending,
                        &mut edge_set,
                        edge_at(&r.src, &k, "IMPORTS", r.line),
                    ),
                    PathResolution::External => {
                        // A CDN script/stylesheet: the URL is the identity.
                        external.insert(r.target.clone());
                        add_edge(
                            &mut pending,
                            &mut edge_set,
                            edge_at(&r.src, &r.target, "IMPORTS", r.line),
                        );
                    }
                    PathResolution::Miss => missed_files += 1,
                },
                RefKind::Link => match resolve_path(&f.file, &r.target) {
                    PathResolution::File(k) => add_edge(
                        &mut pending,
                        &mut edge_set,
                        edge_at(&r.src, &k, "LINKS", r.line),
                    ),
                    PathResolution::Fragment(file, frag) => {
                        // Link to an id: the fragment key when the element
                        // was parsed, the page when it was not.
                        let anchored = format!("{file}#{frag}");
                        let dst = if seen.contains(&anchored) {
                            anchored
                        } else {
                            file
                        };
                        add_edge(
                            &mut pending,
                            &mut edge_set,
                            edge_at(&r.src, &dst, "LINKS", r.line),
                        );
                    }
                    PathResolution::External => external_links += 1,
                    PathResolution::Miss => missed_files += 1,
                },
                RefKind::Class => {
                    // Nearest first: the page's own <style>, else the one
                    // unique stylesheet definition.
                    if let Some(key) = own_classes.get(&f.file).and_then(|m| m.get(&r.target)) {
                        add_edge(
                            &mut pending,
                            &mut edge_set,
                            edge_at(&r.src, key, "STYLED_BY", r.line),
                        );
                        continue;
                    }
                    match unique(&classes, &classes_min, &r.target) {
                        Some(one) => add_edge(
                            &mut pending,
                            &mut edge_set,
                            edge_at(&r.src, one, "STYLED_BY", r.line),
                        ),
                        None => unresolved += 1,
                    }
                }
                RefKind::Var => {
                    if let Some(key) = own_vars.get(&f.file).and_then(|m| m.get(&r.target)) {
                        add_edge(
                            &mut pending,
                            &mut edge_set,
                            edge_at(&r.src, key, "USES", r.line),
                        );
                        continue;
                    }
                    match unique(&vars, &vars_min, &r.target) {
                        Some(one) => add_edge(
                            &mut pending,
                            &mut edge_set,
                            edge_at(&r.src, one, "USES", r.line),
                        ),
                        None => unresolved += 1,
                    }
                }
                RefKind::Call => {
                    // An inline script's world is its page.
                    match own_fns.get(&f.file).and_then(|m| m.get(&r.target)) {
                        Some(key) => add_edge(
                            &mut pending,
                            &mut edge_set,
                            edge_at(&r.src, key, "CALLS", r.line),
                        ),
                        None => unresolved += 1,
                    }
                }
            }
        }
        unresolved += f.opaque;
    }
    out.edges = pending;

    // ---- external and implied nodes ---------------------------------------
    for url in &external {
        if seen.insert(url.clone()) {
            out.nodes.push(Node {
                key: url.clone(),
                label: "File".into(),
                extra_labels: vec!["External".into()],
                props: Props::new(),
            });
        }
    }
    let mut implied: BTreeSet<String> = BTreeSet::new();
    for e in &out.edges {
        for key in [&e.src, &e.dst] {
            if !seen.contains(key.as_str()) {
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

    // ---- the account ------------------------------------------------------
    if unresolved > 0 {
        out.notes.push(format!(
            "{unresolved} reference(s) left unresolved: a class or property \
             defined in more than one stylesheet (or none this tree holds), \
             or a call outside the page's own script"
        ));
    }
    if external_links > 0 {
        out.notes.push(format!(
            "{external_links} link(s) to the world outside this tree — \
             recorded in no graph"
        ));
    }
    if missed_files > 0 {
        out.notes.push(format!(
            "{missed_files} reference(s) named files the digest never saw — \
             assets, or paths outside the tree"
        ));
    }
    if merged > 0 {
        out.notes.push(format!(
            "{merged} declaration(s) shared a key across files; the first \
             seen is kept"
        ));
    }
    out
}

/// The one definition to bind: unique among source files first; minified
/// definitions only speak when no source defines the name.
fn unique<'m>(
    source: &'m BTreeMap<String, Vec<String>>,
    minified: &'m BTreeMap<String, Vec<String>>,
    name: &str,
) -> Option<&'m String> {
    match source.get(name).map(Vec::as_slice) {
        Some([one]) => Some(one),
        Some(_) => None,
        None => match minified.get(name).map(Vec::as_slice) {
            Some([one]) => Some(one),
            _ => None,
        },
    }
}

enum PathResolution {
    File(String),
    Fragment(String, String),
    External,
    Miss,
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
