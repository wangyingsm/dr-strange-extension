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

/// Name → defining keys, per kind, plus the per-file maps that let a
/// reference prefer the nearest definition.
///
/// Definitions are split by provenance: a `.min.css` is a build artifact of
/// its readable sibling, not a second opinion — minified definitions yield to
/// source ones, and only tie-break among themselves when no source defines the
/// name at all.
#[derive(Default)]
struct Index {
    files: BTreeSet<String>,
    classes: BTreeMap<String, Vec<String>>,
    classes_min: BTreeMap<String, Vec<String>>,
    vars: BTreeMap<String, Vec<String>>,
    vars_min: BTreeMap<String, Vec<String>>,
    own_classes: BTreeMap<String, BTreeMap<String, String>>,
    own_vars: BTreeMap<String, BTreeMap<String, String>>,
    own_fns: BTreeMap<String, BTreeMap<String, String>>,
}

fn build_index(all: &[FileFacts], out: &mut Assembled) -> Index {
    let mut ix = Index::default();
    for f in all {
        if f.failed {
            out.skipped += 1;
            continue;
        }
        ix.files.insert(f.file.clone());
        let minified = is_minified(&f.file);
        for d in &f.classes {
            let sink = if minified {
                &mut ix.classes_min
            } else {
                &mut ix.classes
            };
            sink.entry(d.name.clone()).or_default().push(d.key.clone());
            ix.own_classes
                .entry(f.file.clone())
                .or_default()
                .entry(d.name.clone())
                .or_insert_with(|| d.key.clone());
        }
        for d in &f.vars {
            let sink = if minified {
                &mut ix.vars_min
            } else {
                &mut ix.vars
            };
            sink.entry(d.name.clone()).or_default().push(d.key.clone());
            ix.own_vars
                .entry(f.file.clone())
                .or_default()
                .entry(d.name.clone())
                .or_insert_with(|| d.key.clone());
        }
        for d in &f.functions {
            ix.own_fns
                .entry(f.file.clone())
                .or_default()
                .entry(d.name.clone())
                .or_insert_with(|| d.key.clone());
        }
    }
    ix
}

pub fn assemble(all: Vec<FileFacts>) -> Assembled {
    let mut out = Assembled::default();

    let ix = build_index(&all, &mut out);

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

    let mut refs = Refs::default();
    for f in &all {
        for e in &f.edges {
            refs.add(e.clone());
        }
    }

    resolve_refs(&all, &ix, &seen, &mut refs);
    out.edges = std::mem::take(&mut refs.pending);

    // ---- external and implied nodes ---------------------------------------
    for url in &refs.external {
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
    let Refs {
        unresolved,
        external_links,
        missed_files,
        ..
    } = refs;
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

/// What reference resolution accumulates: the edges it decided on, and the
/// four counts the account reports.
#[derive(Default)]
struct Refs {
    pending: Vec<Edge>,
    edge_set: BTreeSet<(String, String, String)>,
    external: BTreeSet<String>,
    unresolved: usize,
    external_links: usize,
    missed_files: usize,
}

impl Refs {
    /// One edge, once: the same (src, type, dst) asserted twice is one fact.
    fn add(&mut self, e: Edge) {
        if self
            .edge_set
            .insert((e.src.clone(), e.ty.clone(), e.dst.clone()))
        {
            self.pending.push(e);
        }
    }
}

/// A relative href resolved against the parsed set — same-directory paths,
/// `../`, and a `#fragment` tail; queries are stripped. External URLs (a
/// scheme, or `//`) are reported as such rather than guessed at.
fn resolve_path(files: &BTreeSet<String>, from: &str, target: &str) -> PathResolution {
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
}

/// Every reference in the tree, resolved against the index — nearest
/// definition first, then the one unique definition, else counted.
fn resolve_refs(all: &[FileFacts], ix: &Index, seen: &BTreeSet<String>, refs: &mut Refs) {
    for f in all {
        for r in &f.refs {
            match r.kind {
                RefKind::Import => match resolve_path(&ix.files, &f.file, &r.target) {
                    PathResolution::File(k) | PathResolution::Fragment(k, _) => {
                        refs.add(edge_at(&r.src, &k, "IMPORTS", r.line))
                    }
                    PathResolution::External => {
                        // A CDN script/stylesheet: the URL is the identity.
                        refs.external.insert(r.target.clone());
                        refs.add(edge_at(&r.src, &r.target, "IMPORTS", r.line));
                    }
                    PathResolution::Miss => refs.missed_files += 1,
                },
                RefKind::Link => match resolve_path(&ix.files, &f.file, &r.target) {
                    PathResolution::File(k) => refs.add(edge_at(&r.src, &k, "LINKS", r.line)),
                    PathResolution::Fragment(file, frag) => {
                        // Link to an id: the fragment key when the element
                        // was parsed, the page when it was not.
                        let anchored = format!("{file}#{frag}");
                        let dst = if seen.contains(&anchored) {
                            anchored
                        } else {
                            file
                        };
                        refs.add(edge_at(&r.src, &dst, "LINKS", r.line));
                    }
                    PathResolution::External => refs.external_links += 1,
                    PathResolution::Miss => refs.missed_files += 1,
                },
                RefKind::Class => {
                    // Nearest first: the page's own <style>, else the one
                    // unique stylesheet definition.
                    if let Some(key) = ix.own_classes.get(&f.file).and_then(|m| m.get(&r.target)) {
                        refs.add(edge_at(&r.src, key, "STYLED_BY", r.line));
                        continue;
                    }
                    match unique(&ix.classes, &ix.classes_min, &r.target) {
                        Some(one) => refs.add(edge_at(&r.src, one, "STYLED_BY", r.line)),
                        None => refs.unresolved += 1,
                    }
                }
                RefKind::Var => {
                    if let Some(key) = ix.own_vars.get(&f.file).and_then(|m| m.get(&r.target)) {
                        refs.add(edge_at(&r.src, key, "USES", r.line));
                        continue;
                    }
                    match unique(&ix.vars, &ix.vars_min, &r.target) {
                        Some(one) => refs.add(edge_at(&r.src, one, "USES", r.line)),
                        None => refs.unresolved += 1,
                    }
                }
                RefKind::Call => {
                    // An inline script's world is its page.
                    match ix.own_fns.get(&f.file).and_then(|m| m.get(&r.target)) {
                        Some(key) => refs.add(edge_at(&r.src, key, "CALLS", r.line)),
                        None => refs.unresolved += 1,
                    }
                }
            }
        }
        refs.unresolved += f.opaque;
    }
}
