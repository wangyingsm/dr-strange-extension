//! The cross-file half: type references bound against the parsed type set —
//! same package first, then imports, then wildcards, then `java.lang` —
//! calls walked onto method surfaces and up the in-tree extends chain, and
//! every clause turned into its edge. Once, over every chunk's facts, in
//! chunk order.

use crate::{CallKind, Edge, FileFacts, Injection, Node, Props, STEREOTYPES, edge_at};
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

/// What one `USES_TYPE` edge says: the positions its owner names the type in,
/// the first name it does so under, and how the dependency arrives.
struct Uses {
    roles: BTreeSet<String>,
    name: String,
    injected: Option<&'static str>,
}

impl Uses {
    /// Keeps the strongest evidence seen: an annotation someone wrote outranks
    /// a constructor Spring injects by convention.
    fn take_injection(&mut self, found: Option<&&Injection>) {
        match found.map(|i| i.kind.as_str()) {
            Some("annotation") => self.injected = Some("annotation"),
            Some("constructor") if self.injected.is_none() => {
                self.injected = Some("constructor");
            }
            _ => {}
        }
    }
}

/// How much an injection's evidence is worth, ranked as `test_flag`'s is.
fn injection_confidence(kind: &str) -> &'static str {
    match kind {
        "annotation" => "definitive",
        _ => "strong",
    }
}

/// What a parsed type offers a resolver: its method surface and its bases.
struct TypeEntry {
    methods: BTreeSet<String>,
    /// The superclass, or an interface's extended interfaces.
    extends: Vec<String>,
    /// A class's interfaces.
    implements: Vec<String>,
    /// Its label, so a port (`Interface`) is told from a type that writes its
    /// own methods.
    kind: String,
}

/// A bean Spring would instantiate: the name it is registered under, and
/// whether it wins a tie.
struct Bean {
    name: String,
    primary: bool,
}

/// The containers Spring fills with every bean of the element type.
const MULTIVALUED: &[&str] = &["List", "Set", "Collection", "Iterable", "Map", "Stream"];

/// The last segment of a qualified name — an annotation is matched by the name
/// Spring knows it by, however the source spelled the import.
fn simple(qualified: &str) -> &str {
    qualified.rsplit('.').next().unwrap_or(qualified)
}

/// Spring's default bean name: the simple name decapitalized, except that two
/// leading capitals stay as written (`URLFetcher`), as `Introspector` does.
fn decapitalize(simple: &str) -> String {
    let mut cs = simple.chars();
    match (cs.next(), cs.next()) {
        (Some(a), Some(b)) if a.is_uppercase() && b.is_uppercase() => simple.to_string(),
        (Some(a), _) => a.to_lowercase().chain(simple.chars().skip(1)).collect(),
        _ => simple.to_string(),
    }
}

/// The bean name an annotation's arguments state: a lone string literal, or a
/// `name = "…"` / `value = "…"` member. Nothing when they say something else.
fn named_arg(args: &str) -> Option<String> {
    let literal = |s: &str| -> Option<String> {
        let t = s.trim();
        t.strip_prefix('"')
            .and_then(|t| t.strip_suffix('"'))
            .map(str::to_string)
    };
    if let Some(one) = literal(args) {
        return Some(one);
    }
    args.split(',').find_map(|part| {
        let (key, value) = part.split_once('=')?;
        matches!(key.trim(), "name" | "value")
            .then(|| literal(value))
            .flatten()
    })
}

struct Index {
    /// Every type this run parsed, by qualified name.
    types: BTreeMap<String, TypeEntry>,
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

    /// Does `type_key` answer `method`, on itself or up its in-tree ancestry —
    /// superclass and interfaces both? Returns the type that declares it.
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
        let entry = self.types.get(type_key)?;
        if entry.methods.contains(method) {
            return Some(type_key.to_string());
        }
        for base in entry.extends.iter().chain(&entry.implements) {
            if let Ok((base_key, true)) = self.type_ref(base, f)
                && let Some(found) = self.find_method(f, &base_key, method, depth + 1)
            {
                return Some(found);
            }
        }
        None
    }

    /// Every in-tree type above `type_key` — superclasses and interfaces, all
    /// the way up. A port names its implementations through this, reversed.
    fn ancestors(&self, f: &FileFacts, type_key: &str, depth: usize, out: &mut BTreeSet<String>) {
        if depth > 16 {
            return;
        }
        let Some(entry) = self.types.get(type_key) else {
            return;
        };
        for base in entry.extends.iter().chain(&entry.implements) {
            if let Ok((base_key, true)) = self.type_ref(base, f)
                && out.insert(base_key.clone())
            {
                self.ancestors(f, &base_key, depth + 1, out);
            }
        }
    }
}

/// One call site asking for a port's implementation.
struct Site<'a> {
    /// The interface the receiver is typed by, whose implementations are the
    /// candidates.
    port: &'a str,
    method: &'a str,
    /// The port method the source named — where the written edge went.
    via: &'a str,
    /// The type holding the receiver, whose injection points say how the
    /// dependency arrives.
    owner: &'a str,
    injection: Option<&'a Injection>,
}

/// What the container's rules conclude at one call site.
enum Picked {
    /// Not a port, or not an injected receiver: nothing to say.
    Silent,
    /// The bean, or the container-full of beans, that Spring wires.
    Wired(Vec<Wired>),
    /// Several beans implement the port and nothing says which — the case
    /// Spring itself refuses to start on.
    Ambiguous,
}

/// One edge the container implies: the bean's method, the rule that picked it,
/// and the port method the source actually named.
struct Wired {
    dst: String,
    by: &'static str,
    confidence: &'static str,
    via: String,
}

/// What Spring's rules need to pick a bean at one call site.
struct Dispatch<'a> {
    ix: &'a Index,
    beans: &'a BTreeMap<String, Bean>,
    implementors: &'a BTreeMap<String, BTreeSet<String>>,
    collections: &'a BTreeSet<(String, String)>,
}

impl Dispatch<'_> {
    /// Which bean's method a call through a port reaches — Spring's own rules,
    /// in its own order, and nothing where the container itself would refuse
    /// to start.
    ///
    /// Only an injected receiver is dispatched: when a port value arrives any
    /// other way, whoever built it chose the implementation, and that is data
    /// flow a parser must not guess at. A type that asks for a container-full
    /// of the port has declared it takes every bean of it, which counts for
    /// any value of that port inside it.
    fn pick(&self, at: &Site<'_>, f: &FileFacts) -> Picked {
        let by_the_container = self
            .collections
            .contains(&(at.owner.to_string(), at.port.to_string()));
        if at.injection.is_none() && !by_the_container {
            return Picked::Silent;
        }
        if self.ix.types.get(at.port).map(|e| e.kind.as_str()) != Some("Interface") {
            return Picked::Silent;
        }
        // A candidate is a bean that answers the method; the type that
        // declares it is where the code is, which may be a base class.
        let candidates: Vec<(&String, &Bean, String)> = self
            .implementors
            .get(at.port)
            .into_iter()
            .flatten()
            .filter_map(|key| {
                let bean = self.beans.get(key)?;
                let owner = self.ix.find_method(f, key, at.method, 0)?;
                Some((key, bean, format!("{owner}.{}", at.method)))
            })
            .collect();
        if candidates.is_empty() {
            return Picked::Silent;
        }
        let one = |dst: String, by: &'static str, confidence: &'static str| -> Picked {
            Picked::Wired(vec![Wired {
                dst,
                by,
                confidence,
                via: at.via.to_string(),
            }])
        };

        if let Some(name) = at
            .injection
            .and_then(|i| i.qualifier.as_deref())
            .and_then(named_arg)
            && let Some((_, _, dst)) = candidates.iter().find(|(_, b, _)| b.name == name)
        {
            return one(dst.clone(), "spring-qualifier", "high");
        }
        if let [(_, _, dst)] = candidates.as_slice() {
            return one(dst.clone(), "spring-bean", "high");
        }
        if let [(_, _, dst)] = candidates
            .iter()
            .filter(|(_, b, _)| b.primary)
            .collect::<Vec<_>>()
            .as_slice()
        {
            return one(dst.to_string(), "spring-primary", "high");
        }
        // A container-full of the port: Spring injects every bean of it, and
        // which one runs at a given moment is not written anywhere.
        if by_the_container {
            return Picked::Wired(
                candidates
                    .iter()
                    .map(|(_, _, dst)| Wired {
                        dst: dst.clone(),
                        by: "spring-bean-collection",
                        confidence: "medium",
                        via: at.via.to_string(),
                    })
                    .collect(),
            );
        }
        Picked::Ambiguous
    }
}

/// A dispatch edge, carrying the rule that picked the bean.
fn wired_edge(src: &str, line: u64, w: &Wired) -> Edge {
    let mut e = edge_at(src, &w.dst, "CALLS", line);
    e.props
        .insert("_resolved_by".into(), Value::String(w.by.into()));
    e.props
        .insert("_confidence".into(), Value::String(w.confidence.into()));
    e.props.insert(
        "_via".into(),
        serde_json::json!({
            "$desc": "the port method the source named, which the container wires to this",
            "$value": w.via.clone(),
        }),
    );
    e
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
            ix.types.entry(t.key.clone()).or_insert_with(|| TypeEntry {
                methods: t.methods.iter().cloned().collect(),
                extends: t.extends.clone(),
                implements: t.implements.clone(),
                kind: t.kind.clone(),
            });
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

    // ---- receiver-typing indexes (params, locals, fields, returns) --------
    // Hints resolve to IN-TREE types only: an external receiver's methods
    // are a checker's business and stay out (counted unresolved).
    let mut hint_ix: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut returns_ix: BTreeMap<String, String> = BTreeMap::new();
    for f in &all {
        for h in &f.hints {
            if let Ok((key, true)) = ix.type_ref(&h.written, f) {
                hint_ix
                    .entry((h.caller.clone(), h.name.clone()))
                    .or_insert(key);
            }
        }
        for (mkey, written) in &f.returns {
            if let Ok((key, true)) = ix.type_ref(written, f) {
                returns_ix.entry(mkey.clone()).or_insert(key);
            }
        }
    }

    // Injection points by the type and the name they arrive under — which is
    // how a receiver reaches one: `repo.save()` in a bean asks what wired
    // `repo`.
    let mut inject_ix: BTreeMap<(String, String), &Injection> = BTreeMap::new();
    // The ports an owner asks for by the container-full: Spring hands these
    // every bean of the element type, so a call through one reaches them all.
    let mut collections: BTreeSet<(String, String)> = BTreeSet::new();
    for f in &all {
        for i in &f.injections {
            inject_ix
                .entry((i.owner.clone(), i.name.clone()))
                .or_insert(i);
            if !MULTIVALUED.contains(&simple(&i.written)) {
                continue;
            }
            for arg in &i.args {
                if let Ok((key, true)) = ix.type_ref(arg, f) {
                    collections.insert((i.owner.clone(), key));
                }
            }
        }
    }

    // ---- the bean index ---------------------------------------------------
    // A stereotype makes a type a bean; a `@Bean` method makes one of what it
    // returns. The name is what a `@Qualifier` matches against.
    let mut beans: BTreeMap<String, Bean> = BTreeMap::new();
    for f in &all {
        for c in &f.clauses {
            if c.ty != "ANNOTATED_BY" {
                continue;
            }
            let anno = simple(&c.written);
            if STEREOTYPES.contains(&anno) {
                if !ix.types.contains_key(&c.src) {
                    continue;
                }
                let name = c
                    .args
                    .as_deref()
                    .and_then(named_arg)
                    .unwrap_or_else(|| decapitalize(simple(&c.src)));
                beans.entry(c.src.clone()).or_insert(Bean {
                    name,
                    primary: false,
                });
            } else if anno == "Bean" {
                let Some(returned) = returns_ix.get(&c.src) else {
                    continue;
                };
                let name = c
                    .args
                    .as_deref()
                    .and_then(named_arg)
                    .unwrap_or_else(|| simple(&c.src).to_string());
                beans.entry(returned.clone()).or_insert(Bean {
                    name,
                    primary: false,
                });
            }
        }
    }
    // `@Primary` is read after the beans exist: it ranks one, it does not
    // declare it.
    for f in &all {
        for c in &f.clauses {
            if c.ty == "ANNOTATED_BY" && simple(&c.written) == "Primary" {
                if let Some(b) = beans.get_mut(&c.src) {
                    b.primary = true;
                }
                if let Some(returned) = returns_ix.get(&c.src)
                    && let Some(b) = beans.get_mut(returned)
                {
                    b.primary = true;
                }
            }
        }
    }

    // Every in-tree type under each port it answers to, transitively, so
    // `FooImpl extends AbstractFoo implements Foo` is one of `Foo`'s.
    let mut implementors: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for f in &all {
        for t in &f.types {
            let mut ports: BTreeSet<String> = BTreeSet::new();
            ix.ancestors(f, &t.key, 0, &mut ports);
            for port in ports {
                implementors.entry(port).or_default().insert(t.key.clone());
            }
        }
    }
    let dispatch = Dispatch {
        ix: &ix,
        beans: &beans,
        implementors: &implementors,
        collections: &collections,
    };
    let mut wired_calls = 0usize;
    let mut ambiguous_ports = 0usize;

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
        unresolved += f.opaque;
        // A value's stated class: a local or parameter of the caller, else a
        // field of the enclosing type (bare field names reach here too).
        let type_of = |caller: &str, in_type: &str, name: &str| -> Option<&String> {
            hint_ix
                .get(&(caller.to_string(), name.to_string()))
                .or_else(|| hint_ix.get(&(in_type.to_string(), name.to_string())))
        };

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
                CallKind::Receiver { name, method } => {
                    let hit = type_of(&c.caller, &c.in_type, name).and_then(|class| {
                        ix.find_method(f, class, method, 0)
                            .map(|o| (class.clone(), o))
                    });
                    match hit {
                        Some((declared, owner)) => {
                            let via = format!("{owner}.{method}");
                            add_edge(
                                &mut pending,
                                &mut edge_set,
                                stamped(
                                    &c.caller,
                                    &via,
                                    c.line,
                                    "receiver",
                                    &format!("{name}.{method}"),
                                ),
                            );
                            let site = Site {
                                port: &declared,
                                method,
                                via: &via,
                                owner: &c.in_type,
                                injection: inject_ix
                                    .get(&(c.in_type.clone(), name.clone()))
                                    .copied(),
                            };
                            match dispatch.pick(&site, f) {
                                Picked::Wired(ws) => {
                                    for w in &ws {
                                        add_edge(
                                            &mut pending,
                                            &mut edge_set,
                                            wired_edge(&c.caller, c.line, w),
                                        );
                                        wired_calls += 1;
                                    }
                                }
                                Picked::Ambiguous => ambiguous_ports += 1,
                                Picked::Silent => {}
                            }
                        }
                        None => unresolved += 1,
                    }
                }
                CallKind::SelfField { field, method } => {
                    let hit = hint_ix
                        .get(&(c.in_type.clone(), field.clone()))
                        .and_then(|class| {
                            ix.find_method(f, class, method, 0)
                                .map(|o| (class.clone(), o))
                        });
                    match hit {
                        Some((declared, owner)) => {
                            let via = format!("{owner}.{method}");
                            add_edge(
                                &mut pending,
                                &mut edge_set,
                                stamped(
                                    &c.caller,
                                    &via,
                                    c.line,
                                    "self-attr",
                                    &format!("this.{field}.{method}"),
                                ),
                            );
                            let site = Site {
                                port: &declared,
                                method,
                                via: &via,
                                owner: &c.in_type,
                                injection: inject_ix
                                    .get(&(c.in_type.clone(), field.clone()))
                                    .copied(),
                            };
                            match dispatch.pick(&site, f) {
                                Picked::Wired(ws) => {
                                    for w in &ws {
                                        add_edge(
                                            &mut pending,
                                            &mut edge_set,
                                            wired_edge(&c.caller, c.line, w),
                                        );
                                        wired_calls += 1;
                                    }
                                }
                                Picked::Ambiguous => ambiguous_ports += 1,
                                Picked::Silent => {}
                            }
                        }
                        None => unresolved += 1,
                    }
                }
                CallKind::FieldChain { obj, field, method } => {
                    let hit = type_of(&c.caller, &c.in_type, obj)
                        .and_then(|c1| {
                            hint_ix
                                .get(&(c1.clone(), field.clone()))
                                .map(|c2| (c1.clone(), c2.clone()))
                        })
                        .and_then(|(holder, declared)| {
                            ix.find_method(f, &declared, method, 0)
                                .map(|o| (holder, declared, o))
                        });
                    match hit {
                        Some((holder, declared, owner)) => {
                            let via = format!("{owner}.{method}");
                            add_edge(
                                &mut pending,
                                &mut edge_set,
                                stamped(
                                    &c.caller,
                                    &via,
                                    c.line,
                                    "receiver",
                                    &format!("{obj}.{field}.{method}"),
                                ),
                            );
                            let site = Site {
                                port: &declared,
                                method,
                                via: &via,
                                owner: &holder,
                                injection: inject_ix.get(&(holder.clone(), field.clone())).copied(),
                            };
                            match dispatch.pick(&site, f) {
                                Picked::Wired(ws) => {
                                    for w in &ws {
                                        add_edge(
                                            &mut pending,
                                            &mut edge_set,
                                            wired_edge(&c.caller, c.line, w),
                                        );
                                        wired_calls += 1;
                                    }
                                }
                                Picked::Ambiguous => ambiguous_ports += 1,
                                Picked::Silent => {}
                            }
                        }
                        None => unresolved += 1,
                    }
                }
                CallKind::StaticChain {
                    type_ref,
                    first,
                    method,
                } => {
                    // The factory's declared return names what the chain
                    // holds; both hops must be this tree's to say anything.
                    let hit = ix
                        .type_ref(type_ref, f)
                        .ok()
                        .filter(|(_, in_tree)| *in_tree)
                        .and_then(|(k, _)| ix.find_method(f, &k, first, 0))
                        .and_then(|owner| returns_ix.get(&format!("{owner}.{first}")))
                        .and_then(|ret| ix.find_method(f, ret, method, 0));
                    match hit {
                        Some(owner) => add_edge(
                            &mut pending,
                            &mut edge_set,
                            stamped(
                                &c.caller,
                                &format!("{owner}.{method}"),
                                c.line,
                                "chain",
                                &format!("{type_ref}.{first}().{method}"),
                            ),
                        ),
                        None => unresolved += 1,
                    }
                }
                CallKind::Super(method) => {
                    // The extends chain, where this tree holds it.
                    let resolved = ix
                        .types
                        .get(&c.in_type)
                        .map(|e| e.extends.clone())
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

        for c in &f.clauses {
            match ix.type_ref(&c.written, f) {
                Ok((key, in_tree)) => {
                    if !in_tree {
                        let label = match c.ty.as_str() {
                            "IMPLEMENTS" => "Interface",
                            "ANNOTATED_BY" => "Annotation",
                            _ => "Class",
                        };
                        note_external(&seen, &mut external, &key, label);
                    }
                    let mut e = edge_at(&c.src, &key, &c.ty, c.line);
                    if let Some(args) = &c.args {
                        e.props.insert(
                            "args".into(),
                            serde_json::json!({
                                "$desc": "the annotation's arguments, as the source writes them",
                                "$value": args,
                            }),
                        );
                    }
                    add_edge(&mut pending, &mut edge_set, e);
                }
                Err(()) => unresolved += 1,
            }
        }
    }
    out.edges = pending;

    // A declared type is a dependency as surely as a call is. Until this
    // existed, asking what a type's change would break saw only who
    // constructed one — never who takes it as a parameter, returns it, or
    // holds it in a field.
    //
    // The same in-tree rule as the hints above, and it has to hold here: the
    // implied-node pass below mints a bare `Type` for any endpoint nothing
    // declares, so an edge to `String` would put `String` in the graph.
    let mut uses: BTreeMap<(String, String), Uses> = BTreeMap::new();
    let mut foreign_type_refs = 0usize;
    for f in &all {
        for r in &f.type_refs {
            match ix.type_ref(&r.written, f) {
                // A type that mentions itself is a real shape and a useless
                // edge.
                Ok((key, true)) if key != r.owner => {
                    let slot = uses.entry((r.owner.clone(), key)).or_insert_with(|| Uses {
                        roles: BTreeSet::new(),
                        name: r.name.clone(),
                        injected: None,
                    });
                    slot.roles.insert(r.role.clone());
                    slot.take_injection(inject_ix.get(&(r.owner.clone(), r.name.clone())));
                }
                Ok((_, true)) => {}
                _ => foreign_type_refs += 1,
            }
        }
    }
    let type_ref_edges = uses.len();
    let injected_edges = uses.values().filter(|u| u.injected.is_some()).count();
    for ((owner, target), u) in uses {
        let mut e = edge_at(&owner, &target, "USES_TYPE", 0);
        e.props.remove("line");
        e.props.insert(
            "role".into(),
            serde_json::json!({
                "$desc": "the type position this declaration names the type in",
                "$value": u.roles.iter().cloned().collect::<Vec<_>>().join(", "),
            }),
        );
        if !u.name.is_empty() {
            e.props.insert("name".into(), Value::String(u.name));
        }
        if let Some(kind) = u.injected {
            e.props.insert(
                "injected".into(),
                serde_json::json!({
                    "$desc": "what says a container wires this dependency",
                    "$value": kind,
                }),
            );
            e.props.insert(
                "_injected_confidence".into(),
                Value::String(injection_confidence(kind).into()),
            );
        }
        out.edges.push(e);
    }

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
    if type_ref_edges > 0 || foreign_type_refs > 0 {
        out.notes.push(format!(
            "{type_ref_edges} type reference(s) recorded as `USES_TYPE` — what a \
             declaration's fields, parameters and returns are typed by; \
             {foreign_type_refs} more name types this tree does not declare and \
             are left as written text on the node"
        ));
    }
    if injected_edges > 0 {
        out.notes.push(format!(
            "{injected_edges} of those are injection points — a dependency a \
             container wires, marked `injected` with what the evidence was"
        ));
    }
    if wired_calls > 0 {
        out.notes.push(format!(
            "{wired_calls} call(s) through an injected port also reach the bean \
             Spring wires, stamped with the rule that picked it; the call to the \
             port stays beside it, as the source wrote it"
        ));
    }
    if ambiguous_ports > 0 {
        out.notes.push(format!(
            "{ambiguous_ports} call(s) stop at their port: several beans \
             implement it and no `@Primary` or `@Qualifier` says which — what \
             Spring itself refuses to start on"
        ));
    }
    out
}
