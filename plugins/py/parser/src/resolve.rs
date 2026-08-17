//! The cross-file half: imports bound against the parsed module set, dotted
//! chains walked module-by-module, bases turned into EXTENDS — once, over
//! every chunk's facts together, in chunk order. The result must not depend
//! on where the chunk boundaries fell.

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

/// Callable without being declared anywhere; a call to one is not an edge
/// worth recording. Builtins, and the exceptions everyone raises.
const BUILTINS: &[&str] = &[
    "abs",
    "all",
    "any",
    "bool",
    "bytearray",
    "bytes",
    "callable",
    "chr",
    "classmethod",
    "compile",
    "dict",
    "dir",
    "divmod",
    "enumerate",
    "eval",
    "exec",
    "filter",
    "float",
    "format",
    "frozenset",
    "getattr",
    "globals",
    "hasattr",
    "hash",
    "id",
    "input",
    "int",
    "isinstance",
    "issubclass",
    "iter",
    "len",
    "list",
    "locals",
    "map",
    "max",
    "min",
    "next",
    "object",
    "open",
    "ord",
    "pow",
    "print",
    "property",
    "range",
    "repr",
    "reversed",
    "round",
    "set",
    "setattr",
    "sorted",
    "staticmethod",
    "str",
    "sum",
    "super",
    "tuple",
    "type",
    "vars",
    "zip",
    "Exception",
    "ValueError",
    "TypeError",
    "KeyError",
    "IndexError",
    "AttributeError",
    "RuntimeError",
    "StopIteration",
    "StopAsyncIteration",
    "NotImplementedError",
    "OSError",
    "IOError",
    "FileNotFoundError",
    "PermissionError",
    "TimeoutError",
    "ConnectionError",
    "InterruptedError",
    "KeyboardInterrupt",
    "SystemExit",
    "ArithmeticError",
    "ZeroDivisionError",
    "OverflowError",
    "AssertionError",
    "LookupError",
    "NameError",
    "UnicodeDecodeError",
    "UnicodeEncodeError",
    "MemoryError",
    "RecursionError",
    "BaseException",
    "Warning",
    "DeprecationWarning",
    "UserWarning",
    "RuntimeWarning",
    "FutureWarning",
];

struct Index {
    /// Every module this run parsed, by dotted id.
    modules: BTreeSet<String>,
    /// module → name → (key, is-plain-value), everything declared at
    /// module level.
    decls: BTreeMap<String, BTreeMap<String, (String, bool)>>,
    /// module → name → key, the star-import surface (`__all__`, or public).
    exports: BTreeMap<String, BTreeMap<String, String>>,
    /// class key → its method names, for `self.m()`.
    class_methods: BTreeMap<String, BTreeSet<String>>,
}

impl Index {
    /// A name looked up in a module: a declaration, or — for the star
    /// surface — an export.
    fn decl(&self, module: &str, name: &str) -> Option<String> {
        self.decls.get(module)?.get(name).map(|(k, _)| k.clone())
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
    if have.is_none() || (have == Some("Package") && label != "Package") {
        external.insert(key.to_string(), label);
    }
}

pub fn assemble(all: Vec<FileFacts>) -> Assembled {
    let mut out = Assembled::default();

    // ---- indexes ----------------------------------------------------------
    let mut ix = Index {
        modules: BTreeSet::new(),
        decls: BTreeMap::new(),
        exports: BTreeMap::new(),
        class_methods: BTreeMap::new(),
    };
    for f in &all {
        if f.failed {
            out.skipped += 1;
            continue;
        }
        ix.modules.insert(f.module_id.clone());
        let d = ix.decls.entry(f.module_id.clone()).or_default();
        for l in &f.decls {
            d.entry(l.name.clone())
                .or_insert_with(|| (l.key.clone(), l.value));
        }
        let e = ix.exports.entry(f.module_id.clone()).or_default();
        for l in &f.exports {
            e.entry(l.name.clone()).or_insert_with(|| l.key.clone());
        }
        for (class, method) in &f.class_methods {
            ix.class_methods
                .entry(format!("{}.{class}", f.module_id))
                .or_default()
                .insert(method.clone());
        }
    }

    // ---- nodes, first seen wins ------------------------------------------
    // Two files rarely share a module id; when they do (a `.py` beside its
    // `.pyi` stub), the first seen is kept and the count says so.
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

    // ---- the package tree -------------------------------------------------
    // `mypkg` contains `mypkg.core` contains `mypkg.core.utils` — derivable
    // from the dotted ids, and only claimed where both ends were parsed.
    for m in &ix.modules {
        if let Some((parent, _)) = m.rsplit_once('.')
            && ix.modules.contains(parent)
        {
            add_edge(
                &mut pending,
                &mut edge_set,
                Edge {
                    src: parent.to_string(),
                    dst: m.clone(),
                    ty: "CONTAINS".into(),
                    props: Props::new(),
                },
            );
        }
    }

    // ---- IMPORTS ----------------------------------------------------------
    let mut external: BTreeMap<String, &'static str> = BTreeMap::new();
    for f in &all {
        for (target, line) in &f.imports {
            if ix.modules.contains(target) {
                add_edge(
                    &mut pending,
                    &mut edge_set,
                    edge_at(&f.module_id, target, "IMPORTS", *line),
                );
            } else {
                note_external(&seen, &mut external, target, "Package");
                add_edge(
                    &mut pending,
                    &mut edge_set,
                    edge_at(&f.module_id, target, "IMPORTS", *line),
                );
            }
        }
    }

    // ---- receiver-typing indexes (annotations, constructors, bases) -------
    // A lightweight resolver for TYPE names only: no externals are minted —
    // a class outside this tree contributes no methods anyway.
    let classes: BTreeSet<String> = all.iter().flat_map(|f| f.classes.iter().cloned()).collect();
    let file_bindings = |f: &FileFacts| -> BTreeMap<String, (String, String)> {
        f.bindings
            .iter()
            .map(|b| (b.local.clone(), (b.target.clone(), b.member.clone())))
            .collect()
    };
    let decl_of = |f: &FileFacts,
                   bindings: &BTreeMap<String, (String, String)>,
                   written: &str|
     -> Option<String> {
        let parts: Vec<&str> = written.split('.').collect();
        match parts.as_slice() {
            [name] => {
                if let Some((key, _)) = ix.decls.get(&f.module_id).and_then(|d| d.get(*name)) {
                    return Some(key.clone());
                }
                let (target, member) = bindings.get(*name)?;
                if member.is_empty() {
                    return None;
                }
                ix.decl(target, member)
            }
            [root, rest @ ..] => {
                let (target, member) = bindings.get(*root)?;
                let mut module = target.clone();
                if !member.is_empty() {
                    module = format!("{module}.{member}");
                }
                for part in &rest[..rest.len() - 1] {
                    module = format!("{module}.{part}");
                }
                ix.decl(&module, rest.last()?)
            }
            [] => None,
        }
    };
    // Declared returns first: a constructed hint may type through a factory.
    let mut returns_ix: BTreeMap<String, String> = BTreeMap::new();
    let mut bases_ix: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for f in &all {
        let bindings = file_bindings(f);
        for (fn_key, written) in &f.returns {
            if let Some(key) = decl_of(f, &bindings, written)
                && classes.contains(&key)
            {
                returns_ix.entry(fn_key.clone()).or_insert(key);
            }
        }
        for (class_key, written, _) in &f.bases {
            if let Some(key) = decl_of(f, &bindings, written)
                && classes.contains(&key)
            {
                bases_ix.entry(class_key.clone()).or_default().push(key);
            }
        }
    }
    let mut hint_ix: BTreeMap<(String, String), String> = BTreeMap::new();
    for f in &all {
        let bindings = file_bindings(f);
        for h in &f.hints {
            let resolved = decl_of(f, &bindings, &h.written);
            let class = match resolved {
                Some(key) if classes.contains(&key) => Some(key),
                // A constructed hint may name a factory: its declared
                // return, resolved in ITS file's context, types the value.
                Some(key) if h.constructed => returns_ix.get(&key).cloned(),
                _ => None,
            };
            if let Some(class) = class {
                hint_ix
                    .entry((h.caller.clone(), h.name.clone()))
                    .or_insert(class);
            }
        }
    }
    // The method a class answers `name` with: its own, else the bases',
    // left to right depth-first — Python's resolution order, near enough
    // for declared facts.
    fn method_walk(
        ix: &Index,
        bases: &BTreeMap<String, Vec<String>>,
        class: &str,
        name: &str,
        depth: usize,
    ) -> Option<String> {
        if depth == 0 {
            return None;
        }
        if ix
            .class_methods
            .get(class)
            .is_some_and(|ms| ms.contains(name))
        {
            return Some(format!("{class}.{name}"));
        }
        bases
            .get(class)
            .into_iter()
            .flatten()
            .find_map(|b| method_walk(ix, bases, b, name, depth - 1))
    }

    // ---- calls and bases ---------------------------------------------------
    let mut unresolved = 0usize;
    // The unresolved ledger (P1): a queryable UnresolvedRef node per
    // (file, written form), attributed to the caller's file for fold
    // ownership; the reason rides the edge. Counted still — but shown.
    let mut unresolved_nodes: std::collections::BTreeMap<String, Node> =
        std::collections::BTreeMap::new();
    let mut external_calls = 0usize;
    for f in &all {
        let bindings: BTreeMap<&str, (&str, &str)> = f
            .bindings
            .iter()
            .map(|b| (b.local.as_str(), (b.target.as_str(), b.member.as_str())))
            .collect();
        unresolved += f.opaque;

        // A bare name: local declaration first (shadowing is file truth),
        // then an imported binding, then the star-import surfaces in import
        // order, then a builtin, then nothing a parser can say.
        let resolve_plain = |name: &str,
                             external: &mut BTreeMap<String, &'static str>,
                             external_calls: &mut usize|
         -> Option<Option<String>> {
            // Some(Some(key)) = resolved; Some(None) = skip silently;
            // None = unresolved.
            // A plain value assignment yields to an import binding of the
            // same name: `try: from x import y / except: y = None` is the
            // fallback idiom, and the import is the primary.
            let local = ix
                .decls
                .get(&f.module_id)
                .and_then(|d| d.get(name))
                .cloned();
            if let Some((key, is_value)) = &local
                && !(*is_value && bindings.contains_key(name))
            {
                return Some(Some(key.clone()));
            }
            if let Some((target, member)) = bindings.get(name) {
                if member.is_empty() {
                    // The binding is a module; calling one is not a thing.
                    return None;
                }
                // `from t import x` — x may be t's declaration, or the
                // submodule t.x (imported for its side or passed around).
                if let Some(key) = ix.decl(target, member) {
                    return Some(Some(key));
                }
                let submodule = format!("{target}.{member}");
                if ix.modules.contains(&submodule) {
                    return None; // calling a module is not a thing
                }
                if !ix.modules.contains(*target) {
                    let key = format!("{target}.{member}");
                    note_external(&seen, external, &key, "Function");
                    note_external(&seen, external, target, "Package");
                    *external_calls += 1;
                    return Some(Some(key));
                }
                return None;
            }
            for star in &f.stars {
                if let Some(key) = ix.exports.get(star).and_then(|e| e.get(name)) {
                    return Some(Some(key.clone()));
                }
            }
            if BUILTINS.contains(&name) {
                return Some(None);
            }
            None
        };

        // A dotted chain rooted at a bare name: the root must be an import
        // binding, and each step walks a submodule until the last resolves
        // as a declaration. One step onto anything else is an attribute of a
        // value — a checker's business.
        let resolve_chain = |parts: &[String],
                             external: &mut BTreeMap<String, &'static str>,
                             external_calls: &mut usize|
         -> Option<Option<String>> {
            let root = parts.first()?.as_str();
            let name = parts.last()?.as_str();
            let mid = &parts[1..parts.len() - 1];

            let (mut module, member) = match bindings.get(root) {
                Some((t, m)) => (t.to_string(), *m),
                None => return None,
            };
            if !member.is_empty() {
                // `from a import b` then `b.c()` — b must be the submodule.
                module = format!("{module}.{member}");
            }
            for part in mid {
                module = format!("{module}.{part}");
            }
            if ix.modules.contains(&module) {
                return ix.decl(&module, name).map(Some);
            }
            // Not parsed here: external if its root is, an attribute of a
            // value otherwise.
            let root_module = module.split('.').next().unwrap_or(&module).to_owned();
            if !ix.modules.contains(&root_module) && ix.decl(&f.module_id, root).is_none() {
                let key = format!("{module}.{name}");
                note_external(&seen, external, &key, "Function");
                note_external(&seen, external, &root_module, "Package");
                *external_calls += 1;
                return Some(Some(key));
            }
            None
        };

        for c in &f.calls {
            // A stamp the resolution path may sharpen past the kind default.
            let mut how: Option<(&'static str, &'static str)> = None;
            let resolved = match &c.kind {
                CallKind::Plain(name) => resolve_plain(name, &mut external, &mut external_calls),
                CallKind::Chain(parts) => {
                    let chained = resolve_chain(parts, &mut external, &mut external_calls);
                    match chained {
                        Some(r) => Some(r),
                        // Not an import chain. Two declared readings remain
                        // for `x.m()`: a typed receiver (annotation or
                        // constructor), or a class receiver (`C.make()`).
                        None if parts.len() == 2 => {
                            let (root, m) = (&parts[0], &parts[1]);
                            if let Some(class) = hint_ix.get(&(c.caller.clone(), root.clone()))
                                && let Some(key) = method_walk(&ix, &bases_ix, class, m, 5)
                            {
                                how = Some(("receiver", "high"));
                                Some(Some(key))
                            } else if let Some(class_key) = {
                                let b = file_bindings(f);
                                decl_of(f, &b, root).filter(|k| classes.contains(k))
                            } && let Some(key) =
                                method_walk(&ix, &bases_ix, &class_key, m, 5)
                            {
                                how = Some(("class", "high"));
                                Some(Some(key))
                            } else {
                                None
                            }
                        }
                        None => None,
                    }
                }
                CallKind::This { class, method } => {
                    let class_key = format!("{}.{class}", f.module_id);
                    method_walk(&ix, &bases_ix, &class_key, method, 5).map(Some)
                }
                CallKind::Super { class, method } => {
                    // The bases only: `super()` never lands on the own class.
                    let class_key = format!("{}.{class}", f.module_id);
                    bases_ix
                        .get(&class_key)
                        .into_iter()
                        .flatten()
                        .find_map(|b| method_walk(&ix, &bases_ix, b, method, 5))
                        .map(Some)
                }
                CallKind::SelfAttr {
                    class,
                    attr,
                    method,
                } => {
                    let class_key = format!("{}.{class}", f.module_id);
                    hint_ix
                        .get(&(class_key, attr.clone()))
                        .and_then(|t| method_walk(&ix, &bases_ix, t, method, 5))
                        .map(Some)
                }
            };
            let (written, strategy, band) = match &c.kind {
                CallKind::Plain(name) => (name.clone(), "name", "medium"),
                CallKind::Chain(parts) => (parts.join("."), "chain", "medium"),
                CallKind::This { method, .. } => (format!("self.{method}"), "self-class", "high"),
                CallKind::Super { method, .. } => (format!("super().{method}"), "super", "high"),
                CallKind::SelfAttr { attr, method, .. } => {
                    (format!("self.{attr}.{method}"), "self-attr", "high")
                }
            };
            let (strategy, band) = how.unwrap_or((strategy, band));
            match resolved {
                Some(Some(key)) => {
                    let mut e = edge_at(&c.caller, &key, "CALLS", c.line);
                    e.props
                        .insert("_resolved_by".into(), Value::String(strategy.into()));
                    e.props
                        .insert("_confidence".into(), Value::String(band.into()));
                    e.props
                        .insert("_ref".into(), Value::String(written.clone()));
                    add_edge(&mut pending, &mut edge_set, e);
                }
                Some(None) => {}
                None => {
                    unresolved += 1;
                    let key = format!("?::{}::{written}", f.file);
                    unresolved_nodes.entry(key.clone()).or_insert_with(|| Node {
                        key: key.clone(),
                        label: "UnresolvedRef".into(),
                        extra_labels: Vec::new(),
                        props: {
                            let mut p = Props::new();
                            p.insert("name".into(), Value::String(written.clone()));
                            p.insert("file".into(), Value::String(f.file.clone()));
                            p
                        },
                    });
                    let mut e = edge_at(&c.caller, &key, "CALLS", c.line);
                    e.props
                        .insert("_resolved_by".into(), Value::String("unresolved".into()));
                    e.props
                        .insert("_confidence".into(), Value::String("none".into()));
                    e.props.insert("_ref".into(), Value::String(written));
                    e.props.insert(
                        "_reason".into(),
                        Value::String(
                            "name or receiver not resolvable from imports or scope".into(),
                        ),
                    );
                    add_edge(&mut pending, &mut edge_set, e);
                }
            }
        }

        for (class_key, written, line) in &f.bases {
            let parts: Vec<String> = written.split('.').map(str::to_string).collect();
            let resolved = if parts.len() == 1 {
                resolve_plain(&parts[0], &mut external, &mut external_calls)
            } else {
                resolve_chain(&parts, &mut external, &mut external_calls)
            };
            match resolved {
                Some(Some(key)) => {
                    if !seen.contains(&key) {
                        // A base is a class; say so on the stand-in.
                        external.insert(key.clone(), "Class");
                    }
                    add_edge(
                        &mut pending,
                        &mut edge_set,
                        edge_at(class_key, &key, "EXTENDS", *line),
                    );
                }
                // `object`, `Exception` — extending a builtin says little
                // worth a stand-in.
                Some(None) => {}
                None => unresolved += 1,
            }
        }
    }
    // String references (P2): a literal that names a known symbol becomes a
    // REFERENCES edge — never CALLS, never a ledger entry when it misses
    // (most strings are not code).
    for f in &all {
        for (caller, text, line) in &f.string_refs {
            if seen.contains(text) && seen.contains(caller) {
                let mut e = edge_at(caller, text, "REFERENCES", *line);
                e.props
                    .insert("_resolved_by".into(), Value::String("string-name".into()));
                e.props
                    .insert("_confidence".into(), Value::String("high".into()));
                e.props
                    .insert("_ref".into(), Value::String((*text).clone()));
                add_edge(&mut pending, &mut edge_set, e);
            }
        }
    }
    // Into `seen` as well: a ledger key is an edge target, and the implied-
    // node pass below would otherwise mint a second, bare node for it.
    seen.extend(unresolved_nodes.keys().cloned());
    out.nodes.extend(unresolved_nodes.into_values());
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
            "{unresolved} call(s) left unresolved: a method or attribute names \
             a value whose type only a checker would know"
        ));
    }
    if external_calls > 0 {
        out.notes.push(format!(
            "{external_calls} call(s) into other packages, recorded as external \
             nodes carrying the dotted path and nothing else"
        ));
    }
    if merged > 0 {
        out.notes.push(format!(
            "{merged} declaration(s) shared a key across files — a stub beside \
             its module, or a name rebound; the first seen is kept"
        ));
    }
    out
}
