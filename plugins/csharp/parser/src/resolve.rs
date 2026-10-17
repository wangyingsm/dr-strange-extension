//! The cross-file half: type references bound against the parsed type set —
//! already-qualified first, then the enclosing namespaces outward, then
//! aliases, then `using` namespaces, then the run's `global using`s — calls
//! walked onto member surfaces and up the in-tree base chain, and every
//! clause turned into its edge. Once, over every chunk's facts, in chunk
//! order.

use crate::{
    Call, CallKind, Clause, Edge, FileFacts, Injection, Node, Props, TypeDecl, edge_at, walk,
};
use dr_strange_ext::route;
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

/// The types every C# file uses without importing them in its head: a miss
/// that names one of these is `System`, not an unresolved reference.
const SYSTEM_TYPES: &[&str] = &[
    "Object",
    "String",
    "Type",
    "Guid",
    "DateTime",
    "DateTimeOffset",
    "TimeSpan",
    "Uri",
    "Nullable",
    "Array",
    "Span",
    "Memory",
    "Tuple",
    "ValueTuple",
    "StringBuilder",
    "Console",
    "Math",
    "Convert",
    "Random",
    "Lazy",
    "Task",
    "ValueTask",
    "Action",
    "Func",
    "Predicate",
    "EventHandler",
    "EventArgs",
    "CancellationToken",
    "IDisposable",
    "IAsyncDisposable",
    "IEnumerable",
    "IEnumerator",
    "IAsyncEnumerable",
    "ICollection",
    "IList",
    "IReadOnlyList",
    "IReadOnlyCollection",
    "IDictionary",
    "IReadOnlyDictionary",
    "ISet",
    "List",
    "Dictionary",
    "HashSet",
    "Queue",
    "Stack",
    "KeyValuePair",
    "IEquatable",
    "IComparable",
    "IComparer",
    "IEqualityComparer",
    "Attribute",
    "Enum",
    "ValueType",
    "Delegate",
    "Exception",
    "ArgumentException",
    "ArgumentNullException",
    "ArgumentOutOfRangeException",
    "InvalidOperationException",
    "NotImplementedException",
    "NotSupportedException",
    "IndexOutOfRangeException",
    "KeyNotFoundException",
    "FormatException",
    "OperationCanceledException",
];

/// What a parsed type offers a resolver: its member surface and its bases.
struct TypeEntry {
    methods: BTreeSet<String>,
    bases: Vec<String>,
    /// Its label, so a service interface is told from a type that writes its
    /// own members.
    kind: String,
}

/// Every name this run parsed, and where each one lives.
struct Index {
    /// Every type this run parsed, by qualified name. A `partial` type is
    /// one entry holding every half's members.
    types: BTreeMap<String, TypeEntry>,
    /// namespace → simple name → qualified, for same-namespace resolution.
    by_namespace: BTreeMap<String, BTreeMap<String, String>>,
    namespaces: BTreeSet<String>,
    /// The `global using` namespaces, which are in scope in every file of
    /// the compilation.
    globals: Vec<String>,
    /// Every key this run declared — what tells an external reference from
    /// one the tree holds.
    declared: BTreeSet<String>,
    /// `(receiver type, name)` → the extension method's key. Flat across
    /// namespaces: an extension is reached through a `using`, and resolving
    /// on the pair is what the compiler itself does.
    extensions: BTreeMap<(String, String), String>,
}

impl Index {
    fn of(all: &[FileFacts], skipped: &mut usize) -> Self {
        let mut ix = Index {
            types: BTreeMap::new(),
            by_namespace: BTreeMap::new(),
            namespaces: BTreeSet::new(),
            globals: Vec::new(),
            declared: BTreeSet::new(),
            extensions: BTreeMap::new(),
        };
        for f in all {
            if f.failed {
                *skipped += 1;
                continue;
            }
            for ns in &f.namespaces {
                ix.namespaces.insert(ns.clone());
                ix.declared.insert(ns.clone());
            }
            for g in &f.globals {
                if !ix.globals.contains(g) {
                    ix.globals.push(g.clone());
                }
            }
            for n in &f.nodes {
                ix.declared.insert(n.key.clone());
            }
            for (recv, name, key) in &f.extensions {
                ix.extensions
                    .entry((recv.clone(), name.clone()))
                    .or_insert_with(|| key.clone());
            }
            for t in &f.types {
                ix.add_type(t);
            }
        }
        ix
    }

    /// One type declaration. A `partial` type is written in several files
    /// and is one type: the halves' member surfaces and bases are merged,
    /// or a call into the other half would resolve nowhere.
    fn add_type(&mut self, t: &TypeDecl) {
        let entry = self
            .types
            .entry(t.key.clone())
            .or_insert_with(|| TypeEntry {
                methods: BTreeSet::new(),
                bases: Vec::new(),
                kind: t.kind.clone(),
            });
        entry.methods.extend(t.methods.iter().cloned());
        for base in &t.bases {
            if !entry.bases.contains(base) {
                entry.bases.push(base.clone());
            }
        }
        if let Some((namespace, simple_name)) = t.key.rsplit_once('.') {
            // Only a type whose container is a namespace is reachable by its
            // simple name; a nested one is reached through its outer.
            if self.namespaces.contains(namespace) {
                self.by_namespace
                    .entry(namespace.to_string())
                    .or_default()
                    .entry(simple_name.to_string())
                    .or_insert_with(|| t.key.clone());
            }
        } else {
            self.by_namespace
                .entry(String::new())
                .or_default()
                .entry(t.key.clone())
                .or_insert_with(|| t.key.clone());
        }
    }

    /// A type reference resolved the way the compiler reads it:
    /// already-qualified, else the enclosing namespaces outward, else an
    /// alias, else a `using` namespace, else a `global using`, else
    /// `System`, else the name as written.
    ///
    /// `false` means an external — and unlike Java's, a C# external is
    /// often named without its namespace, because `using Microsoft.
    /// AspNetCore.Mvc` imports a whole namespace and nothing says which of
    /// them a bare `ControllerBase` came from. Recording the name the
    /// source wrote keeps the reference; inventing a namespace for it would
    /// not.
    fn type_ref(&self, written: &str, f: &FileFacts) -> (String, bool) {
        if written.contains('.') {
            return self.qualified_ref(written, f);
        }
        // C# searches the enclosing namespace and then outward through its
        // ancestors, which is why `Acme.Core.Deep` sees `Acme.Core`'s types.
        for scope in self.scopes(f) {
            if let Some(q) = self.by_namespace.get(&scope).and_then(|m| m.get(written)) {
                return (q.clone(), true);
            }
        }
        if let Some(alias) = f.imports.iter().find(|i| i.local == written) {
            let in_tree = self.types.contains_key(&alias.target);
            return (alias.target.clone(), in_tree);
        }
        for namespace in f.wildcards.iter().chain(&self.globals) {
            let candidate = format!("{namespace}.{written}");
            if self.types.contains_key(&candidate) {
                return (candidate, true);
            }
        }
        if SYSTEM_TYPES.contains(&written) {
            return (format!("System.{written}"), false);
        }
        (written.to_string(), false)
    }

    /// The namespaces a file's references are read in: each one it declares
    /// and every ancestor of it, innermost first, then the global
    /// namespace.
    fn scopes(&self, f: &FileFacts) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for ns in &f.namespaces {
            let mut rest = ns.as_str();
            loop {
                if !out.iter().any(|s| s == rest) {
                    out.push(rest.to_string());
                }
                match rest.rsplit_once('.') {
                    Some((head, _)) => rest = head,
                    None => break,
                }
            }
        }
        out.push(String::new());
        out
    }

    /// A dotted reference: in-tree if we parsed it, else an `Outer.Inner`
    /// through a scope or an alias, else a namespace path that reads as
    /// external.
    fn qualified_ref(&self, written: &str, f: &FileFacts) -> (String, bool) {
        if self.types.contains_key(written) {
            return (written.to_string(), true);
        }
        let (head, rest) = written.split_once('.').unwrap_or((written, ""));
        if let (outer, true) = self.type_ref(head, f) {
            let nested = format!("{outer}.{rest}");
            if self.types.contains_key(&nested) {
                return (nested, true);
            }
        }
        // A dotted name under a namespace this run parsed is that
        // namespace's type, wherever it was declared.
        for scope in self.scopes(f) {
            let candidate = if scope.is_empty() {
                written.to_string()
            } else {
                format!("{scope}.{written}")
            };
            if self.types.contains_key(&candidate) {
                return (candidate, true);
            }
        }
        (written.to_string(), false)
    }

    /// Does `type_key` answer `method`, on itself or up its in-tree bases?
    /// Returns the type that declares it.
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
        for base in &entry.bases {
            if let (base_key, true) = self.type_ref(base, f)
                && let Some(found) = self.find_method(f, &base_key, method, depth + 1)
            {
                return Some(found);
            }
        }
        None
    }

    /// The edge a base-type clause really is. C#'s syntax only says which
    /// base came first; the target's own declaration says what it is,
    /// wherever the tree holds it.
    fn base_edge<'a>(&self, clause: &'a Clause, key: &str) -> &'a str {
        if clause.ty == "ANNOTATED_BY" {
            return &clause.ty;
        }
        match self.types.get(key).map(|e| e.kind.as_str()) {
            Some("Interface") => "IMPLEMENTS",
            Some(_) => "EXTENDS",
            None => &clause.ty,
        }
    }

    /// An attribute reference, which C# lets the source write without the
    /// `Attribute` suffix the type actually carries.
    fn attribute_ref(&self, written: &str, f: &FileFacts) -> (String, bool) {
        match self.type_ref(written, f) {
            (key, true) => (key, true),
            first => match self.type_ref(&format!("{written}Attribute"), f) {
                (key, true) => (key, true),
                _ => first,
            },
        }
    }
}

/// The containers the DI container fills with every registration of the
/// element type.
const MULTIVALUED: &[&str] = &[
    "IEnumerable",
    "IAsyncEnumerable",
    "IList",
    "List",
    "ICollection",
    "IReadOnlyList",
    "IReadOnlyCollection",
    "Array",
];

/// One name for the two spellings .NET gives the same dependency: a
/// constructor takes `repo` and the field it fills is `_repo` or `Repo`.
/// That convention is what binds an injection point to the member a call
/// reaches it through — every codebase follows it and none guarantees it, so
/// where the names differ beyond this the type alone decides.
fn member_name(name: &str) -> String {
    let bare = name.trim_start_matches('_');
    let mut cs = bare.chars();
    match cs.next() {
        Some(first) => first.to_lowercase().chain(cs).collect(),
        None => String::new(),
    }
}

/// The last segment of a qualified name.
fn simple(qualified: &str) -> &str {
    qualified.rsplit('.').next().unwrap_or(qualified)
}

/// The key an attribute's arguments state: a lone string literal, or a
/// `Key = "…"` member. Nothing when they say something else.
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
        matches!(key.trim(), "Key" | "key" | "Name" | "name")
            .then(|| literal(value))
            .flatten()
    })
}

/// Every path-shaped string literal an attribute's arguments hold.
///
/// Read from the argument rather than from a table of attribute names, the
/// way a request is read from a call's: `"items"`, `Template = "items"` and
/// `"/items"` are one rule, and a framework nobody listed declares its
/// routes here too. The attribute's own name is consulted for one thing
/// only — whether a path written without a leading slash is a path at all
/// (see [`walk::as_path`]).
fn attr_paths(written: &str, args: &str) -> Vec<String> {
    let routing = route::Verb::from_name(walk::bare(written)).is_some()
        || matches!(walk::bare(written), "Route" | "RoutePrefix");
    let mut out = Vec::new();
    let mut rest = args;
    while let Some(start) = rest.find('"') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('"') else { break };
        if let Some(path) = walk::as_path(&after[..end], routing) {
            out.push(path);
        }
        rest = &after[end + 1..];
    }
    out
}

/// The verbs an attribute states: the one its own name carries, plus any
/// named as a string in an `[AcceptVerbs]`-style argument.
fn attr_verbs(written: &str) -> Vec<route::Verb> {
    route::Verb::from_name(walk::bare(written))
        .map(|v| vec![v])
        .unwrap_or_default()
}

/// What one handler's attributes declare, before the controller's prefix
/// applies.
#[derive(Default)]
struct Declared {
    verbs: Vec<route::Verb>,
    paths: BTreeSet<String>,
    line: u64,
}

/// One call site asking for a service's implementation.
struct Site<'a> {
    /// The interface the receiver is typed by, whose registrations are the
    /// candidates.
    service: &'a str,
    method: &'a str,
    /// The service member the source named — where the written edge went.
    via: &'a str,
    /// The type holding the receiver, whose injection points say how the
    /// dependency arrives.
    owner: &'a str,
    /// The member the call is in, and where in it — what a wired edge hangs
    /// off.
    caller: &'a str,
    line: u64,
    injection: Option<&'a Injection>,
}

/// What the container's rules conclude at one call site.
enum Picked {
    /// Not a service interface, or not an injected receiver: nothing to say.
    Silent,
    /// The implementation, or the container-full of them, that the registry
    /// resolves.
    Wired(Vec<Wired>),
    /// Several registrations answer and nothing in the source says which
    /// one runs.
    Ambiguous,
}

/// One edge the container implies: the implementation's member, the rule
/// that picked it, and the service member the source actually named.
struct Wired {
    dst: String,
    by: &'static str,
    confidence: &'static str,
    via: String,
}

/// What the container's rules need to pick an implementation at one call
/// site.
struct Dispatch<'a> {
    ix: &'a Index,
    /// service type → every implementation registered for it, with the key
    /// it was registered under.
    registry: &'a BTreeMap<String, Vec<(String, Option<String>)>>,
    collections: &'a BTreeSet<(String, String)>,
}

impl Dispatch<'_> {
    /// Which implementation's member a call through a service reaches — the
    /// container's own registry, read from the calls that fill it, and
    /// nothing where the source does not say.
    ///
    /// Only an injected receiver is dispatched: when a service value
    /// arrives any other way, whoever built it chose the implementation,
    /// and that is data flow a parser must not guess at. A type that asks
    /// for a container-full of the service has declared it takes every
    /// registration of it, which counts for any value of that service
    /// inside it.
    fn pick(&self, at: &Site<'_>, f: &FileFacts) -> Picked {
        let by_the_container = self
            .collections
            .contains(&(at.owner.to_string(), at.service.to_string()));
        if at.injection.is_none() && !by_the_container {
            return Picked::Silent;
        }
        if self.ix.types.get(at.service).map(|e| e.kind.as_str()) != Some("Interface") {
            return Picked::Silent;
        }
        let candidates = self.candidates(at, f);
        if candidates.is_empty() {
            return Picked::Silent;
        }
        self.rules(at, &candidates, by_the_container)
    }

    /// A candidate is a registered implementation that answers the member;
    /// the type that declares it is where the code is, which may be a base
    /// class.
    fn candidates(&self, at: &Site<'_>, f: &FileFacts) -> Vec<(Option<String>, String)> {
        self.registry
            .get(at.service)
            .into_iter()
            .flatten()
            .filter_map(|(key, registered_as)| {
                let owner = self.ix.find_method(f, key, at.method, 0)?;
                Some((registered_as.clone(), format!("{owner}.{}", at.method)))
            })
            .collect()
    }

    /// The container's order: a key names one, else one registration, else
    /// every registration of a container-full, else nothing.
    fn rules(
        &self,
        at: &Site<'_>,
        candidates: &[(Option<String>, String)],
        by_the_container: bool,
    ) -> Picked {
        let one = |dst: String, by: &'static str, confidence: &'static str| -> Picked {
            Picked::Wired(vec![Wired {
                dst,
                by,
                confidence,
                via: at.via.to_string(),
            }])
        };
        if let Some(key) = at
            .injection
            .and_then(|i| i.keyed.as_deref())
            .and_then(named_arg)
            && let Some((_, dst)) = candidates
                .iter()
                .find(|(registered_as, _)| registered_as.as_deref() == Some(key.as_str()))
        {
            return one(dst.clone(), "di-keyed", "high");
        }
        // A keyed registration answers only the key it was registered
        // under; an unkeyed injection point does not reach it.
        let plain: Vec<&(Option<String>, String)> = candidates
            .iter()
            .filter(|(registered_as, _)| registered_as.is_none())
            .collect();
        if let [(_, dst)] = plain.as_slice() {
            return one(dst.clone(), "di-registration", "high");
        }
        if by_the_container && !plain.is_empty() {
            return Picked::Wired(
                plain
                    .iter()
                    .map(|(_, dst)| Wired {
                        dst: dst.clone(),
                        by: "di-registration-collection",
                        confidence: "medium",
                        via: at.via.to_string(),
                    })
                    .collect(),
            );
        }
        if plain.len() > 1 {
            return Picked::Ambiguous;
        }
        Picked::Silent
    }
}

/// A dispatch edge, carrying the rule that picked the implementation.
fn wired_edge(src: &str, line: u64, w: &Wired) -> Edge {
    let mut e = edge_at(src, &w.dst, "CALLS", line);
    e.props
        .insert("_resolved_by".into(), Value::String(w.by.into()));
    e.props
        .insert("_confidence".into(), Value::String(w.confidence.into()));
    e.props.insert(
        "_via".into(),
        serde_json::json!({
            "$desc": "the service member the source named, which the container wires to this",
            "$value": w.via.clone(),
        }),
    );
    e
}

/// How much an injection's evidence is worth, ranked as `test_flag`'s is.
fn injection_confidence(kind: &str) -> &'static str {
    match kind {
        "attribute" => "definitive",
        _ => "strong",
    }
}

/// Everything the resolution passes read, built once over every chunk.
struct Resolved<'a> {
    ix: Index,
    /// `(scope, name)` → the in-tree type the source states for it. Hints
    /// resolve to IN-TREE types only: an external receiver's members are a
    /// checker's business and stay out (counted unresolved).
    hints: BTreeMap<(String, String), String>,
    /// `(scope, name)` → the type as *written*, in-tree or not. An extension
    /// method on an external type is still this tree's own method, so
    /// `value.Shout()` on a `string` resolves where `value.Trim()` cannot —
    /// and that needs the written name the `hints` filter discards.
    written: BTreeMap<(String, String), String>,
    /// A member key → the in-tree type it declares it returns.
    returns: BTreeMap<String, String>,
    /// Injection points by the type and the name they arrive under — which
    /// is how a receiver reaches one: `_repo.Save()` in a registered type
    /// asks what the container passed its constructor.
    injections: BTreeMap<(String, String), &'a Injection>,
    /// The services an owner asks for by the container-full: the container
    /// hands these every registration of the element type, so a call
    /// through one reaches them all.
    collections: BTreeSet<(String, String)>,
    /// service type → every implementation registered for it, with the key
    /// it was registered under.
    registry: BTreeMap<String, Vec<(String, Option<String>)>>,
}

impl<'a> Resolved<'a> {
    fn of(all: &'a [FileFacts], ix: Index) -> Self {
        let mut hints = BTreeMap::new();
        let mut written = BTreeMap::new();
        let mut returns = BTreeMap::new();
        for f in all {
            for (scope, name, text) in &f.written_types {
                written
                    .entry((scope.clone(), name.clone()))
                    .or_insert_with(|| text.clone());
            }
            for h in &f.hints {
                written
                    .entry((h.caller.clone(), h.name.clone()))
                    .or_insert_with(|| h.written.clone());
                if let (key, true) = ix.type_ref(&h.written, f) {
                    hints
                        .entry((h.caller.clone(), h.name.clone()))
                        .or_insert(key);
                }
            }
            for (member, written) in &f.returns {
                if let (key, true) = ix.type_ref(written, f) {
                    returns.entry(member.clone()).or_insert(key);
                }
            }
        }
        let (registry, registered) = registry(all, &ix);
        let (injections, collections) = injection_indexes(all, &ix, &registered);
        Self {
            ix,
            hints,
            written,
            returns,
            injections,
            collections,
            registry,
        }
    }

    fn dispatch(&self) -> Dispatch<'_> {
        Dispatch {
            ix: &self.ix,
            registry: &self.registry,
            collections: &self.collections,
        }
    }

    /// A value's stated type: a local or parameter of the caller, else a
    /// field or property of the enclosing type (bare member names reach
    /// here too).
    fn type_of(&self, caller: &str, in_type: &str, name: &str) -> Option<&String> {
        self.hints
            .get(&(caller.to_string(), name.to_string()))
            .or_else(|| self.hints.get(&(in_type.to_string(), name.to_string())))
    }

    /// The type written for a name, in-tree or not — see [`Self::written`].
    fn written_type(&self, caller: &str, in_type: &str, name: &str) -> Option<&String> {
        self.written
            .get(&(caller.to_string(), name.to_string()))
            .or_else(|| self.written.get(&(in_type.to_string(), name.to_string())))
    }

    fn injection_at(&self, owner: &str, name: &str) -> Option<&'a Injection> {
        self.injections
            .get(&(owner.to_string(), member_name(name)))
            .copied()
    }
}

/// What the container was told to build, read from the calls that tell it —
/// and the set of types it therefore constructs itself.
type Registry = (
    BTreeMap<String, Vec<(String, Option<String>)>>,
    BTreeSet<String>,
);

fn registry(all: &[FileFacts], ix: &Index) -> Registry {
    let mut registry: BTreeMap<String, Vec<(String, Option<String>)>> = BTreeMap::new();
    let mut registered: BTreeSet<String> = BTreeSet::new();
    for f in all {
        for r in &f.registrations {
            let ((service, true), (implementation, true)) = (
                ix.type_ref(&r.service, f),
                ix.type_ref(&r.implementation, f),
            ) else {
                continue;
            };
            registered.insert(implementation.clone());
            let entry = registry.entry(service).or_default();
            let row = (implementation, r.key.clone());
            if !entry.contains(&row) {
                entry.push(row);
            }
        }
    }
    (registry, registered)
}

/// The injection points this run states, and the services asked for by the
/// container-full.
///
/// Every constructor parameter is a candidate; the ones that count are the
/// ones the container itself passes — a registered type's — or the ones an
/// attribute asks for outright.
type Injections<'a> = (
    BTreeMap<(String, String), &'a Injection>,
    BTreeSet<(String, String)>,
);

fn injection_indexes<'a>(
    all: &'a [FileFacts],
    ix: &Index,
    registered: &BTreeSet<String>,
) -> Injections<'a> {
    let mut points = BTreeMap::new();
    let mut collections = BTreeSet::new();
    for f in all {
        for i in &f.injections {
            if i.kind != "attribute" && !registered.contains(&i.owner) {
                continue;
            }
            points
                .entry((i.owner.clone(), member_name(&i.name)))
                .or_insert(i);
            if !MULTIVALUED.contains(&simple(&i.written)) {
                continue;
            }
            for arg in &i.args {
                if let (key, true) = ix.type_ref(arg, f) {
                    collections.insert((i.owner.clone(), key));
                }
            }
        }
    }
    (points, collections)
}

/// Edges, deduplicated by `(src, type, dst)` — the same relation recorded
/// twice is one fact.
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

/// Nodes nothing in the tree declares, and what each reference proves of
/// them.
#[derive(Default)]
struct Externals {
    map: BTreeMap<String, &'static str>,
}

impl Externals {
    fn note(&mut self, declared: &BTreeSet<String>, key: &str, label: &'static str) {
        if declared.contains(key) {
            return;
        }
        let have = self.map.get(key).copied();
        // Bare `External` (a `using` proves only existence) yields to
        // anything more specific a clause or a call proves.
        if have.is_none() || (have == Some("External") && label != "External") {
            self.map.insert(key.to_string(), label);
        }
    }
}

/// The running account of what could not be done, and what was done by a
/// rule rather than by the source.
#[derive(Default)]
struct Account {
    unresolved: usize,
    external_calls: usize,
    merged: usize,
    wired: usize,
    ambiguous: usize,
    type_ref_edges: usize,
    foreign_type_refs: usize,
    injected_edges: usize,
    registrations: usize,
}

impl Account {
    fn notes(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut say = |n: usize, text: String| {
            if n > 0 {
                out.push(text);
            }
        };
        say(
            self.unresolved,
            format!(
                "{} call(s) left unresolved: a receiver names a value whose type \
                 only a compiler would know",
                self.unresolved
            ),
        );
        say(
            self.external_calls,
            format!(
                "{} call(s) into other libraries, recorded as external nodes \
                 carrying the qualified name and nothing else",
                self.external_calls
            ),
        );
        say(
            self.merged,
            format!(
                "{} declaration(s) shared a key across files; the first seen is kept \
                 — which is what a `partial` type is",
                self.merged
            ),
        );
        say(
            self.type_ref_edges + self.foreign_type_refs,
            format!(
                "{} type reference(s) recorded as `USES_TYPE` — what a declaration's \
                 fields, properties, parameters and returns are typed by; {} more name \
                 types this tree does not declare and are left as written text on the \
                 node",
                self.type_ref_edges, self.foreign_type_refs
            ),
        );
        say(
            self.registrations,
            format!(
                "{} service registration(s) read from the calls that make them — \
                 `AddScoped<IRepo, JpaRepo>()` and its kin",
                self.registrations
            ),
        );
        say(
            self.injected_edges,
            format!(
                "{} injection point(s) — a dependency the container passes a \
                 registered type's constructor, marked `injected` with what the \
                 evidence was",
                self.injected_edges
            ),
        );
        say(
            self.wired,
            format!(
                "{} call(s) through an injected service also reach the registered \
                 implementation, stamped with the rule that picked it; the call to \
                 the service stays beside it, as the source wrote it",
                self.wired
            ),
        );
        say(
            self.ambiguous,
            format!(
                "{} call(s) stop at their service: several implementations are \
                 registered for it and the order the registrations run in is not \
                 written in any one file",
                self.ambiguous
            ),
        );
        out
    }
}

/// A resolved edge, carrying the rule that read it.
fn stamped(src: &str, dst: &str, line: u64, strategy: &str, written: &str) -> Edge {
    let mut e = edge_at(src, dst, "CALLS", line);
    e.props
        .insert("_resolved_by".into(), Value::String(strategy.into()));
    e.props
        .insert("_confidence".into(), Value::String("high".into()));
    e.props.insert("_ref".into(), Value::String(written.into()));
    e
}

/// The accumulating facts every resolution pass writes into.
struct World {
    edges: Edges,
    ext: Externals,
    acct: Account,
}

impl World {
    /// The edges the per-file walk already drew.
    fn file_edges(&mut self, all: &[FileFacts]) {
        for f in all {
            for e in &f.edges {
                self.edges.add(e.clone());
            }
        }
    }

    /// The namespace tree, where both ends were parsed.
    fn namespace_tree(&mut self, ix: &Index) {
        for ns in &ix.namespaces {
            if let Some((parent, _)) = ns.rsplit_once('.')
                && ix.namespaces.contains(parent)
            {
                self.edges.add(Edge {
                    src: parent.to_string(),
                    dst: ns.clone(),
                    ty: "CONTAINS".into(),
                    props: Props::new(),
                });
            }
        }
    }

    /// `IMPORTS`, from each of the file's top-level types — C#'s `using` is
    /// file scoped, and the type is what a reader navigates by.
    fn imports(&mut self, all: &[FileFacts], ix: &Index) {
        for f in all {
            for (target, line) in &f.import_lines {
                if !ix.declared.contains(target) {
                    self.ext.note(&ix.declared, target, "External");
                }
                for t in &f.top_types {
                    self.edges.add(edge_at(t, target, "IMPORTS", *line));
                }
            }
        }
    }

    /// Every call one file makes.
    fn calls(&mut self, f: &FileFacts, r: &Resolved<'_>) {
        for c in &f.calls {
            match &c.kind {
                CallKind::Own(method) => self.own_call(f, c, method, r),
                CallKind::Static { type_ref, method } => {
                    self.static_call(f, c, (type_ref, method), r)
                }
                CallKind::New(written) => self.new_call(f, c, written, r),
                CallKind::Super(method) => self.super_call(f, c, method, r),
                CallKind::Receiver { name, method } => self.receiver_call(f, c, (name, method), r),
                CallKind::SelfField { field, method } => {
                    self.self_member_call(f, c, (field, method), r)
                }
                CallKind::FieldChain { obj, field, method } => {
                    self.member_chain_call(f, c, (obj, field, method), r)
                }
                CallKind::StaticChain {
                    type_ref,
                    first,
                    method,
                } => self.static_chain_call(f, c, (type_ref, first, method), r),
            }
        }
    }

    /// `Helper()`: the type's own surface and its in-tree bases, else a
    /// member a `using static` brought into scope.
    fn own_call(&mut self, f: &FileFacts, c: &Call, method: &str, r: &Resolved<'_>) {
        if let Some(owner) = r.ix.find_method(f, &c.in_type, method, 0) {
            self.edges.add(edge_at(
                &c.caller,
                &format!("{owner}.{method}"),
                "CALLS",
                c.line,
            ));
            return;
        }
        for imported in f.imports.iter().filter(|i| i.is_static) {
            let (holder, in_tree) = r.ix.type_ref(&imported.target, f);
            if in_tree {
                if let Some(owner) = r.ix.find_method(f, &holder, method, 0) {
                    self.edges.add(edge_at(
                        &c.caller,
                        &format!("{owner}.{method}"),
                        "CALLS",
                        c.line,
                    ));
                    return;
                }
                continue;
            }
            let target = format!("{holder}.{method}");
            self.ext.note(&r.ix.declared, &target, "Function");
            self.acct.external_calls += 1;
            self.edges.add(edge_at(&c.caller, &target, "CALLS", c.line));
            return;
        }
        self.acct.unresolved += 1;
    }

    /// `Helper.Create()`: through a type reference, written down.
    fn static_call(&mut self, f: &FileFacts, c: &Call, at: (&str, &str), r: &Resolved<'_>) {
        let (type_ref, method) = at;
        match r.ix.type_ref(type_ref, f) {
            (key, true) => match r.ix.find_method(f, &key, method, 0) {
                Some(owner) => self.edges.add(edge_at(
                    &c.caller,
                    &format!("{owner}.{method}"),
                    "CALLS",
                    c.line,
                )),
                None => self.acct.unresolved += 1,
            },
            (key, false) => {
                let target = format!("{key}.{method}");
                self.ext.note(&r.ix.declared, &target, "Function");
                self.ext.note(&r.ix.declared, &key, "External");
                self.acct.external_calls += 1;
                self.edges.add(edge_at(&c.caller, &target, "CALLS", c.line));
            }
        }
    }

    /// `new Helper()`: instantiation is a call to the type.
    fn new_call(&mut self, f: &FileFacts, c: &Call, written: &str, r: &Resolved<'_>) {
        let (key, in_tree) = r.ix.type_ref(written, f);
        if !in_tree {
            self.ext.note(&r.ix.declared, &key, "Class");
            self.acct.external_calls += 1;
        }
        self.edges.add(edge_at(&c.caller, &key, "CALLS", c.line));
    }

    /// `base.M()`: the base class, where this tree holds it.
    fn super_call(&mut self, f: &FileFacts, c: &Call, method: &str, r: &Resolved<'_>) {
        let resolved =
            r.ix.types
                .get(&c.in_type)
                .map(|e| e.bases.clone())
                .unwrap_or_default()
                .iter()
                .find_map(|base| {
                    let (key, true) = r.ix.type_ref(base, f) else {
                        return None;
                    };
                    r.ix.find_method(f, &key, method, 0)
                });
        match resolved {
            Some(owner) => self.edges.add(edge_at(
                &c.caller,
                &format!("{owner}.{method}"),
                "CALLS",
                c.line,
            )),
            None => self.acct.unresolved += 1,
        }
    }

    /// `log.Write()`: a value receiver, typed where the source states it.
    fn receiver_call(&mut self, f: &FileFacts, c: &Call, at: (&str, &str), r: &Resolved<'_>) {
        let (name, method) = at;
        let hit = r
            .type_of(&c.caller, &c.in_type, name)
            .and_then(|declared| {
                r.ix.find_method(f, declared, method, 0)
                    .map(|owner| (declared.clone(), format!("{owner}.{method}")))
            })
            // No member of that name: an extension method on the same type is
            // the other thing `value.Shout()` can mean, and the compiler picks
            // it by the receiver's static type. Its key is the whole target —
            // the method lives on the static class that declares it.
            .or_else(|| {
                let w = r.written_type(&c.caller, &c.in_type, name)?;
                let simple = w.rsplit('.').next().unwrap_or(w);
                r.ix.extensions
                    .get(&(simple.to_string(), method.to_string()))
                    .map(|key| (w.clone(), key.clone()))
            });
        let Some((declared, via)) = hit else {
            self.acct.unresolved += 1;
            return;
        };
        self.edges.add(stamped(
            &c.caller,
            &via,
            c.line,
            "receiver",
            &format!("{name}.{method}"),
        ));
        self.wire(
            &Site {
                service: &declared,
                method,
                via: &via,
                owner: &c.in_type,
                caller: &c.caller,
                line: c.line,
                injection: r.injection_at(&c.in_type, name),
            },
            r,
            f,
        );
    }

    /// `this._repo.Save()`: through the member's declared type.
    fn self_member_call(&mut self, f: &FileFacts, c: &Call, at: (&str, &str), r: &Resolved<'_>) {
        let (field, method) = at;
        let hit = r
            .hints
            .get(&(c.in_type.clone(), field.to_string()))
            .and_then(|declared| {
                r.ix.find_method(f, declared, method, 0)
                    .map(|owner| (declared.clone(), owner))
            });
        let Some((declared, owner)) = hit else {
            self.acct.unresolved += 1;
            return;
        };
        let via = format!("{owner}.{method}");
        self.edges.add(stamped(
            &c.caller,
            &via,
            c.line,
            "self-attr",
            &format!("this.{field}.{method}"),
        ));
        self.wire(
            &Site {
                service: &declared,
                method,
                via: &via,
                owner: &c.in_type,
                caller: &c.caller,
                line: c.line,
                injection: r.injection_at(&c.in_type, field),
            },
            r,
            f,
        );
    }

    /// `obj.Repo.Save()`: one member hop on a typed value.
    fn member_chain_call(
        &mut self,
        f: &FileFacts,
        c: &Call,
        at: (&str, &str, &str),
        r: &Resolved<'_>,
    ) {
        let (obj, field, method) = at;
        let hit = r
            .type_of(&c.caller, &c.in_type, obj)
            .and_then(|holder| {
                r.hints
                    .get(&(holder.clone(), field.to_string()))
                    .map(|declared| (holder.clone(), declared.clone()))
            })
            .and_then(|(holder, declared)| {
                r.ix.find_method(f, &declared, method, 0)
                    .map(|owner| (holder, declared, owner))
            });
        let Some((holder, declared, owner)) = hit else {
            self.acct.unresolved += 1;
            return;
        };
        let via = format!("{owner}.{method}");
        self.edges.add(stamped(
            &c.caller,
            &via,
            c.line,
            "receiver",
            &format!("{obj}.{field}.{method}"),
        ));
        self.wire(
            &Site {
                service: &declared,
                method,
                via: &via,
                owner: &holder,
                caller: &c.caller,
                line: c.line,
                injection: r.injection_at(&holder, field),
            },
            r,
            f,
        );
    }

    /// `Foo.Create().Bar()`: the factory's declared return names what the
    /// chain holds; both hops must be this tree's to say anything.
    fn static_chain_call(
        &mut self,
        f: &FileFacts,
        c: &Call,
        at: (&str, &str, &str),
        r: &Resolved<'_>,
    ) {
        let (type_ref, first, method) = at;
        let hit =
            r.ix.type_ref(type_ref, f)
                .1
                .then(|| r.ix.type_ref(type_ref, f).0)
                .and_then(|k| r.ix.find_method(f, &k, first, 0))
                .and_then(|owner| r.returns.get(&format!("{owner}.{first}")))
                .and_then(|ret| r.ix.find_method(f, ret, method, 0));
        match hit {
            Some(owner) => self.edges.add(stamped(
                &c.caller,
                &format!("{owner}.{method}"),
                c.line,
                "chain",
                &format!("{type_ref}.{first}().{method}"),
            )),
            None => self.acct.unresolved += 1,
        }
    }

    /// What the container does at one call site, beside what the source
    /// wrote.
    fn wire(&mut self, at: &Site<'_>, r: &Resolved<'_>, f: &FileFacts) {
        match r.dispatch().pick(at, f) {
            Picked::Wired(ws) => {
                for w in &ws {
                    self.edges.add(wired_edge(at.caller, at.line, w));
                    self.acct.wired += 1;
                }
            }
            Picked::Ambiguous => self.acct.ambiguous += 1,
            Picked::Silent => {}
        }
    }

    /// Base types and attributes, every one resolved the same way.
    fn clauses(&mut self, f: &FileFacts, r: &Resolved<'_>) {
        for c in &f.clauses {
            let resolved = if c.ty == "ANNOTATED_BY" {
                r.ix.attribute_ref(&c.written, f)
            } else {
                r.ix.type_ref(&c.written, f)
            };
            let (key, in_tree) = resolved;
            let ty = r.ix.base_edge(c, &key);
            if !in_tree {
                let label = match ty {
                    "IMPLEMENTS" => "Interface",
                    "ANNOTATED_BY" => "Annotation",
                    _ => "Class",
                };
                self.ext.note(&r.ix.declared, &key, label);
            }
            let mut e = edge_at(&c.src, &key, ty, c.line);
            if let Some(args) = &c.args {
                e.props.insert(
                    "args".into(),
                    serde_json::json!({
                        "$desc": "the attribute's arguments, as the source writes them",
                        "$value": args,
                    }),
                );
            }
            self.edges.add(e);
        }
    }

    /// A declared type is a dependency as surely as a call is: asking what a
    /// type's change would break must see who takes it as a parameter,
    /// returns it, or holds it in a field — not only who constructed one.
    ///
    /// The in-tree rule has to hold here: the implied-node pass mints a bare
    /// `Type` for any endpoint nothing declares, so an edge to `string`
    /// would put `string` in the graph.
    fn uses_types(&mut self, all: &[FileFacts], r: &Resolved<'_>) {
        let mut uses: BTreeMap<(String, String), Uses> = BTreeMap::new();
        for f in all {
            for t in &f.type_refs {
                match r.ix.type_ref(&t.written, f) {
                    // A type that mentions itself is a real shape and a
                    // useless edge.
                    (key, true) if key != t.owner => {
                        let slot = uses.entry((t.owner.clone(), key)).or_insert_with(|| Uses {
                            roles: BTreeSet::new(),
                            name: t.name.clone(),
                            injected: None,
                        });
                        slot.roles.insert(t.role.clone());
                        slot.take_injection(r.injection_at(&t.owner, &t.name));
                    }
                    (_, true) => {}
                    _ => self.acct.foreign_type_refs += 1,
                }
            }
        }
        self.acct.type_ref_edges = uses.len();
        self.acct.injected_edges = uses.values().filter(|u| u.injected.is_some()).count();
        for ((owner, target), u) in uses {
            self.edges.pending.push(u.edge(&owner, &target));
        }
    }
}

/// What one `USES_TYPE` edge says: the positions its owner names the type
/// in, the first name it does so under, and how the dependency arrives.
struct Uses {
    roles: BTreeSet<String>,
    name: String,
    injected: Option<&'static str>,
}

impl Uses {
    /// Keeps the strongest evidence seen: an attribute someone wrote
    /// outranks a constructor the container calls by convention.
    fn take_injection(&mut self, found: Option<&Injection>) {
        match found.map(|i| i.kind.as_str()) {
            Some("attribute") => self.injected = Some("attribute"),
            Some("constructor") if self.injected.is_none() => {
                self.injected = Some("constructor");
            }
            _ => {}
        }
    }

    fn edge(&self, owner: &str, target: &str) -> Edge {
        let mut e = edge_at(owner, target, "USES_TYPE", 0);
        e.props.remove("line");
        e.props.insert(
            "role".into(),
            serde_json::json!({
                "$desc": "the type position this declaration names the type in",
                "$value": self.roles.iter().cloned().collect::<Vec<_>>().join(", "),
            }),
        );
        if !self.name.is_empty() {
            e.props
                .insert("name".into(), Value::String(self.name.clone()));
        }
        if let Some(kind) = self.injected {
            e.props.insert(
                "injected".into(),
                serde_json::json!({
                    "$desc": "what says the container wires this dependency",
                    "$value": kind,
                }),
            );
            e.props.insert(
                "_injected_confidence".into(),
                Value::String(injection_confidence(kind).into()),
            );
        }
        e
    }
}

/// Every file's nodes, first seen winning, and how many keys were shared —
/// which for C# is how a `partial` type reaches one node.
fn collect_nodes(all: &[FileFacts], out: &mut Assembled) -> (usize, BTreeSet<String>) {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut merged = 0usize;
    for f in all {
        for n in &f.nodes {
            if seen.insert(n.key.clone()) {
                out.nodes.push(n.clone());
            } else {
                merged += 1;
            }
        }
    }
    (merged, seen)
}

/// One node per namespace declared, which is what contains everything else.
fn add_namespaces(ix: &Index, out: &mut Assembled, seen: &mut BTreeSet<String>) {
    for ns in &ix.namespaces {
        if !seen.insert(ns.clone()) {
            continue;
        }
        let mut props = Props::new();
        props.insert("name".into(), Value::String(ns.clone()));
        out.nodes.push(Node {
            key: ns.clone(),
            label: "Namespace".into(),
            extra_labels: Vec::new(),
            props,
        });
    }
}

/// The two halves of a route, written on the symbols that state them.
///
/// Never a node or an edge of their own: a request and the declaration that
/// serves it are matched when someone asks, and an edge drawn here would
/// freeze a guess about which handler that is.
fn write_routes(all: &[FileFacts], ix: &Index, out: &mut Assembled) {
    let serves = declared_routes(all, ix);
    let asked = route::Routes::of(route::Side::Requests, all.iter().flat_map(|f| &f.requests));
    let served = route::Routes::of(route::Side::Serves, &serves);
    let mut recorded = 0usize;
    let mut declares = 0usize;
    for node in &mut out.nodes {
        recorded += asked.write(&node.key, &mut node.props);
        declares += served.write(&node.key, &mut node.props);
    }
    out.notes.extend(served.note(declares));
    out.notes.extend(asked.note(recorded));
}

/// What the attributes declare: a type-level `[Route]` is the prefix every
/// handler under it hangs off, which is why the two are read in one pass.
fn declared_routes(all: &[FileFacts], ix: &Index) -> Vec<route::RouteRef> {
    let mut prefixes: BTreeMap<String, String> = BTreeMap::new();
    let mut declared: BTreeMap<String, Declared> = BTreeMap::new();
    for f in all {
        for c in f.clauses.iter().filter(|c| c.ty == "ANNOTATED_BY") {
            let paths = attr_paths(&c.written, c.args.as_deref().unwrap_or_default());
            if ix.types.contains_key(&c.src) {
                if let Some(first) = paths.into_iter().next() {
                    prefixes.entry(c.src.clone()).or_insert(first);
                }
                continue;
            }
            let verbs = attr_verbs(&c.written);
            if verbs.is_empty() && paths.is_empty() {
                continue;
            }
            let d = declared.entry(c.src.clone()).or_default();
            for v in verbs {
                if !d.verbs.contains(&v) {
                    d.verbs.push(v);
                }
            }
            d.paths.extend(paths);
            d.line = if d.line == 0 {
                c.line
            } else {
                d.line.min(c.line)
            };
        }
    }
    declared
        .iter()
        .flat_map(|(handler, d)| cross(handler, d, &prefixes, ix))
        .collect()
}

/// One handler's verbs crossed with its paths, the type's prefix applied
/// and ASP.NET's own route tokens filled in.
fn cross(
    handler: &str,
    d: &Declared,
    prefixes: &BTreeMap<String, String>,
    ix: &Index,
) -> Vec<route::RouteRef> {
    let (owner, member) = handler.rsplit_once('.').unwrap_or((handler, ""));
    // An interface member has no body and serves nothing: a Refit client
    // declares a route it *asks for*, which is the other half of the
    // convention and a slice of its own.
    if ix.types.get(owner).map(|e| e.kind.as_str()) == Some("Interface") {
        return Vec::new();
    }
    let prefix = prefixes
        .get(owner)
        .map(|p| tokens(p, owner, member))
        .unwrap_or_default();
    // No path of its own means the type's: `[HttpGet]` under
    // `[Route("api/orders")]` serves `/api/orders`.
    let own: Vec<String> = if d.paths.is_empty() {
        vec![String::new()]
    } else {
        d.paths.iter().map(|p| tokens(p, owner, member)).collect()
    };
    let verbs: Vec<Option<route::Verb>> = if d.verbs.is_empty() {
        vec![None]
    } else {
        d.verbs.iter().copied().map(Some).collect()
    };
    let mut out = Vec::new();
    for path in own {
        let written = if path.is_empty() {
            prefix.clone()
        } else {
            route::join(&prefix, &path)
        };
        // Neither the handler nor its type states a path: nothing is known,
        // and a guess would be worse than the silence.
        if written.is_empty() {
            continue;
        }
        for verb in &verbs {
            out.push(route::RouteRef {
                symbol: handler.to_string(),
                verb: *verb,
                written: written.clone(),
                line: d.line,
            });
        }
    }
    out
}

/// ASP.NET's route tokens, filled in the way it fills them: `[controller]`
/// is the type's name without its `Controller` suffix and `[action]` is the
/// method's. Any other token is a convention this reader was not taught,
/// and stays as written.
fn tokens(path: &str, owner: &str, member: &str) -> String {
    if !path.contains('[') {
        return path.to_string();
    }
    let controller = simple(owner)
        .strip_suffix("Controller")
        .unwrap_or(simple(owner));
    path.replace("[controller]", controller)
        .replace("[action]", member)
}

/// Nodes every edge needs and nothing declared: a bare `Type` for an
/// endpoint this tree does not hold, and a stand-in for each external
/// carrying what its references proved.
fn add_implied(out: &mut Assembled, seen: &BTreeSet<String>, ext: &Externals) {
    let mut implied: BTreeSet<String> = BTreeSet::new();
    for e in &out.edges {
        for key in [&e.src, &e.dst] {
            if !seen.contains(key.as_str()) && !ext.map.contains_key(key.as_str()) {
                implied.insert(key.clone());
            }
        }
    }
    for key in implied {
        out.nodes.push(Node {
            key,
            label: "Type".into(),
            extra_labels: Vec::new(),
            props: Props::new(),
        });
    }
    for (key, label) in &ext.map {
        out.nodes.push(Node {
            key: key.clone(),
            label: (*label).into(),
            extra_labels: vec!["External".into()],
            props: Props::new(),
        });
    }
}

/// Resolve every chunk's facts into one answer, in the order given — the
/// host guarantees chunk order, and order is what keeps the result
/// independent of where the boundaries fell.
pub fn assemble(all: Vec<FileFacts>) -> Assembled {
    let mut out = Assembled::default();
    let ix = Index::of(&all, &mut out.skipped);
    let (merged, mut seen) = collect_nodes(&all, &mut out);
    add_namespaces(&ix, &mut out, &mut seen);
    write_routes(&all, &ix, &mut out);

    let r = Resolved::of(&all, ix);
    let registrations = r.registry.values().map(Vec::len).sum();
    let mut w = World {
        edges: Edges::default(),
        ext: Externals::default(),
        acct: Account {
            merged,
            registrations,
            ..Default::default()
        },
    };
    w.file_edges(&all);
    w.namespace_tree(&r.ix);
    w.imports(&all, &r.ix);
    for f in &all {
        w.acct.unresolved += f.opaque;
        w.calls(f, &r);
        w.clauses(f, &r);
    }
    w.uses_types(&all, &r);
    out.edges = w.edges.pending;
    add_implied(&mut out, &seen, &w.ext);
    out.notes.extend(w.acct.notes());
    out
}
