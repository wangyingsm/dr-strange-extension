//! The cross-file half: type references bound against the parsed type set —
//! same package first, then imports, then wildcards, then Kotlin's own
//! default imports — calls walked onto member surfaces, companion objects
//! and the in-tree supertype chain, and every clause turned into its edge.
//! Once, over every chunk's facts, in chunk order.

use crate::{
    Call, CallKind, Clause, Edge, FileFacts, Injection, Node, Props, STEREOTYPES, TypeDecl, edge_at,
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

/// Kotlin imports these packages into every file; a miss that names one of
/// their types is that, not an unresolved reference. The spelling is
/// `kotlin.*` even where the JVM maps it elsewhere, because that is the name
/// the source could have written.
const DEFAULT_IMPORTS: &[&str] = &[
    "Any",
    "Unit",
    "Nothing",
    "String",
    "CharSequence",
    "Char",
    "Boolean",
    "Byte",
    "Short",
    "Int",
    "Long",
    "Float",
    "Double",
    "Number",
    "Array",
    "List",
    "MutableList",
    "Set",
    "MutableSet",
    "Map",
    "MutableMap",
    "Collection",
    "Iterable",
    "Sequence",
    "Pair",
    "Triple",
    "Result",
    "Lazy",
    "Comparable",
    "Enum",
    "Regex",
    "StringBuilder",
    "Throwable",
    "Exception",
    "RuntimeException",
    "Error",
    "IllegalArgumentException",
    "IllegalStateException",
    "NullPointerException",
    "IndexOutOfBoundsException",
    "UnsupportedOperationException",
    "NumberFormatException",
    "ArithmeticException",
    "ClassCastException",
    "Deprecated",
    "Function",
];

/// What a parsed type offers a resolver: its member surface, its companion
/// and its supertypes.
struct TypeEntry {
    methods: BTreeSet<String>,
    /// The one supertype written with a constructor invocation.
    extends: Vec<String>,
    /// The ones written without: interfaces.
    implements: Vec<String>,
    /// Its label, so a port (`Interface`) is told from a type that writes
    /// its own members.
    kind: String,
    /// Its companion object, which answers `Type.member()`.
    companion: Option<String>,
}

/// Every name this run parsed, and where each one lives.
struct Index {
    /// Every type this run parsed, by qualified name.
    types: BTreeMap<String, TypeEntry>,
    /// package → simple name → qualified, for same-package resolution.
    by_package: BTreeMap<String, BTreeMap<String, String>>,
    /// package → function name → key, for a bare call to a top-level
    /// function, which Kotlin resolves without a receiver.
    functions: BTreeMap<String, BTreeMap<String, String>>,
    packages: BTreeSet<String>,
    /// Every key this run declared — what tells an external reference from
    /// one the tree holds.
    declared: BTreeSet<String>,
    /// `(receiver type, name)` → the extension function's key. Flat across
    /// packages: an extension is reached through an import, and resolving on
    /// the pair is what Kotlin itself does at compile time.
    extensions: BTreeMap<(String, String), String>,
}

impl Index {
    fn of(all: &[FileFacts], skipped: &mut usize) -> Self {
        let mut ix = Index {
            types: BTreeMap::new(),
            by_package: BTreeMap::new(),
            functions: BTreeMap::new(),
            packages: BTreeSet::new(),
            declared: BTreeSet::new(),
            extensions: BTreeMap::new(),
        };
        for f in all {
            if f.failed {
                *skipped += 1;
                continue;
            }
            if !f.package.is_empty() {
                ix.packages.insert(f.package.clone());
                ix.declared.insert(f.package.clone());
            }
            for n in &f.nodes {
                ix.declared.insert(n.key.clone());
            }
            for (name, key) in &f.top_functions {
                ix.functions
                    .entry(f.package.clone())
                    .or_default()
                    .entry(name.clone())
                    .or_insert_with(|| key.clone());
            }
            for (recv, name, key) in &f.extensions {
                ix.extensions
                    .entry((recv.clone(), name.clone()))
                    .or_insert_with(|| key.clone());
            }
            for t in &f.types {
                ix.add_type(t, &f.package);
            }
        }
        ix
    }

    fn add_type(&mut self, t: &TypeDecl, package: &str) {
        self.types
            .entry(t.key.clone())
            .or_insert_with(|| TypeEntry {
                methods: t.methods.iter().cloned().collect(),
                extends: t.extends.clone(),
                implements: t.implements.clone(),
                kind: t.kind.clone(),
                companion: t.companion.clone(),
            });
        // Only a top-level type is reachable by its simple name from
        // elsewhere in its package; a nested one is reached through its
        // outer.
        if let Some(simple_name) = t.key.rsplit('.').next()
            && t.key
                .strip_suffix(simple_name)
                .and_then(|p| p.strip_suffix('.'))
                .map(|p| p == package)
                .unwrap_or(package.is_empty())
        {
            self.by_package
                .entry(package.to_string())
                .or_default()
                .entry(simple_name.to_string())
                .or_insert_with(|| t.key.clone());
        }
    }

    /// A type reference resolved the way the compiler reads it:
    /// already-qualified, else same package, else an import, else a wildcard
    /// import, else a default import, else nothing a parser can say.
    /// `Ok(None)` means a certain external; `Err(())` means unresolvable.
    #[allow(clippy::result_unit_err)]
    fn type_ref(&self, written: &str, f: &FileFacts) -> Result<(String, bool), ()> {
        if written.contains('.') {
            return self.qualified_ref(written, f);
        }
        if let Some(q) = self.by_package.get(&f.package).and_then(|m| m.get(written)) {
            return Ok((q.clone(), true));
        }
        if let Some(imp) = f
            .imports
            .iter()
            .find(|i| !i.is_member && i.local == written)
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
        if DEFAULT_IMPORTS.contains(&written) {
            return Ok((format!("kotlin.{written}"), false));
        }
        Err(())
    }

    /// A dotted reference: in-tree if we parsed it, else an `outer.Inner`
    /// through an import, else a package path that reads as external.
    #[allow(clippy::result_unit_err)]
    fn qualified_ref(&self, written: &str, f: &FileFacts) -> Result<(String, bool), ()> {
        if self.types.contains_key(written) {
            return Ok((written.to_string(), true));
        }
        let (head, rest) = written.split_once('.').unwrap_or((written, ""));
        if let Ok((outer, true)) = self.type_ref(head, f) {
            let nested = format!("{outer}.{rest}");
            if self.types.contains_key(&nested) {
                return Ok((nested, true));
            }
        }
        if written.chars().next().is_some_and(char::is_lowercase) {
            return Ok((written.to_string(), false));
        }
        Err(())
    }

    /// Does `type_key` answer `method`, on itself, on its companion object,
    /// or up its in-tree supertypes? Returns the type that declares it.
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
        // `Engine.create()` is the companion object's function, written as
        // though it were the type's own — which in Kotlin it is.
        if let Some(companion) = &entry.companion
            && let Some(found) = self.find_method(f, companion, method, depth + 1)
        {
            return Some(found);
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

    /// Every in-tree type above `type_key`. A port names its
    /// implementations through this, reversed.
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

    /// The edge a supertype clause really is. Kotlin's syntax only says
    /// whether the supertype was constructed; the target's own declaration
    /// says what it is, wherever the tree holds it.
    fn supertype_edge<'a>(&self, clause: &'a Clause, key: &str) -> &'a str {
        if clause.ty == "ANNOTATED_BY" {
            return &clause.ty;
        }
        match self.types.get(key).map(|e| e.kind.as_str()) {
            Some("Interface") => "IMPLEMENTS",
            Some(_) => "EXTENDS",
            None => &clause.ty,
        }
    }
}

/// A bean Spring would instantiate: the name it is registered under, and
/// whether it wins a tie.
struct Bean {
    name: String,
    primary: bool,
}

/// The containers Spring fills with every bean of the element type.
const MULTIVALUED: &[&str] = &[
    "List",
    "MutableList",
    "Set",
    "MutableSet",
    "Collection",
    "Iterable",
    "Map",
    "MutableMap",
    "Sequence",
];

/// The last segment of a qualified name — an annotation is matched by the
/// name Spring knows it by, however the source spelled the import.
fn simple(qualified: &str) -> &str {
    qualified.rsplit('.').next().unwrap_or(qualified)
}

/// Spring's default bean name: the simple name decapitalized, except that
/// two leading capitals stay as written (`URLFetcher`), as `Introspector`
/// does.
fn decapitalize(name: &str) -> String {
    let mut cs = name.chars();
    match (cs.next(), cs.next()) {
        (Some(a), Some(b)) if a.is_uppercase() && b.is_uppercase() => name.to_string(),
        (Some(a), _) => a.to_lowercase().chain(name.chars().skip(1)).collect(),
        _ => name.to_string(),
    }
}

/// The bean name an annotation's arguments state: a lone string literal, or
/// a `name = "…"` / `value = "…"` member. Nothing when they say something
/// else.
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

/// Every path-shaped string literal an annotation's arguments hold.
///
/// Read from the argument rather than from a table of annotation names, the
/// way a request is read from a call's: `"/x"`, `value = "/x"`, `path = "/x"`
/// and `["/a", "/b"]` are one rule, and a framework nobody listed declares
/// its routes here too. A `produces = "application/json"` is not path-shaped
/// and says nothing.
fn anno_paths(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = args;
    while let Some(start) = rest.find('"') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('"') else { break };
        let text = &after[..end];
        if route::looks_like_path(text) {
            out.push(text.to_string());
        }
        rest = &after[end + 1..];
    }
    out
}

/// The verbs an annotation states: the one its own name carries, plus any
/// named as an enum entry in its arguments.
///
/// `@RequestMapping(method = [RequestMethod.POST])` is the one shape where
/// the annotation's name does not say the verb.
fn anno_verbs(written: &str, args: Option<&str>) -> Vec<route::Verb> {
    let mut out = Vec::new();
    if let Some(v) = route::Verb::from_name(simple(written)) {
        out.push(v);
    }
    for enum_name in ["RequestMethod.", "HttpMethod."] {
        let mut rest = args.unwrap_or_default();
        while let Some(at) = rest.find(enum_name) {
            rest = &rest[at + enum_name.len()..];
            let word: String = rest.chars().take_while(char::is_ascii_alphabetic).collect();
            if let Some(v) = route::Verb::from_name(&word)
                && !out.contains(&v)
            {
                out.push(v);
            }
        }
    }
    out
}

/// What one handler's annotations declare, before the class prefix applies.
///
/// Crossed rather than taken one annotation at a time: JAX-RS writes `@GET`
/// and `@Path("/{id}")` as two annotations and one route, and reading them
/// separately would claim two.
#[derive(Default)]
struct Declared {
    verbs: Vec<route::Verb>,
    paths: BTreeSet<String>,
    line: u64,
}

/// One call site asking for a port's implementation.
struct Site<'a> {
    /// The interface the receiver is typed by, whose implementations are the
    /// candidates.
    port: &'a str,
    method: &'a str,
    /// The port member the source named — where the written edge went.
    via: &'a str,
    /// The type holding the receiver, whose injection points say how the
    /// dependency arrives.
    owner: &'a str,
    /// The function the call is in, and where in it — what a wired edge
    /// hangs off.
    caller: &'a str,
    line: u64,
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

/// One edge the container implies: the bean's member, the rule that picked
/// it, and the port member the source actually named.
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
    /// Which bean's member a call through a port reaches — Spring's own
    /// rules, in its own order, and nothing where the container itself would
    /// refuse to start.
    ///
    /// Only an injected receiver is dispatched: when a port value arrives
    /// any other way, whoever built it chose the implementation, and that is
    /// data flow a parser must not guess at. A type that asks for a
    /// container-full of the port has declared it takes every bean of it,
    /// which counts for any value of that port inside it.
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
        let candidates = self.candidates(at, f);
        if candidates.is_empty() {
            return Picked::Silent;
        }
        self.rules(at, &candidates, by_the_container)
    }

    /// A candidate is a bean that answers the member; the type that declares
    /// it is where the code is, which may be a base class.
    fn candidates(&self, at: &Site<'_>, f: &FileFacts) -> Vec<(String, bool, String)> {
        self.implementors
            .get(at.port)
            .into_iter()
            .flatten()
            .filter_map(|key| {
                let bean = self.beans.get(key)?;
                let owner = self.ix.find_method(f, key, at.method, 0)?;
                Some((
                    bean.name.clone(),
                    bean.primary,
                    format!("{owner}.{}", at.method),
                ))
            })
            .collect()
    }

    /// Spring's order: a qualifier names one, else one candidate, else one
    /// `@Primary`, else every bean of a container-full, else nothing.
    fn rules(
        &self,
        at: &Site<'_>,
        candidates: &[(String, bool, String)],
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
        if let Some(name) = at
            .injection
            .and_then(|i| i.qualifier.as_deref())
            .and_then(named_arg)
            && let Some((_, _, dst)) = candidates.iter().find(|(n, _, _)| *n == name)
        {
            return one(dst.clone(), "spring-qualifier", "high");
        }
        if let [(_, _, dst)] = candidates {
            return one(dst.clone(), "spring-bean", "high");
        }
        if let [(_, _, dst)] = candidates
            .iter()
            .filter(|(_, primary, _)| *primary)
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
            "$desc": "the port member the source named, which the container wires to this",
            "$value": w.via.clone(),
        }),
    );
    e
}

/// How much an injection's evidence is worth, ranked as `test_flag`'s is.
fn injection_confidence(kind: &str) -> &'static str {
    match kind {
        "annotation" => "definitive",
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
    /// function on an external type is still this tree's own function, so
    /// `value.shout()` on a `String` resolves where `value.length()` cannot —
    /// and that needs the written name the `hints` filter discards.
    written: BTreeMap<(String, String), String>,
    /// A function key → the in-tree type it declares it returns.
    returns: BTreeMap<String, String>,
    /// Injection points by the type and the name they arrive under — which
    /// is how a receiver reaches one: `repo.save()` in a bean asks what
    /// wired `repo`.
    injections: BTreeMap<(String, String), &'a Injection>,
    /// The ports an owner asks for by the container-full: Spring hands these
    /// every bean of the element type, so a call through one reaches them
    /// all.
    collections: BTreeSet<(String, String)>,
    beans: BTreeMap<String, Bean>,
    implementors: BTreeMap<String, BTreeSet<String>>,
}

impl<'a> Resolved<'a> {
    fn of(all: &'a [FileFacts], ix: Index) -> Self {
        let mut hints = BTreeMap::new();
        let mut written = BTreeMap::new();
        let mut returns = BTreeMap::new();
        for f in all {
            for h in &f.hints {
                written
                    .entry((h.caller.clone(), h.name.clone()))
                    .or_insert_with(|| h.written.clone());
                if let Ok((key, true)) = ix.type_ref(&h.written, f) {
                    hints
                        .entry((h.caller.clone(), h.name.clone()))
                        .or_insert(key);
                }
            }
            for (fkey, written) in &f.returns {
                if let Ok((key, true)) = ix.type_ref(written, f) {
                    returns.entry(fkey.clone()).or_insert(key);
                }
            }
        }
        let (injections, collections) = injection_indexes(all, &ix);
        let beans = bean_index(all, &ix, &returns);
        let implementors = implementor_index(all, &ix);
        Self {
            ix,
            hints,
            written,
            returns,
            injections,
            collections,
            beans,
            implementors,
        }
    }

    fn dispatch(&self) -> Dispatch<'_> {
        Dispatch {
            ix: &self.ix,
            beans: &self.beans,
            implementors: &self.implementors,
            collections: &self.collections,
        }
    }

    /// A value's stated type: a local or parameter of the caller, else a
    /// property of the enclosing type (bare property names reach here too).
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
            .get(&(owner.to_string(), name.to_string()))
            .copied()
    }
}

/// The injection points this run states, and the ports asked for by the
/// container-full.
type Injections<'a> = (
    BTreeMap<(String, String), &'a Injection>,
    BTreeSet<(String, String)>,
);

fn injection_indexes<'a>(all: &'a [FileFacts], ix: &Index) -> Injections<'a> {
    let mut points = BTreeMap::new();
    let mut collections = BTreeSet::new();
    for f in all {
        for i in &f.injections {
            points.entry((i.owner.clone(), i.name.clone())).or_insert(i);
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
    (points, collections)
}

/// A stereotype makes a type a bean; a `@Bean` function makes one of what it
/// returns. The name is what a `@Qualifier` matches against.
fn bean_index(
    all: &[FileFacts],
    ix: &Index,
    returns: &BTreeMap<String, String>,
) -> BTreeMap<String, Bean> {
    let mut beans: BTreeMap<String, Bean> = BTreeMap::new();
    for f in all {
        for c in f.clauses.iter().filter(|c| c.ty == "ANNOTATED_BY") {
            let anno = simple(&c.written);
            let named = c.args.as_deref().and_then(named_arg);
            if STEREOTYPES.contains(&anno) && ix.types.contains_key(&c.src) {
                let name = named.unwrap_or_else(|| decapitalize(simple(&c.src)));
                beans.entry(c.src.clone()).or_insert(Bean {
                    name,
                    primary: false,
                });
            } else if anno == "Bean"
                && let Some(returned) = returns.get(&c.src)
            {
                let name = named.unwrap_or_else(|| simple(&c.src).to_string());
                beans.entry(returned.clone()).or_insert(Bean {
                    name,
                    primary: false,
                });
            }
        }
    }
    // `@Primary` is read after the beans exist: it ranks one, it does not
    // declare it.
    for f in all {
        for c in f.clauses.iter().filter(|c| simple(&c.written) == "Primary") {
            for key in [Some(&c.src), returns.get(&c.src)].into_iter().flatten() {
                if let Some(b) = beans.get_mut(key) {
                    b.primary = true;
                }
            }
        }
    }
    beans
}

/// Every in-tree type under each port it answers to, transitively, so
/// `FooImpl : AbstractFoo(), Foo` is one of `Foo`'s.
fn implementor_index(all: &[FileFacts], ix: &Index) -> BTreeMap<String, BTreeSet<String>> {
    let mut implementors: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for f in all {
        for t in &f.types {
            let mut ports: BTreeSet<String> = BTreeSet::new();
            ix.ancestors(f, &t.key, 0, &mut ports);
            for port in ports {
                implementors.entry(port).or_default().insert(t.key.clone());
            }
        }
    }
    implementors
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
        // Bare `External` (an import proves only existence) yields to
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
                "{} declaration(s) shared a key across files; the first seen is kept",
                self.merged
            ),
        );
        say(
            self.type_ref_edges + self.foreign_type_refs,
            format!(
                "{} type reference(s) recorded as `USES_TYPE` — what a declaration's \
                 properties, parameters and returns are typed by; {} more name types \
                 this tree does not declare and are left as written text on the node",
                self.type_ref_edges, self.foreign_type_refs
            ),
        );
        say(
            self.injected_edges,
            format!(
                "{} of those are injection points — a dependency a container wires, \
                 marked `injected` with what the evidence was",
                self.injected_edges
            ),
        );
        say(
            self.wired,
            format!(
                "{} call(s) through an injected port also reach the bean Spring wires, \
                 stamped with the rule that picked it; the call to the port stays \
                 beside it, as the source wrote it",
                self.wired
            ),
        );
        say(
            self.ambiguous,
            format!(
                "{} call(s) stop at their port: several beans implement it and no \
                 `@Primary` or `@Qualifier` says which — what Spring itself refuses \
                 to start on",
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

    /// The package tree, where both ends were parsed.
    fn package_tree(&mut self, ix: &Index) {
        for pkg in &ix.packages {
            if let Some((parent, _)) = pkg.rsplit_once('.')
                && ix.packages.contains(parent)
            {
                self.edges.add(Edge {
                    src: parent.to_string(),
                    dst: pkg.clone(),
                    ty: "CONTAINS".into(),
                    props: Props::new(),
                });
            }
        }
    }

    /// `IMPORTS`, from each of the file's top-level declarations — Kotlin's
    /// import is file scoped, and the declaration is what a reader
    /// navigates by.
    fn imports(&mut self, all: &[FileFacts], ix: &Index) {
        for f in all {
            for (target, line) in &f.import_lines {
                let dst = target.strip_suffix(".*").unwrap_or(target);
                if !ix.declared.contains(dst) {
                    self.ext.note(&ix.declared, dst, "External");
                }
                for t in &f.top_types {
                    self.edges.add(edge_at(t, dst, "IMPORTS", *line));
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
                    self.self_property_call(f, c, (field, method), r)
                }
                CallKind::FieldChain { obj, field, method } => {
                    self.property_chain_call(f, c, (obj, field, method), r)
                }
                CallKind::StaticChain {
                    type_ref,
                    first,
                    method,
                } => self.static_chain_call(f, c, (type_ref, first, method), r),
            }
        }
    }

    /// `helper()`: the type's own surface and its in-tree supertypes, else a
    /// top-level function of this package, else an imported member.
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
        if let Some(key) = r.ix.functions.get(&f.package).and_then(|m| m.get(method)) {
            self.edges.add(edge_at(&c.caller, key, "CALLS", c.line));
            return;
        }
        if let Some(imp) = f.imports.iter().find(|i| i.is_member && i.local == method) {
            if !r.ix.declared.contains(&imp.target) {
                self.ext.note(&r.ix.declared, &imp.target, "Function");
                self.acct.external_calls += 1;
            }
            self.edges
                .add(edge_at(&c.caller, &imp.target, "CALLS", c.line));
            return;
        }
        self.acct.unresolved += 1;
    }

    /// `Helper.create()`: through a type reference, written down — the
    /// companion object included, which is where Kotlin puts what Java
    /// writes `static`.
    fn static_call(&mut self, f: &FileFacts, c: &Call, at: (&str, &str), r: &Resolved<'_>) {
        let (type_ref, method) = at;
        match r.ix.type_ref(type_ref, f) {
            Ok((key, true)) => match r.ix.find_method(f, &key, method, 0) {
                Some(owner) => self.edges.add(edge_at(
                    &c.caller,
                    &format!("{owner}.{method}"),
                    "CALLS",
                    c.line,
                )),
                None => self.acct.unresolved += 1,
            },
            Ok((key, false)) => {
                let target = format!("{key}.{method}");
                self.ext.note(&r.ix.declared, &target, "Function");
                self.ext.note(&r.ix.declared, &key, "External");
                self.acct.external_calls += 1;
                self.edges.add(edge_at(&c.caller, &target, "CALLS", c.line));
            }
            Err(()) => self.acct.unresolved += 1,
        }
    }

    /// `Helper()`: Kotlin spells construction as a call to the type, and so
    /// does the graph.
    fn new_call(&mut self, f: &FileFacts, c: &Call, written: &str, r: &Resolved<'_>) {
        match r.ix.type_ref(written, f) {
            Ok((key, in_tree)) => {
                if !in_tree {
                    self.ext.note(&r.ix.declared, &key, "Class");
                    self.acct.external_calls += 1;
                }
                self.edges.add(edge_at(&c.caller, &key, "CALLS", c.line));
            }
            Err(()) => self.acct.unresolved += 1,
        }
    }

    /// `super.m()`: the one supertype a constructor invocation names, where
    /// this tree holds it.
    fn super_call(&mut self, f: &FileFacts, c: &Call, method: &str, r: &Resolved<'_>) {
        let resolved =
            r.ix.types
                .get(&c.in_type)
                .map(|e| e.extends.clone())
                .unwrap_or_default()
                .iter()
                .find_map(|base| {
                    let (key, true) = r.ix.type_ref(base, f).ok()? else {
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

    /// `lg.log()`: a value receiver, typed where the source states it.
    fn receiver_call(&mut self, f: &FileFacts, c: &Call, at: (&str, &str), r: &Resolved<'_>) {
        let (name, method) = at;
        let declared_type = r.type_of(&c.caller, &c.in_type, name).cloned();
        let hit = declared_type.as_ref().and_then(|declared| {
            r.ix.find_method(f, declared, method, 0)
                .map(|owner| (declared.clone(), format!("{owner}.{method}")))
        });
        // No member of that name: an extension function on the same type is
        // the other thing `value.shout()` can mean, and the only other thing
        // Kotlin resolves without types. Its key is the whole target, since
        // the function lives on its declaring object, not on the receiver.
        let hit = hit.or_else(|| {
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
                port: &declared,
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

    /// `this.repo.save()`: through the property's declared type.
    fn self_property_call(&mut self, f: &FileFacts, c: &Call, at: (&str, &str), r: &Resolved<'_>) {
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
                port: &declared,
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

    /// `obj.repo.save()`: one property hop on a typed value.
    fn property_chain_call(
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
                port: &declared,
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

    /// `Foo.create().bar()`: the factory's declared return names what the
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
                .ok()
                .filter(|(_, in_tree)| *in_tree)
                .and_then(|(k, _)| r.ix.find_method(f, &k, first, 0))
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

    /// Supertypes and annotations, every one resolved the same way.
    fn clauses(&mut self, f: &FileFacts, r: &Resolved<'_>) {
        for c in &f.clauses {
            let Ok((key, in_tree)) = r.ix.type_ref(&c.written, f) else {
                self.acct.unresolved += 1;
                continue;
            };
            let ty = r.ix.supertype_edge(c, &key);
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
                        "$desc": "the annotation's arguments, as the source writes them",
                        "$value": args,
                    }),
                );
            }
            self.edges.add(e);
        }
    }

    /// A declared type is a dependency as surely as a call is: asking what a
    /// type's change would break must see who takes it as a parameter,
    /// returns it, or holds it in a property — not only who constructed one.
    ///
    /// The in-tree rule has to hold here: the implied-node pass mints a bare
    /// `Type` for any endpoint nothing declares, so an edge to `String`
    /// would put `String` in the graph.
    fn uses_types(&mut self, all: &[FileFacts], r: &Resolved<'_>) {
        let mut uses: BTreeMap<(String, String), Uses> = BTreeMap::new();
        for f in all {
            for t in &f.type_refs {
                match r.ix.type_ref(&t.written, f) {
                    // A type that mentions itself is a real shape and a
                    // useless edge.
                    Ok((key, true)) if key != t.owner => {
                        let slot = uses.entry((t.owner.clone(), key)).or_insert_with(|| Uses {
                            roles: BTreeSet::new(),
                            name: t.name.clone(),
                            injected: None,
                        });
                        slot.roles.insert(t.role.clone());
                        slot.take_injection(r.injection_at(&t.owner, &t.name));
                    }
                    Ok((_, true)) => {}
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
    /// Keeps the strongest evidence seen: an annotation someone wrote
    /// outranks a constructor Spring injects by convention.
    fn take_injection(&mut self, found: Option<&Injection>) {
        match found.map(|i| i.kind.as_str()) {
            Some("annotation") => self.injected = Some("annotation"),
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
                    "$desc": "what says a container wires this dependency",
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

/// Every file's nodes, first seen winning, and how many keys were shared.
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

/// One node per package declared, which is what contains everything else.
fn add_packages(ix: &Index, out: &mut Assembled, seen: &mut BTreeSet<String>) {
    for pkg in &ix.packages {
        if !seen.insert(pkg.clone()) {
            continue;
        }
        let mut props = Props::new();
        props.insert("name".into(), Value::String(pkg.clone()));
        out.nodes.push(Node {
            key: pkg.clone(),
            label: "Package".into(),
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

/// What the annotations declare: a class-level path is the prefix every
/// handler under it hangs off — Spring's `@RequestMapping` on the
/// controller, JAX-RS's `@Path`, Micronaut's `@Controller` — which is why
/// the two are read in one pass.
fn declared_routes(all: &[FileFacts], ix: &Index) -> Vec<route::RouteRef> {
    let mut prefixes: BTreeMap<String, String> = BTreeMap::new();
    let mut declared: BTreeMap<String, Declared> = BTreeMap::new();
    for f in all {
        for c in f.clauses.iter().filter(|c| c.ty == "ANNOTATED_BY") {
            let paths = anno_paths(c.args.as_deref().unwrap_or_default());
            if ix.types.contains_key(&c.src) {
                if let Some(first) = paths.into_iter().next() {
                    prefixes.entry(c.src.clone()).or_insert(first);
                }
                continue;
            }
            let verbs = anno_verbs(&c.written, c.args.as_deref());
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

/// One handler's verbs crossed with its paths, the class's prefix applied.
fn cross(
    handler: &str,
    d: &Declared,
    prefixes: &BTreeMap<String, String>,
    ix: &Index,
) -> Vec<route::RouteRef> {
    let owner = handler.rsplit_once('.').map(|(t, _)| t).unwrap_or_default();
    // An interface member has no body and serves nothing: a Feign or
    // `@HttpExchange` client declares a route it *asks for*, which is the
    // other half of the convention and a slice of its own.
    if ix.types.get(owner).map(|e| e.kind.as_str()) == Some("Interface") {
        return Vec::new();
    }
    let prefix = prefixes.get(owner).map(String::as_str).unwrap_or_default();
    // No path of its own means the class's: `@GetMapping` under
    // `@RequestMapping("/api")` serves `/api`.
    let own: Vec<&str> = if d.paths.is_empty() {
        vec![""]
    } else {
        d.paths.iter().map(String::as_str).collect()
    };
    let verbs: Vec<Option<route::Verb>> = if d.verbs.is_empty() {
        vec![None]
    } else {
        d.verbs.iter().copied().map(Some).collect()
    };
    let mut out = Vec::new();
    for path in own {
        let written = if path.is_empty() {
            prefix.to_string()
        } else {
            route::join(prefix, path)
        };
        // Neither the handler nor its class states a path: nothing is known,
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
    add_packages(&ix, &mut out, &mut seen);
    write_routes(&all, &ix, &mut out);

    let r = Resolved::of(&all, ix);
    let mut w = World {
        edges: Edges::default(),
        ext: Externals::default(),
        acct: Account {
            merged,
            ..Default::default()
        },
    };
    w.file_edges(&all);
    w.package_tree(&r.ix);
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
