//! The cross-file half: type references bound against the parsed type set —
//! same package first, then imports, then wildcards, then `java.lang` —
//! calls walked onto method surfaces and up the in-tree extends chain, and
//! every clause turned into its edge. Once, over every chunk's facts, in
//! chunk order.

use crate::{CallKind, Edge, FileFacts, Node, Props, edge_at};
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

/// `java.lang` needs no import; a miss that names one of these is that.
const JAVA_LANG: &[&str] = &[
    "Object",
    "String",
    "StringBuilder",
    "StringBuffer",
    "CharSequence",
    "Integer",
    "Long",
    "Short",
    "Byte",
    "Double",
    "Float",
    "Boolean",
    "Character",
    "Number",
    "Math",
    "System",
    "Thread",
    "Runnable",
    "Class",
    "ClassLoader",
    "Comparable",
    "Iterable",
    "Enum",
    "Record",
    "Void",
    "Throwable",
    "Exception",
    "RuntimeException",
    "Error",
    "IllegalArgumentException",
    "IllegalStateException",
    "NullPointerException",
    "IndexOutOfBoundsException",
    "UnsupportedOperationException",
    "ArithmeticException",
    "ClassCastException",
    "NumberFormatException",
    "InterruptedException",
    "CloneNotSupportedException",
    "AutoCloseable",
    "Process",
    "Runtime",
];

struct Index {
    /// Every type this run parsed: qualified name → (methods, extends).
    types: BTreeMap<String, (BTreeSet<String>, Vec<String>)>,
    /// package → simple name → qualified, for same-package resolution.
    by_package: BTreeMap<String, BTreeMap<String, String>>,
    packages: BTreeSet<String>,
}

impl Index {
    /// A type reference resolved the way javac reads it: already-qualified,
    /// else same package, else an import, else a wildcard import, else
    /// `java.lang`, else nothing a parser can say. `Ok(None)` means a
    /// certain external; `Err(())` means unresolvable.
    #[allow(clippy::result_unit_err)]
    fn type_ref(&self, written: &str, f: &FileFacts) -> Result<(String, bool), ()> {
        // (key, in_tree)
        if written.contains('.') {
            // Qualified as written: in-tree if we parsed it; an outer.Inner
            // reference through an import resolves the outer first.
            if self.types.contains_key(written) {
                return Ok((written.to_string(), true));
            }
            let (head, rest) = written.split_once('.').unwrap();
            if let Ok((outer, true)) = self.type_ref(head, f) {
                let nested = format!("{outer}.{rest}");
                if self.types.contains_key(&nested) {
                    return Ok((nested, true));
                }
            }
            // A dotted name whose head is lowercase reads as a package path
            // — external, as written.
            if written.chars().next().is_some_and(char::is_lowercase) {
                return Ok((written.to_string(), false));
            }
            return Err(());
        }
        if let Some(q) = self.by_package.get(&f.package).and_then(|m| m.get(written)) {
            return Ok((q.clone(), true));
        }
        if let Some(imp) = f
            .imports
            .iter()
            .find(|i| !i.is_static && i.local == written)
        {
            let in_tree = self.types.contains_key(&imp.target);
            return Ok((imp.target.clone(), in_tree));
        }
        for pkg in &f.wildcards {
            let candidate = format!("{pkg}.{written}");
            if self.types.contains_key(&candidate) {
                return Ok((candidate, true));
            }
        }
        if JAVA_LANG.contains(&written) {
            return Ok((format!("java.lang.{written}"), false));
        }
        Err(())
    }

    /// Does `type_key` answer `method`, on itself or up its in-tree extends
    /// chain? Returns the type that declares it.
    fn find_method(
        &self,
        f: &FileFacts,
        type_key: &str,
        method: &str,
        depth: usize,
    ) -> Option<String> {
        if depth > 16 {
            return None;
        }
        let (methods, extends) = self.types.get(type_key)?;
        if methods.contains(method) {
            return Some(type_key.to_string());
        }
        for base in extends.clone() {
            if let Ok((base_key, true)) = self.type_ref(&base, f)
                && let Some(found) = self.find_method(f, &base_key, method, depth + 1)
            {
                return Some(found);
            }
        }
        None
    }
}

fn note_external(
    seen: &BTreeSet<String>,
    external: &mut BTreeMap<String, &'static str>,
    key: &str,
    label: &'static str,
) {
    if seen.contains(key) {
        return;
    }
    let have = external.get(key).copied();
    // Bare `External` (an import proves only existence) yields to anything
    // more specific a clause or call proves.
    if have.is_none() || (have == Some("External") && label != "External") {
        external.insert(key.to_string(), label);
    }
}

pub fn assemble(all: Vec<FileFacts>) -> Assembled {
    let mut out = Assembled::default();

    // ---- indexes ----------------------------------------------------------
    let mut ix = Index {
        types: BTreeMap::new(),
        by_package: BTreeMap::new(),
        packages: BTreeSet::new(),
    };
    let mut package_docs: BTreeMap<String, String> = BTreeMap::new();
    for f in &all {
        if f.failed {
            out.skipped += 1;
            continue;
        }
        if !f.package.is_empty() {
            ix.packages.insert(f.package.clone());
        }
        if let Some(doc) = &f.package_doc {
            package_docs
                .entry(f.package.clone())
                .or_insert_with(|| doc.clone());
        }
        for t in &f.types {
            ix.types
                .entry(t.key.clone())
                .or_insert_with(|| (t.methods.iter().cloned().collect(), t.extends.clone()));
            if let Some(simple) = t.key.rsplit('.').next()
                && t.key
                    .strip_suffix(simple)
                    .and_then(|p| p.strip_suffix('.'))
                    .map(|p| p == f.package)
                    .unwrap_or(f.package.is_empty())
            {
                ix.by_package
                    .entry(f.package.clone())
                    .or_default()
                    .entry(simple.to_string())
                    .or_insert_with(|| t.key.clone());
            }
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

    // ---- packages ---------------------------------------------------------
    let mut package_keys: Vec<String> = ix.packages.iter().cloned().collect();
    package_keys.sort();
    for pkg in &package_keys {
        if seen.insert(pkg.clone()) {
            let mut props = Props::new();
            props.insert("name".into(), Value::String(pkg.clone()));
            if let Some(doc) = package_docs.get(pkg) {
                props.insert("doc_comment".into(), Value::String(doc.clone()));
            }
            out.nodes.push(Node {
                key: pkg.clone(),
                label: "Package".into(),
                extra_labels: Vec::new(),
                props,
            });
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
    // The package tree, where both ends were parsed.
    for pkg in &package_keys {
        if let Some((parent, _)) = pkg.rsplit_once('.')
            && ix.packages.contains(parent)
        {
            add_edge(
                &mut pending,
                &mut edge_set,
                Edge {
                    src: parent.to_string(),
                    dst: pkg.clone(),
                    ty: "CONTAINS".into(),
                    props: Props::new(),
                },
            );
        }
    }

    // ---- IMPORTS ----------------------------------------------------------
    // From each of the file's top-level types — Java's import is file
    // scoped, and the type is what a reader navigates by.
    let mut external: BTreeMap<String, &'static str> = BTreeMap::new();
    for f in &all {
        for (target, line) in &f.import_lines {
            let dst = target.strip_suffix(".*").unwrap_or(target);
            let in_tree = ix.types.contains_key(dst) || ix.packages.contains(dst);
            if !in_tree {
                note_external(&seen, &mut external, dst, "External");
            }
            for t in &f.top_types {
                add_edge(
                    &mut pending,
                    &mut edge_set,
                    edge_at(t, dst, "IMPORTS", *line),
                );
            }
        }
    }

    // ---- calls and clauses -------------------------------------------------
    let mut unresolved = 0usize;
    let mut external_calls = 0usize;
    for f in &all {
        unresolved += f.opaque;

        for c in &f.calls {
            match &c.kind {
                CallKind::Own(method) => {
                    // The class's own surface, its in-tree ancestry, or a
                    // static import.
                    if let Some(owner) = ix.find_method(f, &c.in_type, method, 0) {
                        add_edge(
                            &mut pending,
                            &mut edge_set,
                            edge_at(&c.caller, &format!("{owner}.{method}"), "CALLS", c.line),
                        );
                        continue;
                    }
                    if let Some(imp) = f.imports.iter().find(|i| i.is_static && i.local == *method)
                    {
                        let in_tree_owner = imp.target.rsplit_once('.').and_then(|(t, _)| {
                            ix.types.contains_key(t).then(|| imp.target.clone())
                        });
                        match in_tree_owner {
                            Some(key) => add_edge(
                                &mut pending,
                                &mut edge_set,
                                edge_at(&c.caller, &key, "CALLS", c.line),
                            ),
                            None => {
                                note_external(&seen, &mut external, &imp.target, "Function");
                                external_calls += 1;
                                add_edge(
                                    &mut pending,
                                    &mut edge_set,
                                    edge_at(&c.caller, &imp.target, "CALLS", c.line),
                                );
                            }
                        }
                        continue;
                    }
                    unresolved += 1;
                }
                CallKind::Static { type_ref, method } => match ix.type_ref(type_ref, f) {
                    Ok((key, true)) => match ix.find_method(f, &key, method, 0) {
                        Some(owner) => add_edge(
                            &mut pending,
                            &mut edge_set,
                            edge_at(&c.caller, &format!("{owner}.{method}"), "CALLS", c.line),
                        ),
                        None => unresolved += 1,
                    },
                    Ok((key, false)) => {
                        let target = format!("{key}.{method}");
                        note_external(&seen, &mut external, &target, "Function");
                        note_external(&seen, &mut external, &key, "External");
                        external_calls += 1;
                        add_edge(
                            &mut pending,
                            &mut edge_set,
                            edge_at(&c.caller, &target, "CALLS", c.line),
                        );
                    }
                    Err(()) => unresolved += 1,
                },
                CallKind::New(written) => match ix.type_ref(written, f) {
                    Ok((key, true)) => add_edge(
                        &mut pending,
                        &mut edge_set,
                        edge_at(&c.caller, &key, "CALLS", c.line),
                    ),
                    Ok((key, false)) => {
                        note_external(&seen, &mut external, &key, "Class");
                        external_calls += 1;
                        add_edge(
                            &mut pending,
                            &mut edge_set,
                            edge_at(&c.caller, &key, "CALLS", c.line),
                        );
                    }
                    Err(()) => unresolved += 1,
                },
                CallKind::Super(method) => {
                    // The extends chain, where this tree holds it.
                    let resolved = ix
                        .types
                        .get(&c.in_type)
                        .map(|(_, ext)| ext.clone())
                        .unwrap_or_default()
                        .iter()
                        .find_map(|base| {
                            let (key, true) = ix.type_ref(base, f).ok()? else {
                                return None;
                            };
                            ix.find_method(f, &key, method, 0)
                        });
                    match resolved {
                        Some(owner) => add_edge(
                            &mut pending,
                            &mut edge_set,
                            edge_at(&c.caller, &format!("{owner}.{method}"), "CALLS", c.line),
                        ),
                        None => unresolved += 1,
                    }
                }
            }
        }

        for (src, written, ty, line) in &f.clauses {
            match ix.type_ref(written, f) {
                Ok((key, in_tree)) => {
                    if !in_tree {
                        let label = match ty.as_str() {
                            "IMPLEMENTS" => "Interface",
                            "ANNOTATED_BY" => "Annotation",
                            _ => "Class",
                        };
                        note_external(&seen, &mut external, &key, label);
                    }
                    add_edge(&mut pending, &mut edge_set, edge_at(src, &key, ty, *line));
                }
                Err(()) => unresolved += 1,
            }
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
            "{unresolved} call(s) left unresolved: a receiver names a value \
             whose type only a compiler would know"
        ));
    }
    if external_calls > 0 {
        out.notes.push(format!(
            "{external_calls} call(s) into other libraries, recorded as external \
             nodes carrying the qualified name and nothing else"
        ));
    }
    if merged > 0 {
        out.notes.push(format!(
            "{merged} declaration(s) shared a key across files; the first seen \
             is kept"
        ));
    }
    out
}
