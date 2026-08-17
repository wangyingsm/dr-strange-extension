//! The cross-file half: imports resolved against the parsed file set,
//! exports chased through re-export chains, calls bound, and `implements` /
//! `extends` clauses turned into edges — once, over every chunk's facts
//! together, in chunk order. The result must not depend on where the chunk
//! boundaries fell, and does not: everything keys on file-order-stable
//! indexes, never on how the facts were batched.

use crate::{CallKind, EXTENSIONS, Edge, FileFacts, Node, Props, edge_at};
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

/// Everything resolution reads, built once over all facts.
struct Index {
    /// file path (as parsed) → module id.
    module_of_file: BTreeMap<String, String>,
    /// module id → file path, to resolve a module's own specifiers.
    file_of_module: BTreeMap<String, String>,
    /// module id → name → (key, callable).
    decls: BTreeMap<String, BTreeMap<String, (String, bool)>>,
    /// module id → export name → key.
    exports: BTreeMap<String, BTreeMap<String, String>>,
    /// module id → (published, original, specifier).
    reexports: BTreeMap<String, Vec<(String, String, String)>>,
    /// module id → `export * from` specifiers.
    reexport_all: BTreeMap<String, Vec<String>>,
    /// class key → its method names, for `this.m()`.
    class_methods: BTreeMap<String, BTreeSet<String>>,
}

enum Resolved {
    Module(String),
    External(String),
    Miss,
}

impl Index {
    /// A relative specifier resolves against the files this run actually
    /// parsed — certain, no filesystem guessing. A bare one names another
    /// package's surface.
    fn resolve_spec(&self, from_file: &str, spec: &str) -> Resolved {
        if !spec.starts_with('.') {
            return Resolved::External(bare_package(spec));
        }
        let base = crate::parent_dir(from_file);
        let joined = normalize(&if base.is_empty() {
            spec.to_string()
        } else {
            format!("{base}/{spec}")
        });
        // `./x.js` may mean `x.ts` on disk — ESM imports write the emitted
        // extension. Probe the stem across every claimed extension.
        let stem = match joined.rsplit_once('.') {
            Some((s, ext)) if EXTENSIONS.contains(&ext) => s.to_string(),
            _ => joined.clone(),
        };
        if let Some(m) = self.module_of_file.get(&joined) {
            return Resolved::Module(m.clone());
        }
        for ext in EXTENSIONS {
            if let Some(m) = self.module_of_file.get(&format!("{stem}.{ext}")) {
                return Resolved::Module(m.clone());
            }
        }
        for ext in EXTENSIONS {
            if let Some(m) = self.module_of_file.get(&format!("{stem}/index.{ext}")) {
                return Resolved::Module(m.clone());
            }
        }
        Resolved::Miss
    }

    /// What `module` exports under `name`, chased through re-export chains —
    /// a barrel file republishes another module's surface, and the chain is
    /// finite unless it cycles, which is not a thing to chase.
    fn lookup_export(&self, module: &str, name: &str, depth: usize) -> Option<String> {
        if depth > 16 {
            return None;
        }
        if let Some(key) = self.exports.get(module).and_then(|e| e.get(name)) {
            return Some(key.clone());
        }
        let file = self.file_of_module.get(module)?;
        if let Some(res) = self.reexports.get(module) {
            for (published, original, spec) in res {
                if published == name
                    && let Resolved::Module(target) = self.resolve_spec(file, spec)
                {
                    return self.lookup_export(&target, original, depth + 1);
                }
            }
        }
        if let Some(alls) = self.reexport_all.get(module) {
            for spec in alls {
                if let Resolved::Module(target) = self.resolve_spec(file, spec)
                    && let Some(key) = self.lookup_export(&target, name, depth + 1)
                {
                    return Some(key);
                }
            }
        }
        None
    }
}

/// External stand-ins, strengthened as more is learned: an import alone says
/// only "a package"; a call through it names a function too.
fn note_external(
    seen: &BTreeMap<String, usize>,
    external: &mut BTreeMap<String, &'static str>,
    key: &str,
    label: &'static str,
) {
    if seen.contains_key(key) {
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
        module_of_file: BTreeMap::new(),
        file_of_module: BTreeMap::new(),
        decls: BTreeMap::new(),
        exports: BTreeMap::new(),
        reexports: BTreeMap::new(),
        reexport_all: BTreeMap::new(),
        class_methods: BTreeMap::new(),
    };
    for f in &all {
        if f.failed {
            out.skipped += 1;
            continue;
        }
        ix.module_of_file
            .insert(f.file.clone(), f.module_id.clone());
        ix.file_of_module
            .entry(f.module_id.clone())
            .or_insert_with(|| f.file.clone());
        let d = ix.decls.entry(f.module_id.clone()).or_default();
        for l in &f.decls {
            d.entry(l.name.clone())
                .or_insert_with(|| (l.key.clone(), l.callable));
        }
        let e = ix.exports.entry(f.module_id.clone()).or_default();
        for ex in &f.exports {
            e.entry(ex.name.clone()).or_insert_with(|| ex.key.clone());
        }
        // A declaration exported at its site is an export under its own name.
        let prefix = format!("{}.", f.module_id);
        for n in &f.nodes {
            if n.props.get("visibility").and_then(Value::as_str) == Some("exported")
                && let Some(name) = n.key.strip_prefix(&prefix)
                && !name.contains('.')
            {
                e.entry(name.to_string()).or_insert_with(|| n.key.clone());
            }
        }
        for r in &f.reexports {
            ix.reexports.entry(f.module_id.clone()).or_default().push((
                r.name.clone(),
                r.original.clone(),
                r.specifier.clone(),
            ));
        }
        for spec in &f.reexport_all {
            ix.reexport_all
                .entry(f.module_id.clone())
                .or_default()
                .push(spec.clone());
        }
        for (class, method) in &f.class_methods {
            ix.class_methods
                .entry(format!("{}.{class}", f.module_id))
                .or_default()
                .insert(method.clone());
        }
    }

    // ---- nodes, first seen wins ------------------------------------------
    // TypeScript merges same-name declarations; across files the first seen
    // is kept and the count says so.
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut merged = 0usize;
    for f in &all {
        for n in &f.nodes {
            if seen.contains_key(&n.key) {
                merged += 1;
                continue;
            }
            seen.insert(n.key.clone(), out.nodes.len());
            out.nodes.push(n.clone());
        }
    }

    let mut edge_set: BTreeSet<(String, String, String)> = BTreeSet::new();
    let mut pending_edges: Vec<Edge> = Vec::new();
    let add_edge =
        |pending: &mut Vec<Edge>, set: &mut BTreeSet<(String, String, String)>, e: Edge| {
            if set.insert((e.src.clone(), e.ty.clone(), e.dst.clone())) {
                pending.push(e);
            }
        };
    for f in &all {
        for e in &f.edges {
            add_edge(&mut pending_edges, &mut edge_set, e.clone());
        }
    }

    // ---- packages ---------------------------------------------------------
    let mut packages: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for f in &all {
        if !f.failed {
            packages
                .entry(f.package.clone())
                .or_default()
                .push(f.module_id.clone());
        }
    }
    for (pkg, modules) in &packages {
        if !seen.contains_key(pkg) {
            let mut props = Props::new();
            props.insert("name".into(), Value::String(pkg.clone()));
            seen.insert(pkg.clone(), out.nodes.len());
            out.nodes.push(Node {
                key: pkg.clone(),
                label: "Package".into(),
                extra_labels: Vec::new(),
                props,
            });
        }
        for m in modules {
            if m != pkg {
                add_edge(
                    &mut pending_edges,
                    &mut edge_set,
                    Edge {
                        src: pkg.clone(),
                        dst: m.clone(),
                        ty: "CONTAINS".into(),
                        props: Props::new(),
                    },
                );
            }
        }
    }

    // ---- IMPORTS ----------------------------------------------------------
    let mut external: BTreeMap<String, &'static str> = BTreeMap::new();
    let mut missed_specs = 0usize;
    for f in &all {
        for (spec, line) in &f.imports {
            match ix.resolve_spec(&f.file, spec) {
                Resolved::Module(target) => add_edge(
                    &mut pending_edges,
                    &mut edge_set,
                    edge_at(&f.module_id, &target, "IMPORTS", *line),
                ),
                Resolved::External(pkg) => {
                    note_external(&seen, &mut external, &pkg, "Package");
                    add_edge(
                        &mut pending_edges,
                        &mut edge_set,
                        edge_at(&f.module_id, &pkg, "IMPORTS", *line),
                    );
                }
                Resolved::Miss => missed_specs += 1,
            }
        }
    }

    // ---- receiver-typing indexes (annotations, new, factories, bases) -----
    // A lightweight resolver for TYPE names only — no externals minted: a
    // class outside this tree contributes no methods anyway.
    let classes: BTreeSet<String> = all.iter().flat_map(|f| f.classes.iter().cloned()).collect();
    let type_decl =
        |f: &FileFacts, bindings: &BTreeMap<&str, (&str, &str)>, written: &str| -> Option<String> {
            match written.split_once('.') {
                None => {
                    if let Some((key, _)) = ix.decls.get(&f.module_id).and_then(|d| d.get(written))
                    {
                        return Some(key.clone());
                    }
                    let (imported, spec) = bindings.get(written)?;
                    if *imported == "*" {
                        return None;
                    }
                    match ix.resolve_spec(&f.file, spec) {
                        Resolved::Module(target) => {
                            let name = if *imported == "default" {
                                "default"
                            } else {
                                imported
                            };
                            ix.lookup_export(&target, name, 0)
                        }
                        _ => None,
                    }
                }
                Some((ns, name)) => {
                    let (imported, spec) = bindings.get(ns)?;
                    if *imported != "*" {
                        return None;
                    }
                    match ix.resolve_spec(&f.file, spec) {
                        Resolved::Module(target) => ix.lookup_export(&target, name, 0),
                        _ => None,
                    }
                }
            }
        };
    fn file_bindings(f: &FileFacts) -> BTreeMap<&str, (&str, &str)> {
        f.bindings
            .iter()
            .map(|b| {
                (
                    b.local.as_str(),
                    (b.imported.as_str(), b.specifier.as_str()),
                )
            })
            .collect()
    }
    // Declared returns first: a constructed hint may type through a factory.
    let mut returns_ix: BTreeMap<String, String> = BTreeMap::new();
    let mut bases_ix: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for f in &all {
        let bindings = file_bindings(f);
        for (fn_key, written) in &f.returns {
            if let Some(key) = type_decl(f, &bindings, written)
                && classes.contains(&key)
            {
                returns_ix.entry(fn_key.clone()).or_insert(key);
            }
        }
        for (class_key, written, ty, _) in &f.clauses {
            if ty == "EXTENDS"
                && let Some(key) = type_decl(f, &bindings, written)
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
            let resolved = type_decl(f, &bindings, &h.written);
            let class = match resolved {
                Some(key) if classes.contains(&key) => Some(key),
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
    // The method a class answers `name` with: its own, else the base
    // chain's, depth-capped.
    fn method_walk(
        methods: &BTreeMap<String, BTreeSet<String>>,
        bases: &BTreeMap<String, Vec<String>>,
        class: &str,
        name: &str,
        depth: usize,
    ) -> Option<String> {
        if depth == 0 {
            return None;
        }
        if methods.get(class).is_some_and(|ms| ms.contains(name)) {
            return Some(format!("{class}.{name}"));
        }
        bases
            .get(class)
            .into_iter()
            .flatten()
            .find_map(|b| method_walk(methods, bases, b, name, depth - 1))
    }
    // A resolution stamp for the typed paths, matching the family shape.
    let stamped = |src: &str, dst: &str, line: u64, strategy: &str, written: &str| -> Edge {
        let mut e = edge_at(src, dst, "CALLS", line);
        e.props
            .insert("_resolved_by".into(), Value::String(strategy.into()));
        e.props
            .insert("_confidence".into(), Value::String("high".into()));
        e.props.insert("_ref".into(), Value::String(written.into()));
        e
    };

    // ---- calls and clauses -------------------------------------------------
    let mut unresolved = 0usize;
    let mut external_calls = 0usize;
    for f in &all {
        let bindings: BTreeMap<&str, (&str, &str)> = f
            .bindings
            .iter()
            .map(|b| {
                (
                    b.local.as_str(),
                    (b.imported.as_str(), b.specifier.as_str()),
                )
            })
            .collect();
        unresolved += f.opaque;

        // A bare name: a local declaration (shadowing is file truth), or an
        // imported binding, or nothing a parser can say.
        let resolve_name = |name: &str,
                            callable_only: bool,
                            external: &mut BTreeMap<String, &'static str>,
                            external_calls: &mut usize|
         -> Option<String> {
            if let Some((key, callable)) = ix.decls.get(&f.module_id).and_then(|d| d.get(name)) {
                return (!callable_only || *callable).then(|| key.clone());
            }
            let (imported, spec) = bindings.get(name)?;
            match ix.resolve_spec(&f.file, spec) {
                // Calling a whole-module binding — `const f = require('./x');
                // f()` — reaches whatever the module *is*: its default.
                Resolved::Module(target) => {
                    let name = if *imported == "*" {
                        "default"
                    } else {
                        imported
                    };
                    ix.lookup_export(&target, name, 0)
                }
                Resolved::External(pkg) => {
                    let key = if *imported == "default" || *imported == "*" {
                        pkg.clone()
                    } else {
                        format!("{pkg}.{imported}")
                    };
                    let label = if key == pkg { "Package" } else { "Function" };
                    note_external(&seen, external, &key, label);
                    if key != pkg {
                        note_external(&seen, external, &pkg, "Package");
                    }
                    *external_calls += 1;
                    Some(key)
                }
                Resolved::Miss => None,
            }
        };

        for c in &f.calls {
            match &c.kind {
                CallKind::Plain | CallKind::New => {
                    match resolve_name(&c.name, true, &mut external, &mut external_calls) {
                        Some(key) => add_edge(
                            &mut pending_edges,
                            &mut edge_set,
                            edge_at(&c.caller, &key, "CALLS", c.line),
                        ),
                        None => unresolved += 1,
                    }
                }
                CallKind::Qualified(obj) => match bindings.get(obj.as_str()) {
                    // A namespace import makes `obj.name` resolvable outright.
                    Some(("*", spec)) => match ix.resolve_spec(&f.file, spec) {
                        Resolved::Module(target) => match ix.lookup_export(&target, &c.name, 0) {
                            Some(key) => add_edge(
                                &mut pending_edges,
                                &mut edge_set,
                                edge_at(&c.caller, &key, "CALLS", c.line),
                            ),
                            None => unresolved += 1,
                        },
                        Resolved::External(pkg) => {
                            let key = format!("{pkg}.{}", c.name);
                            note_external(&seen, &mut external, &key, "Function");
                            note_external(&seen, &mut external, &pkg, "Package");
                            external_calls += 1;
                            add_edge(
                                &mut pending_edges,
                                &mut edge_set,
                                edge_at(&c.caller, &key, "CALLS", c.line),
                            );
                        }
                        Resolved::Miss => unresolved += 1,
                    },
                    // Otherwise two declared readings remain: a typed
                    // receiver (annotation, `new`, factory), or a class
                    // receiver (`Util.helper()`, statics included).
                    _ => {
                        let written = format!("{obj}.{}", c.name);
                        let receiver = hint_ix
                            .get(&(c.caller.clone(), obj.clone()))
                            .and_then(|class| {
                                method_walk(&ix.class_methods, &bases_ix, class, &c.name, 5)
                                    .map(|k| (k, "receiver"))
                            })
                            .or_else(|| {
                                let class = type_decl(f, &bindings, obj)?;
                                classes.contains(&class).then_some(())?;
                                method_walk(&ix.class_methods, &bases_ix, &class, &c.name, 5)
                                    .map(|k| (k, "class"))
                            });
                        match receiver {
                            Some((key, how)) => add_edge(
                                &mut pending_edges,
                                &mut edge_set,
                                stamped(&c.caller, &key, c.line, how, &written),
                            ),
                            None => unresolved += 1,
                        }
                    }
                },
                CallKind::This(class) => {
                    let class_key = format!("{}.{class}", f.module_id);
                    match method_walk(&ix.class_methods, &bases_ix, &class_key, &c.name, 5) {
                        Some(key) => add_edge(
                            &mut pending_edges,
                            &mut edge_set,
                            edge_at(&c.caller, &key, "CALLS", c.line),
                        ),
                        None => unresolved += 1,
                    }
                }
                CallKind::Super(class) => {
                    // The bases only: `super.m()` never lands on the own
                    // class, even when it overrides the name.
                    let class_key = format!("{}.{class}", f.module_id);
                    let hit = bases_ix
                        .get(&class_key)
                        .into_iter()
                        .flatten()
                        .find_map(|b| method_walk(&ix.class_methods, &bases_ix, b, &c.name, 5));
                    match hit {
                        Some(key) => add_edge(
                            &mut pending_edges,
                            &mut edge_set,
                            stamped(
                                &c.caller,
                                &key,
                                c.line,
                                "super",
                                &format!("super.{}", c.name),
                            ),
                        ),
                        None => unresolved += 1,
                    }
                }
                CallKind::SelfAttr { class, attr } => {
                    let class_key = format!("{}.{class}", f.module_id);
                    let hit = hint_ix
                        .get(&(class_key, attr.clone()))
                        .and_then(|t| method_walk(&ix.class_methods, &bases_ix, t, &c.name, 5));
                    match hit {
                        Some(key) => add_edge(
                            &mut pending_edges,
                            &mut edge_set,
                            stamped(
                                &c.caller,
                                &key,
                                c.line,
                                "self-attr",
                                &format!("this.{attr}.{}", c.name),
                            ),
                        ),
                        None => unresolved += 1,
                    }
                }
                CallKind::FieldChain { obj, field } => {
                    let hit = hint_ix
                        .get(&(c.caller.clone(), obj.clone()))
                        .and_then(|t1| hint_ix.get(&(t1.clone(), field.clone())))
                        .and_then(|t2| method_walk(&ix.class_methods, &bases_ix, t2, &c.name, 5));
                    match hit {
                        Some(key) => add_edge(
                            &mut pending_edges,
                            &mut edge_set,
                            stamped(
                                &c.caller,
                                &key,
                                c.line,
                                "receiver",
                                &format!("{obj}.{field}.{}", c.name),
                            ),
                        ),
                        None => unresolved += 1,
                    }
                }
            }
        }

        for (class_key, written, ty, line) in &f.clauses {
            let target = if let Some((obj, name)) = written.split_once('.') {
                match bindings.get(obj) {
                    Some(("*", spec)) => match ix.resolve_spec(&f.file, spec) {
                        Resolved::Module(target) => ix.lookup_export(&target, name, 0),
                        Resolved::External(pkg) => Some(format!("{pkg}.{name}")),
                        Resolved::Miss => None,
                    },
                    _ => None,
                }
            } else {
                resolve_name(written, false, &mut external, &mut external_calls)
            };
            match target {
                Some(key) => {
                    if !seen.contains_key(&key) {
                        // The clause says what kind of thing it names.
                        let label = if ty == "IMPLEMENTS" {
                            "Interface"
                        } else {
                            "Class"
                        };
                        note_external(&seen, &mut external, &key, label);
                    }
                    add_edge(
                        &mut pending_edges,
                        &mut edge_set,
                        edge_at(class_key, &key, ty, *line),
                    );
                }
                None => unresolved += 1,
            }
        }
    }
    out.edges = pending_edges;

    // ---- implied and external nodes --------------------------------------
    // An edge into nothing is unwritable; what an edge proves exists gets a
    // bare node saying only that.
    let mut implied: BTreeSet<String> = BTreeSet::new();
    for e in &out.edges {
        for key in [&e.src, &e.dst] {
            if !seen.contains_key(key.as_str()) && !external.contains_key(key.as_str()) {
                implied.insert(key.clone());
            }
        }
    }
    for key in implied {
        seen.insert(key.clone(), out.nodes.len());
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
            "{unresolved} call(s) left unresolved: a member call names a value \
             whose type only a checker would know"
        ));
    }
    if external_calls > 0 {
        out.notes.push(format!(
            "{external_calls} call(s) into other packages, recorded as external \
             nodes carrying the specifier and nothing else"
        ));
    }
    if missed_specs > 0 {
        out.notes.push(format!(
            "{missed_specs} import specifier(s) named no file in this tree — \
             assets, or files the digest never saw"
        ));
    }
    if merged > 0 {
        out.notes.push(format!(
            "{merged} declaration(s) shared a key across files — TypeScript \
             declaration merging keeps the first seen"
        ));
    }
    out
}

/// `react` → `react`; `@scope/pkg/sub` → `@scope/pkg`; `lodash/fp` → `lodash`.
fn bare_package(spec: &str) -> String {
    let mut parts = spec.split('/');
    match (parts.next(), parts.next()) {
        (Some(scope), Some(name)) if scope.starts_with('@') => format!("{scope}/{name}"),
        (Some(first), _) => first.to_string(),
        _ => spec.to_string(),
    }
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
