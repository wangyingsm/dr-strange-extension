//! Rust source → graph facts (drsg ROADMAP §11), at the interface level.
//!
//! The parsing half of the drsg `rust` plugin, as a plain library so its tests
//! run natively while `../component` wraps it for wasm. It emits what a reader
//! of the *interface* would want — functions, types, traits, constants, what
//! implements what, what contains what, what imports what, and who calls whom.
//!
//! ## Two phases, matching the plugin contract
//!
//! [`parse_chunk`] turns one chunk of files into [`FileFacts`] — pure and
//! per-file, safe to run concurrently anywhere. [`assemble`] resolves across
//! every file's facts at once: calls, impl blocks, re-export facades. The
//! split *is* the wasm contract's `parse`/`assemble`, which is no accident:
//! this structure predates the sandbox and the contract was shaped to it.
//!
//! ## Keys are module paths
//!
//! An item's identity is `dr_strange_core::compute::exec::execute` — what a
//! Rust programmer calls it — rather than the file it happens to live in.
//! Files move; module paths are the name.
//!
//! ## Two limits worth stating
//!
//! **Calls** resolve by written path, then by locality. A method call written
//! `.read()` names no path: it resolves when the body *states* the receiver's
//! type — an annotation, a parameter, a field's declared type, a constructor
//! path, a declared return — and that holds for std and other crates too,
//! where the target is an external stand-in (`Vec::push`) rather than a
//! declaration. A receiver nothing states the type of is counted, never
//! guessed. **Properties** are JSON values in
//! the shape the drsg contract carries (`$desc`/`$value` for described
//! properties), because a property may hold a list or a map and the boundary
//! has no recursive types.
//!
//! Whatever is left unresolved is counted in the notes, so a thin graph is
//! explained rather than mistaken for the whole truth.

use std::collections::{BTreeMap, BTreeSet};

use quote::ToTokens;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use syn::visit::Visit;

/// What the parser reads through — the plugin contract's host, as one small
/// trait so tests can hand in a plain directory walker.
pub trait Files {
    /// Readable paths ending with `suffix` (`""` for all), sorted.
    fn list(&self, suffix: &str) -> Result<Vec<String>, String>;
    fn read(&self, path: &str) -> Result<Vec<u8>, String>;
    /// What to call the tree when its contents do not say.
    fn label(&self) -> Option<String>;
}

/// A property map: JSON object entries, exactly as the contract carries them.
pub type Props = serde_json::Map<String, Value>;

/// A fact about a thing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub key: String,
    pub label: String,
    /// Labels asserted rather than chosen — `External` on a stand-in for
    /// something outside the tree that was read.
    pub extra_labels: Vec<String>,
    pub props: Props,
}

/// A fact about a relation, between node keys.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub src: String,
    pub dst: String,
    pub ty: String,
    pub props: Props,
}

/// The assembled result: facts, and an account of what could not be done.
#[derive(Debug, Default)]
pub struct Assembled {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    /// Files that would not parse.
    pub skipped: usize,
    pub notes: Vec<String>,
}

/// Parse one chunk of paths into per-file facts.
///
/// Pure per file: nothing here looks across files, which is what makes chunks
/// safe to parse concurrently in instances that share nothing. Resolution —
/// the cross-file half — happens in [`assemble`], once, over every chunk's
/// facts together.
pub fn parse_chunk(files: &dyn Files, paths: &[String], include_source: bool) -> Vec<FileFacts> {
    let sources: Vec<(String, String)> = paths
        .iter()
        .filter_map(|p| {
            let bytes = files.read(p).ok()?;
            Some((p.clone(), String::from_utf8(bytes).ok()?))
        })
        .collect();

    // Resolve each crate once, before the per-file walk: every file under one
    // `src/` shares a prefix, and reading its manifest per file would be the
    // same answer over and over.
    let crates = crate_names(&sources, files);
    let fallback = files
        .label()
        .map(|l| l.replace('-', "_"))
        .unwrap_or_else(|| "crate".into());

    sources
        .iter()
        .map(|(path, text)| {
            let module = module_path(path, &crates, &fallback);
            parse_file(path, &module, text, include_source)
        })
        .collect()
}

/// A single document — an upload, one file — parsed as its own chunk.
pub fn parse_document(name: &str, bytes: &[u8], include_source: bool) -> Vec<FileFacts> {
    let text = String::from_utf8_lossy(bytes).into_owned();
    let crates = BTreeMap::new();
    let module = module_path(name, &crates, "crate");
    vec![parse_file(name, &module, &text, include_source)]
}

/// One file's contribution, before cross-file resolution.
///
/// This is the **partial** the plugin contract shuttles between `parse` and
/// `assemble`, which is why it serializes: chunks parse in instances that
/// share nothing, and their facts meet again as bytes.
#[derive(Default, Serialize, Deserialize)]
pub struct FileFacts {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    /// This file's own module path, which resolution is relative to.
    module: String,
    /// The source path as the host handed it — what UnresolvedRef nodes are
    /// attributed to, so an incremental fold can delete and re-create them
    /// with their file.
    path: String,
    /// `(caller key, what it called)`, resolved once every file is known.
    calls: Vec<(String, Call)>,
    /// `(function key, declared return type as written)` for every free
    /// function whose return is a type this parser can read — what types a
    /// `let x = f();` receiver.
    returns: Vec<(String, String)>,
    /// `(const or static key, its type as written)` — a name a body reads
    /// like a local, typed by its declaration.
    consts: Vec<(String, String)>,
    /// `(enum key, variant, fields as (name or position, type as written))`
    /// — what a `Variant(x)` or `Variant { x }` pattern binds.
    variant_fields: Vec<(String, String, VariantFields)>,
    /// Keys of structs declared with unnamed fields — `struct Meters(f64)`.
    /// A tuple struct is the one type whose *name* is also callable, so
    /// `Meters(1.0)` reads as a call and is a construction; knowing which
    /// names those are is what keeps it from becoming a phantom function.
    tuple_structs: Vec<String>,
    /// `(alias key, its type parameters, target type as written)` for every
    /// `type A<T> = B<T, …>;` — a name for its target, which is what a
    /// method call on it needs, with the parameters a use site fills in.
    type_aliases: Vec<(String, Vec<String>, String)>,
    /// `(type key, field name, field type as written)` for plain-path struct
    /// fields — what types `o.field.m()` and `self.field.m()`.
    field_types: Vec<(String, String, String)>,
    /// `(caller key, struct path as written, line)` for struct-literal
    /// expressions — a `Widget { .. }` is a use of the type, worth an edge.
    insts: Vec<(String, String, u64)>,
    /// `(caller key, bare name, line)` — functions passed as values.
    fn_refs: Vec<(String, String, u64)>,
    /// `(caller, callee, arg index, closure param names)` — closures whose
    /// parameters the callee's declared bound may type.
    closure_uses: Vec<(String, ClosureCallee, usize, Vec<ClosureParam>)>,
    /// `(fn key, arg index, declared closure-arg types)` from signatures.
    closure_sigs: Vec<(String, usize, Vec<String>)>,
    /// `(trait key, supertrait base path as written, line)` — `trait E: D`.
    trait_bases: Vec<(String, String, u64)>,
    /// `(type key, derived trait as written, line)` — `#[derive(Clone)]`.
    /// Resolved exactly as a supertrait is, and emitted as IMPLEMENTS: a
    /// derive and a hand-written `impl` state the same fact, and recording
    /// one but not the other made the graph disagree with itself.
    #[serde(default)]
    derives: Vec<(String, String, u64)>,
    /// `(owner key, attribute path as written, the whole attribute, line)` —
    /// `#[tokio::main]`, `#[get("/health")]`. The path is what the node is;
    /// the text rides on the edge, so `get` stays one node with a route per
    /// edge rather than a node per route.
    #[serde(default)]
    annotations: Vec<(String, String, String, u64)>,
    /// `(caller key, local name, how its type is known)` — the bindings whose
    /// type a parser *can* know, kept for method-call resolution.
    local_hints: Vec<(String, String, LocalHint)>,
    /// `impl` blocks, held whole until every file is known — see [`walk_impl`].
    impls: Vec<ImplBlock>,
    /// `(fn key, parameter name, type as written)` — the declared parameter
    /// types, for the `USES_TYPE` edges. Returns, fields, variants and alias
    /// targets are already collected for receiver typing; parameters were the
    /// one type position nothing else needed.
    #[serde(default)]
    param_types: Vec<(String, String, String)>,
    /// Module keys carrying `#[cfg(test)]` — the scopes whose every item is
    /// compiled out of the production build. Everything under one is test
    /// code, including `impl` blocks whose methods are keyed only at
    /// assemble.
    #[serde(default)]
    cfg_test_scopes: Vec<String>,
    /// `(module, macro path, arguments)` for each item-position invocation —
    /// the places where items exist that this parse cannot see.
    macro_calls: Vec<(String, String, String, u64)>,
    /// `(name introduced, path named)` for every `pub use` — the re-exports
    /// that make a facade path like `crate::cache` exist without anything
    /// being declared under it.
    reexports: Vec<(String, String, String)>,
    /// `(module, path as written)` for every `use`/`extern crate` in the file,
    /// **including those inside an inline `mod`** — whose imports are scoped to
    /// that module and would otherwise reach the node's property without ever
    /// reaching an edge.
    ///
    /// The nearest thing to name resolution a parser has: when a file says
    /// `use super::Database`, that *is* where its `impl Database` points.
    imports: Vec<(String, String, String, u64)>,
    /// Each key's enclosing **module**, which is what resolution narrows by.
    ///
    /// Recorded rather than recovered from the key, because a trait method's
    /// key is `<Type as Trait>::m` and the module it lives in cannot be read
    /// back out of that.
    scopes: BTreeMap<String, String>,
    /// A file that would not parse at all.
    unparsed: Option<String>,
}

/// One call site, as much of it as the source actually spelled out.
///
/// A path call writes where it is going — `std::fs::read(…)`, `Vec::new()`,
/// or a bare name the file imported — and that path is enough to record the
/// callee even when it lives in another crate. A method call writes only
/// `.read()`, so it resolves only as far as the receiver's type can be read
/// off the body; one whose type nothing states stays unresolved rather than
/// becoming a guess.
#[derive(Clone, Serialize, Deserialize)]
struct Call {
    /// The final segment — what resolution against local items matches on.
    name: String,
    /// The path as written, when the call site wrote one.
    path: Option<String>,
    /// A method call's receiver as a chain — see [`chain_of`]: `txn` in
    /// `txn.remove(…)`, `self.graph` in `self.graph.len()`, `v.iter()` in
    /// `v.iter().map(…)`. Typing the chain is what makes the call resolvable.
    recv: Option<String>,
    /// The call site's line. Not part of identity: the same callee named
    /// twice in one body is one fact, and the first site is the line —
    /// `BTreeSet::insert` keeps the first, and visiting order is source
    /// order.
    line: u64,
    /// How control departs from waiting for this callee, when it does:
    /// `spawned` for work handed to another task or thread, `blocking` for
    /// `spawn_blocking`. Not part of identity either, and unioned rather than
    /// overwritten when one body reaches one callee both ways — a call
    /// awaited on one line and spawned on another is both, and the spawned
    /// half is the half worth seeing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    concurrent: Option<String>,
}

impl PartialEq for Call {
    fn eq(&self, other: &Self) -> bool {
        (&self.name, &self.path, &self.recv) == (&other.name, &other.path, &other.recv)
    }
}
impl Eq for Call {}
impl PartialOrd for Call {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Call {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (&self.name, &self.path, &self.recv).cmp(&(&other.name, &other.path, &other.recv))
    }
}

/// How a local's type is known without being a compiler.
///
/// Two deterministic sources: an annotation is the type written down, and a
/// binding is a chain the same typing that reads receivers can follow —
/// `f()` to a declared return, `Vec::new()` to a constructor's type, `y.m()`
/// through `y`'s type to what `m` returns — with the steps a pattern takes
/// from there: the item of what a `for` iterates, the payload of a `Some(x)`,
/// the position of a tuple, a field. Every step reads something the source
/// wrote, or a fact about std this parser carries; a step it cannot take
/// types nothing, and the local stays untyped.
#[derive(Clone, Serialize, Deserialize)]
enum LocalHint {
    /// `let x = <chain>;`, `for x in <chain>`, `if let Some(x) = <chain>`,
    /// `let (a, b) = <chain>` — the chain as [`chain_of`] renders it, then
    /// the steps the pattern takes into its type.
    Bound { chain: String, steps: Vec<Step> },
    /// `let x: T = …;` — the annotation, as written.
    Typed(String),
    /// A type resolved at assemble — a closure parameter typed by what it
    /// was handed to.
    Known(Ty),
}

/// One step a pattern takes into a value's type.
#[derive(Clone, Serialize, Deserialize)]
enum Step {
    /// What iterating the value yields — `for x in v`.
    Item,
    /// The payload of `Some(x)`, `Ok(x)` or `Err(e)` — or of a declared
    /// enum's variant, named as the pattern wrote it (`Statement::Write`,
    /// or bare `Write` when imported); a struct's own name is a no-op step.
    /// The fields of a variant are then reached by `Index`/`Field`.
    Payload(String),
    /// Position `n` of a tuple — `(a, b)`.
    Index(usize),
    /// A named field — `Node { key, .. }`.
    Field(String),
}

/// A variant's fields as `(name or position, type as written)`.
type VariantFields = Vec<(String, String)>;

/// One closure parameter: its name, which parameter of the closure it is,
/// the tuple position it destructures (`|(k, v)|`), and its annotation when
/// it has one (`|x: &Node|`).
#[derive(Clone, Serialize, Deserialize)]
struct ClosureParam {
    name: String,
    position: usize,
    index: Option<usize>,
    written: Option<String>,
}

fn parse_file(path: &str, module: &str, text: &str, include_source: bool) -> FileFacts {
    let ast = match syn::parse_file(text) {
        Ok(a) => a,
        // A file that does not parse is reported, not fatal: a repository may
        // hold a deliberately broken fixture, and one of them should not sink
        // the whole ingest.
        Err(e) => {
            return FileFacts {
                unparsed: Some(format!("{path}: {e}")),
                ..Default::default()
            };
        }
    };

    let mut f = FileFacts::default();
    let mut imports = Vec::new();
    let mut reexports = Vec::new();
    collect_imports(&ast.items, module, &mut imports, &mut reexports);
    f.nodes.push(Node {
        key: module.to_string(),
        label: "Module".into(),
        extra_labels: Vec::new(),
        props: props([
            ("path", source_path(path)),
            ("doc_comment", docs_of(&ast.attrs)),
            ("imports", join_imports(&imports)),
        ]),
    });
    // Kept, not just rendered: an `impl Database` in this file is resolved
    // against what the file said it imported.
    f.imports = imports
        .iter()
        .map(|(name, path, line)| (module.to_string(), name.clone(), path.clone(), *line))
        .collect();
    f.reexports = reexports;
    f.module = module.to_string();
    f.path = source_path(path);
    walk_items(&ast.items, module, include_source, &mut f);
    // Every node above came from this file; the file-level module already
    // says so under `path`, everything else says it here.
    for n in &mut f.nodes {
        if !n.props.contains_key("path") {
            n.props
                .insert("file".into(), Value::String(source_path(path)));
        }
    }
    // Impl methods become nodes only at assemble, where the file is no longer
    // in hand — their props are stamped here, or `Method` nodes would be the
    // one kind without file attribution (an incremental sync couldn't tell a
    // deleted method from one it merely didn't look at).
    for b in &mut f.impls {
        for m in &mut b.methods {
            m.props
                .insert("file".into(), Value::String(source_path(path)));
        }
    }

    // Test-ness, from the two scopes cargo compiles apart from the library.
    //
    // A file under `tests/` or `benches/` is a target of its own, built only
    // by `cargo test`/`cargo bench`; a `#[cfg(test)]` module is compiled out
    // of the library entirely. Both are the toolchain's rule rather than a
    // convention, which is what makes them `definitive` — and both are
    // scopes, so they reach the `impl` blocks written inside them, whose
    // methods are keyed only at assemble.
    let target = target_root(path).and_then(|(_, root, _)| match root {
        "tests/" => Some("test code: a file under `tests/`, which cargo builds as an integration-test target and never links into the library"),
        "benches/" => Some("test code: a file under `benches/`, which cargo builds as a bench target and never links into the library"),
        _ => None,
    });
    if let Some(desc) = target {
        for n in &mut f.nodes {
            set_test_flag(&mut n.props, "build-rule", desc, "definitive");
        }
        for b in &mut f.impls {
            for m in &mut b.methods {
                set_test_flag(&mut m.props, "build-rule", desc, "definitive");
            }
        }
    }
    let scopes = f.cfg_test_scopes.clone();
    for scope in &scopes {
        let inner = format!("{scope}::");
        for n in f
            .nodes
            .iter_mut()
            .filter(|n| n.key == *scope || n.key.starts_with(&inner))
        {
            set_test_flag(&mut n.props, "build-rule", CFG_TEST_DESC, "definitive");
        }
        for b in f
            .impls
            .iter_mut()
            .filter(|b| b.module == *scope || b.module.starts_with(&inner))
        {
            for m in &mut b.methods {
                set_test_flag(&mut m.props, "build-rule", CFG_TEST_DESC, "definitive");
            }
        }
    }
    f
}

/// Every `use` and `extern crate` path, as written.
///
/// Kept as a property rather than edges: an import usually names something in
/// another crate, and minting a node for every `std::collections::BTreeMap`
/// would bury the graph in vocabulary nobody asked about. The list is still
/// searchable, which answers "what uses `rayon`" without the node explosion.
fn collect_imports(
    items: &[syn::Item],
    module: &str,
    out: &mut Vec<(String, String, u64)>,
    reexports: &mut Vec<(String, String, String)>,
) {
    for item in items {
        match item {
            syn::Item::Use(u) => {
                let mut named = Vec::new();
                flatten_use(&u.tree, &mut String::new(), &mut named);
                // A re-export does not import a name, it *republishes* one:
                // `pub use api::cache;` at the crate root is what makes
                // `crate::cache` a path at all, and nothing declares an item
                // under it. Recorded so those paths can be followed later.
                //
                // `pub(crate) use` counts. It creates the facade path for the
                // whole crate, which is precisely the scope being parsed — and
                // it is how a crate exposes an internal type to its own
                // modules without exposing it to the world. A bare `use` does
                // not: it is private to its module, and treating it as a
                // re-export would invent paths nothing can actually name.
                if !matches!(u.vis, syn::Visibility::Inherited) {
                    reexports.extend(
                        named
                            .iter()
                            .filter(|(name, _)| name != "*")
                            .map(|(name, path)| (module.to_string(), name.clone(), path.clone())),
                    );
                }
                let line = line_of(&u.use_token);
                out.extend(named.into_iter().map(|(name, path)| (name, path, line)));
            }
            syn::Item::ExternCrate(e) => {
                out.push((e.ident.to_string(), e.ident.to_string(), line_of(&e.ident)))
            }
            _ => {}
        }
    }
    // Sorted then deduplicated by the pair: `use a::b;` and `use a::b as c;`
    // are two names in scope, not one import written twice.
    out.sort();
    out.dedup_by(|a, b| (&a.0, &a.1) == (&b.0, &b.1));
    reexports.sort();
    reexports.dedup();
}

/// Flatten a `use` tree into `(the name it introduces, the path it names)`.
fn flatten_use(tree: &syn::UseTree, prefix: &mut String, out: &mut Vec<(String, String)>) {
    match tree {
        syn::UseTree::Path(p) => {
            let saved = prefix.len();
            if !prefix.is_empty() {
                prefix.push_str("::");
            }
            prefix.push_str(&p.ident.to_string());
            flatten_use(&p.tree, prefix, out);
            prefix.truncate(saved);
        }
        // `use a::b::{self, C}` brings in `a::b` *itself*, not a child called
        // `self` — a path naming no item, which would become a node nothing
        // could ever be.
        syn::UseTree::Name(n) if n.ident == "self" => {
            if let Some(name) = prefix.rsplit("::").next().filter(|_| !prefix.is_empty()) {
                out.push((name.to_string(), prefix.clone()));
            }
        }
        syn::UseTree::Name(n) => {
            let name = n.ident.to_string();
            out.push((name.clone(), joined(prefix, &name)));
        }
        // `use a::b::{self as c}` — the same module under another name.
        syn::UseTree::Rename(r) if r.ident == "self" => {
            if !prefix.is_empty() {
                out.push((r.rename.to_string(), prefix.clone()));
            }
        }
        // `use x as y` introduces `y` while naming `x`.
        syn::UseTree::Rename(r) => {
            out.push((r.rename.to_string(), joined(prefix, &r.ident.to_string())))
        }
        syn::UseTree::Glob(_) => out.push(("*".into(), joined(prefix, "*"))),
        syn::UseTree::Group(g) => {
            for t in &g.items {
                flatten_use(t, prefix, out);
            }
        }
    }
}

fn joined(prefix: &str, leaf: &str) -> String {
    if prefix.is_empty() {
        leaf.to_string()
    } else {
        format!("{prefix}::{leaf}")
    }
}

/// Walk one item list, attributing everything to the module path `parent`.
fn walk_items(items: &[syn::Item], parent: &str, include_source: bool, f: &mut FileFacts) {
    for item in items {
        match item {
            syn::Item::Fn(func) => {
                let key = format!("{parent}::{}", func.sig.ident);
                let (label, mut p) =
                    fn_facts(&func.sig, &func.attrs, Some(&func.vis), Some(&func.block));
                if include_source {
                    add_source(&mut p, item);
                }
                f.nodes.push(Node {
                    key: key.clone(),
                    label: label.into(),
                    extra_labels: Vec::new(),
                    props: p,
                });
                f.scopes.insert(key.clone(), parent.to_string());
                f.edges
                    .push(edge_at(parent, &key, "CONTAINS", line_of(&func.sig.ident)));
                collect_attrs(f, &key, &func.attrs, line_of(&func.sig.ident));
                collect_calls(&key, &func.block, f);
                if let Some(ret) = ret_written(&func.sig) {
                    f.returns.push((key.clone(), ret));
                }
                for (name, written) in sig_params(&func.sig) {
                    f.param_types.push((key.clone(), name, written));
                }
                for (idx, args) in closure_sig(&func.sig) {
                    f.closure_sigs.push((key.clone(), idx, args));
                }
                let generics = generic_bounds(&func.sig.generics, None);
                for (ident, hint) in local_hints(&func.block, &func.sig, &generics) {
                    f.local_hints.push((key.clone(), ident, hint));
                }
            }
            syn::Item::Struct(s) => {
                simple(f, parent, &s.ident, "Struct", &s.attrs, &s.vis);
                set_end(f, s);
                set_fields(f, s.fields.iter());
                set_non_exhaustive(f, &s.attrs);
                // Plain-path field types, machine-readable: what types
                // `o.field.m()` when the receiver walks a field.
                let key = format!("{parent}::{}", s.ident);
                if matches!(s.fields, syn::Fields::Unnamed(_)) {
                    f.tuple_structs.push(key.clone());
                }
                for (i, field) in s.fields.iter().enumerate() {
                    if let Some(t) = written_ty(&field.ty) {
                        let name = field
                            .ident
                            .as_ref()
                            .map_or_else(|| i.to_string(), ToString::to_string);
                        f.field_types.push((key.clone(), name, t));
                    }
                }
            }
            syn::Item::Enum(e) => {
                simple(f, parent, &e.ident, "Enum", &e.attrs, &e.vis);
                set_end(f, e);
                // The variants *are* the enum: a node saying only `Expr` says
                // almost nothing, while its variants are the whole shape of it.
                // A list rather than a joined string, so `CONTAINS` asks about
                // membership of a variant instead of a substring of a blob.
                // An empty enum has no shape to describe, and an empty list
                // property is noise on every read of the node.
                if let Some(n) = f.nodes.last_mut()
                    && !e.variants.is_empty()
                {
                    let variants: Vec<Value> = e
                        .variants
                        .iter()
                        .map(|v| Value::String(variant_of(v)))
                        .collect();
                    n.props.insert(
                        "variants".into(),
                        json!({
                            "$desc": "the enum's variants, each with its fields as written",
                            "$value": variants,
                        }),
                    );
                }
                set_non_exhaustive(f, &e.attrs);
                // Each variant's fields, typed: what `Variant(x)` binds.
                let enum_key = format!("{parent}::{}", e.ident);
                for v in &e.variants {
                    let fields: VariantFields = v
                        .fields
                        .iter()
                        .enumerate()
                        .filter_map(|(i, field)| {
                            let name = field
                                .ident
                                .as_ref()
                                .map_or_else(|| i.to_string(), ToString::to_string);
                            Some((name, written_ty(&field.ty)?))
                        })
                        .collect();
                    f.variant_fields
                        .push((enum_key.clone(), v.ident.to_string(), fields));
                }
            }
            syn::Item::Union(u) => {
                simple(f, parent, &u.ident, "Union", &u.attrs, &u.vis);
                set_end(f, u);
                // A union is a struct whose fields overlap in memory; the
                // fields are just as much its shape.
                set_fields(f, u.fields.named.iter());
                set_non_exhaustive(f, &u.attrs);
            }
            syn::Item::Const(c) => {
                let key = format!("{parent}::{}", c.ident);
                if let Some(t) = written_ty(&c.ty) {
                    f.consts.push((key.clone(), t));
                }
                node(
                    f,
                    parent,
                    key,
                    "Const",
                    &c.attrs,
                    &c.vis,
                    ty_of(&c.ty),
                    line_of(&c.ident),
                );
                set_end(f, c);
                set_value(f, &c.expr);
            }
            syn::Item::Static(s) => {
                let key = format!("{parent}::{}", s.ident);
                if let Some(t) = written_ty(&s.ty) {
                    f.consts.push((key.clone(), t));
                }
                node(
                    f,
                    parent,
                    key,
                    "Static",
                    &s.attrs,
                    &s.vis,
                    ty_of(&s.ty),
                    line_of(&s.ident),
                );
                set_end(f, s);
                set_value(f, &s.expr);
            }
            syn::Item::Type(t) => {
                let key = format!("{parent}::{}", t.ident);
                if let Some(target) = written_ty(&t.ty) {
                    let params = generic_bounds(&t.generics, None).into_keys().collect();
                    f.type_aliases.push((key.clone(), params, target));
                }
                node(
                    f,
                    parent,
                    key,
                    "TypeAlias",
                    &t.attrs,
                    &t.vis,
                    ty_of(&t.ty),
                    line_of(&t.ident),
                );
                set_end(f, t);
            }
            syn::Item::Macro(m) => match &m.ident {
                // `macro_rules! name` — a definition, so it is an item.
                Some(ident) => {
                    let key = format!("{parent}::{ident}");
                    let mut p = props([("doc_comment", docs_of(&m.attrs))]);
                    if include_source {
                        add_source(&mut p, item);
                    }
                    p.insert("line".into(), Value::from(line_of(ident)));
                    p.insert("end_line".into(), Value::from(end_line_of(m)));
                    f.nodes.push(Node {
                        key: key.clone(),
                        label: "Macro".into(),
                        extra_labels: Vec::new(),
                        props: p,
                    });
                    f.scopes.insert(key.clone(), parent.to_string());
                    f.edges
                        .push(edge_at(parent, &key, "CONTAINS", line_of(ident)));
                }
                // An invocation *at item position* — `expr_from_literal!(bool,
                // i32);` — which defines items this parse will never see.
                //
                // Nothing expands it. Expansion is the compiler's: `macro_rules`
                // has hygiene and recursion, and a proc macro is arbitrary code
                // that must actually be run. So the items it declares are simply
                // absent from the graph.
                //
                // Recorded anyway, because a blind spot that is marked can be
                // reasoned about and a silent one cannot: the edge says this
                // module invokes that macro, and carries the arguments, which
                // is usually the shape of what was generated.
                None => f.macro_calls.push((
                    parent.to_string(),
                    path_of(&m.mac.path),
                    tidy(&m.mac.tokens.to_string()),
                    line_of(&m.mac.path),
                )),
            },
            syn::Item::Trait(t) => {
                let key = format!("{parent}::{}", t.ident);
                for bound in &t.supertraits {
                    if let syn::TypeParamBound::Trait(tb) = bound
                        && tb.lifetimes.is_none()
                    {
                        f.trait_bases
                            .push((key.clone(), base_path(&tb.path), line_of(&t.ident)));
                    }
                }
                node(
                    f,
                    parent,
                    key.clone(),
                    "Trait",
                    &t.attrs,
                    &t.vis,
                    // Likewise no signature: it would be the trait's own name.
                    String::new(),
                    line_of(&t.ident),
                );
                set_end(f, t);
                for ti in &t.items {
                    if let syn::TraitItem::Fn(m) = ti {
                        let mkey = format!("{key}::{}", m.sig.ident);
                        // No visibility: a trait's items are as public as the
                        // trait. A default body is where the bindings come from
                        // when there is one.
                        let (label, props) = fn_facts(&m.sig, &m.attrs, None, m.default.as_ref());
                        f.nodes.push(Node {
                            key: mkey.clone(),
                            label: label.into(),
                            extra_labels: Vec::new(),
                            props,
                        });
                        f.scopes.insert(mkey.clone(), parent.to_string());
                        f.edges
                            .push(edge_at(&key, &mkey, "HAS_METHOD", line_of(&m.sig.ident)));
                        collect_attrs(f, &mkey, &m.attrs, line_of(&m.sig.ident));
                        if let Some(ret) = ret_written(&m.sig) {
                            f.returns.push((mkey.clone(), ret));
                        }
                        for (name, written) in sig_params(&m.sig) {
                            f.param_types.push((mkey.clone(), name, written));
                        }
                    }
                }
            }
            syn::Item::Impl(im) => walk_impl(im, parent, include_source, f),
            // An inline `mod` extends the path, which is exactly why two
            // `fn new` in sibling modules are two different nodes.
            syn::Item::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    let key = format!("{parent}::{}", m.ident);
                    if m.attrs.iter().any(cfg_says_test) {
                        f.cfg_test_scopes.push(key.clone());
                    }
                    let mut imports = Vec::new();
                    collect_imports(inner, &key, &mut imports, &mut f.reexports);
                    f.imports.extend(imports.iter().map(|(name, path, line)| {
                        (key.clone(), name.clone(), path.clone(), *line)
                    }));
                    f.nodes.push(Node {
                        key: key.clone(),
                        label: "Module".into(),
                        extra_labels: Vec::new(),
                        props: {
                            let mut p = props([
                                ("doc_comment", docs_of(&m.attrs)),
                                ("visibility", visibility(&m.vis)),
                                ("imports", join_imports(&imports)),
                            ]);
                            p.insert("line".into(), Value::from(line_of(&m.ident)));
                            p.insert("end_line".into(), Value::from(end_line_of(m)));
                            p
                        },
                    });
                    f.edges
                        .push(edge_at(parent, &key, "CONTAINS", line_of(&m.ident)));
                    walk_items(inner, &key, include_source, f);
                }
            }
            _ => {}
        }
    }
}

/// Collect an `impl` block, resolving neither end of it.
///
/// Nothing in an `impl` is reliably declared in the module holding it. The
/// trait usually is not — `Display` lives in std, a project trait a module
/// away — and **neither is the type**: `impl Database` sits in
/// `api/snapshot.rs` while `Database` is declared in `api/mod.rs`. Assuming
/// `{parent}::{name}` for either builds a key nothing owns, which is not just
/// an imprecise edge but a *dangling* one, and a bulk write refuses those.
///
/// So the block is held whole until [`assemble`], where every file is known.
/// Its method keys wait with it, since each is built from the resolved type.
fn walk_impl(im: &syn::ItemImpl, parent: &str, include_source: bool, f: &mut FileFacts) {
    let Some(self_name) = type_name(&im.self_ty) else {
        return;
    };

    let methods = im
        .items
        .iter()
        .filter_map(|ii| match ii {
            syn::ImplItem::Fn(m) => {
                let (label, mut props) = fn_facts(&m.sig, &m.attrs, Some(&m.vis), Some(&m.block));
                if include_source {
                    add_source(&mut props, ii);
                }
                Some({
                    let body = call_names(&m.block);
                    let generics = generic_bounds(&m.sig.generics, Some(&im.generics));
                    ImplMethod {
                        name: m.sig.ident.to_string(),
                        label: label.to_string(),
                        props,
                        calls: body.calls,
                        insts: body.insts,
                        fn_refs: body.fn_refs,
                        closures: body.closures,
                        closure_sigs: closure_sig(&m.sig),
                        ret: ret_written(&m.sig),
                        param_types: sig_params(&m.sig),
                        annotations: annotations_of(&m.attrs),
                        locals: local_hints(&m.block, &m.sig, &generics),
                        line: line_of(&m.sig.ident),
                    }
                })
            }
            _ => None,
        })
        .collect();

    f.impls.push(ImplBlock {
        module: parent.to_string(),
        line: line_of(&im.impl_token),
        self_name,
        trait_base: im.trait_.as_ref().map(|(_, p, _)| base_path(p)),
        trait_full: im.trait_.as_ref().map(|(_, p, _)| path_of(p)),
        methods,
    });
}

/// An `impl` block, held whole until its type and trait can be resolved.
#[derive(Serialize, Deserialize)]
struct ImplBlock {
    /// The module it was written in — where resolution starts looking.
    module: String,
    /// The `impl` keyword's line — where the IMPLEMENTS relation is written.
    line: u64,
    self_name: String,
    /// The trait path without generic arguments, which identifies the node.
    trait_base: Option<String>,
    /// The same path with its arguments, which tells two impls apart.
    trait_full: Option<String>,
    methods: Vec<ImplMethod>,
}

#[derive(Serialize, Deserialize)]
struct ImplMethod {
    name: String,
    /// The definition's line, for the HAS_METHOD edge.
    line: u64,
    /// `Method` when it takes a `self` receiver, `Function` otherwise — an
    /// associated function like `Database::open` is not a method.
    label: String,
    props: Props,
    calls: BTreeSet<Call>,
    /// `(struct path as written, line)` for the body's struct literals.
    insts: BTreeSet<(String, u64)>,
    /// `(bare name, line)` — functions passed as values in the body.
    fn_refs: BTreeSet<(String, u64)>,
    /// Closures handed to calls in the body.
    closures: Vec<(ClosureCallee, usize, Vec<ClosureParam>)>,
    /// Declared closure-arg types per parameter position of this method.
    closure_sigs: Vec<(usize, Vec<String>)>,
    /// Declared return type as written, when this parser can read it;
    /// `Self` is resolved to the impl's type at assemble, where the type's
    /// key is known.
    ret: Option<String>,
    /// `(parameter name, type as written)`, `Self` resolved at assemble with
    /// the return above.
    #[serde(default)]
    param_types: Vec<(String, String)>,
    /// `(attribute path, the whole attribute as written)` — keyed to this
    /// method at assemble, where its key exists.
    #[serde(default)]
    annotations: Vec<(String, String)>,
    /// The body's type-known locals, for its method calls.
    locals: Vec<(String, LocalHint)>,
}

/// Record every call and struct literal this body holds, for later
/// resolution.
fn collect_calls(caller: &str, block: &syn::Block, f: &mut FileFacts) {
    let body = call_names(block);
    for c in body.calls {
        f.calls.push((caller.to_string(), c));
    }
    for (path, line) in body.insts {
        f.insts.push((caller.to_string(), path, line));
    }
    for (name, line) in body.fn_refs {
        f.fn_refs.push((caller.to_string(), name, line));
    }
    for (callee, idx, params) in body.closures {
        f.closure_uses
            .push((caller.to_string(), callee, idx, params));
    }
}

/// A closure argument's parameters: each name, which parameter it is, the
/// tuple position it takes (`|(k, v)|`), and its annotation when written.
/// A pattern deeper than a tuple of names is a checker's business.
fn closure_params(e: &syn::Expr) -> Option<Vec<ClosureParam>> {
    let syn::Expr::Closure(c) = e else {
        return None;
    };
    let mut out = Vec::new();
    for (position, pat) in c.inputs.iter().enumerate() {
        let (pat, written) = match pat {
            syn::Pat::Type(t) => (&*t.pat, written_ty(&t.ty)),
            other => (other, None),
        };
        let name_of = |p: &syn::Pat| match p {
            syn::Pat::Ident(i) => Some(i.ident.to_string()),
            syn::Pat::Reference(r) => match &*r.pat {
                syn::Pat::Ident(i) => Some(i.ident.to_string()),
                _ => None,
            },
            _ => None,
        };
        match pat {
            syn::Pat::Tuple(t) => {
                for (index, elem) in t.elems.iter().enumerate() {
                    if let Some(name) = name_of(elem) {
                        out.push(ClosureParam {
                            name,
                            position,
                            index: Some(index),
                            written: None,
                        });
                    }
                }
            }
            other => {
                if let Some(name) = name_of(other) {
                    out.push(ClosureParam {
                        name,
                        position,
                        index: None,
                        written: written.clone(),
                    });
                }
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The closure-argument types a signature declares, per parameter position:
/// `impl Fn(&Plane)`, a generic with an `Fn(&Plane)` bound (inline or in a
/// where clause), or a plain `fn(&Plane)` pointer. What only inference
/// would know stays out.
fn closure_sig(sig: &syn::Signature) -> Vec<(usize, Vec<String>)> {
    let fn_bound_args = |bounds: &syn::punctuated::Punctuated<
        syn::TypeParamBound,
        syn::token::Plus,
    >|
     -> Option<Vec<String>> {
        for b in bounds {
            if let syn::TypeParamBound::Trait(t) = b
                && let Some(last) = t.path.segments.last()
                && matches!(last.ident.to_string().as_str(), "Fn" | "FnMut" | "FnOnce")
                && let syn::PathArguments::Parenthesized(p) = &last.arguments
            {
                return p.inputs.iter().map(written_ty).collect();
            }
        }
        None
    };
    // Generic params with Fn bounds, by name — inline bounds and where
    // clauses both.
    let mut generic_fns: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for gp in &sig.generics.params {
        if let syn::GenericParam::Type(t) = gp
            && let Some(args) = fn_bound_args(&t.bounds)
        {
            generic_fns.insert(t.ident.to_string(), args);
        }
    }
    if let Some(wc) = &sig.generics.where_clause {
        for pred in &wc.predicates {
            if let syn::WherePredicate::Type(pt) = pred
                && let Some(name) = plain_type_path(&pt.bounded_ty)
                && let Some(args) = fn_bound_args(&pt.bounds)
            {
                generic_fns.entry(name).or_insert(args);
            }
        }
    }
    let mut out = Vec::new();
    for (i, input) in sig.inputs.iter().enumerate() {
        let syn::FnArg::Typed(t) = input else {
            continue;
        };
        // Positions are argument positions: the receiver never counts.
        let index = if matches!(sig.inputs.first(), Some(syn::FnArg::Receiver(_))) {
            i - 1
        } else {
            i
        };
        let args = match &*t.ty {
            syn::Type::ImplTrait(it) => fn_bound_args(&it.bounds),
            syn::Type::BareFn(bf) => bf
                .inputs
                .iter()
                .map(|a| written_ty(&a.ty))
                .collect::<Option<Vec<_>>>(),
            other => plain_type_path(other).and_then(|n| generic_fns.get(&n).cloned()),
        };
        if let Some(args) = args
            && !args.is_empty()
        {
            out.push((index, args));
        }
    }
    out
}

/// A bare (single-segment) name passed as a call argument — `handler` in
/// `register(handler)` or `&handler` — the one shape where a function is
/// verifiably being handed around as a value. Anything qualified, called
/// or computed stays a value's business.
fn bare_fn_arg(e: &syn::Expr) -> Option<String> {
    match e {
        syn::Expr::Path(p) if p.qself.is_none() && p.path.segments.len() == 1 => {
            let seg = &p.path.segments[0];
            seg.arguments.is_none().then(|| seg.ident.to_string())
        }
        syn::Expr::Reference(r) => bare_fn_arg(&r.expr),
        _ => None,
    }
}

/// An expression as a chain of hops typing can follow, or nothing when it
/// starts from something typing cannot read.
///
/// One string, `.`-separated, because it crosses the phase boundary as a
/// partial and is compared for call identity: a head, then hops.
///
/// - head: a local or `self`; `path::f()` for a call written as a path;
///   `#T` for a value whose type the language fixes — `#str` for a string
///   literal, `#String` for `format!`, `#Vec` for `vec!`, `#usize` for
///   `x as usize`.
/// - hop: `field` (or a tuple position, `.0`), `m()` (arguments dropped —
///   they never change the type), `[]` for indexing, or `?`. `&x`, `(x)`
///   and `*x` are looked through, as method resolution looks through them.
///
/// `x.await.m()`, a closure, a block: no chain. Their types are a checker's
/// to know, and a chain that stops short types nothing.
fn chain_of(e: &syn::Expr) -> Option<String> {
    match e {
        syn::Expr::Path(p) if p.qself.is_none() && p.path.segments.len() == 1 => {
            Some(p.path.segments[0].ident.to_string())
        }
        syn::Expr::Field(f) => {
            let base = chain_of(&f.base)?;
            match &f.member {
                syn::Member::Named(id) => Some(format!("{base}.{id}")),
                syn::Member::Unnamed(i) => Some(format!("{base}.{}", i.index)),
            }
        }
        syn::Expr::MethodCall(m) => {
            let recv = chain_of(&m.receiver)?;
            // A turbofish says what `collect`/`parse` produce: kept as
            // `collect<Vec<_>>()`.
            let generics: Vec<String> = m
                .turbofish
                .iter()
                .flat_map(|t| t.args.iter())
                .filter_map(|g| match g {
                    syn::GenericArgument::Type(ty) => {
                        Some(written_ty(ty).unwrap_or_else(|| "_".into()))
                    }
                    _ => None,
                })
                .collect();
            Some(if generics.is_empty() {
                format!("{recv}.{}()", m.method)
            } else {
                format!("{recv}.{}<{}>()", m.method, generics.join(", "))
            })
        }
        // `v[i]` is an item of `v`; `x as T` is a `T`.
        syn::Expr::Index(i) => Some(format!("{}.[]", chain_of(&i.expr)?)),
        syn::Expr::Cast(c) => written_ty(&c.ty).map(|t| format!("#{t}")),
        // `0..n` iterates; a comparison is a `bool`; arithmetic and negation
        // keep the left operand's type, which is what same-type arithmetic
        // gives. `x as T` is handled above.
        syn::Expr::Range(_) => Some("#Range<_>".into()),
        syn::Expr::Binary(b) => match b.op {
            syn::BinOp::Eq(_)
            | syn::BinOp::Ne(_)
            | syn::BinOp::Lt(_)
            | syn::BinOp::Le(_)
            | syn::BinOp::Gt(_)
            | syn::BinOp::Ge(_)
            | syn::BinOp::And(_)
            | syn::BinOp::Or(_) => Some("#bool".into()),
            _ => chain_of(&b.left),
        },
        syn::Expr::Unary(u) if matches!(u.op, syn::UnOp::Neg(_)) => chain_of(&u.expr),
        syn::Expr::Unary(u) if matches!(u.op, syn::UnOp::Not(_)) => chain_of(&u.expr),
        // A block whose whole value is its tail — a closure body, usually.
        syn::Expr::Block(b) if b.block.stmts.len() == 1 => match &b.block.stmts[0] {
            syn::Stmt::Expr(e, None) => chain_of(e),
            _ => None,
        },
        // A struct literal is its type; an array is an array of something.
        syn::Expr::Struct(st) if st.qself.is_none() => Some(format!("#{}", path_of(&st.path))),
        syn::Expr::Array(_) => Some("#array<_>".into()),
        // `Vec::<u8>::new()`: the path's own arguments dropped, as a `use`
        // would write it — the constructor's type is the base.
        syn::Expr::Call(c) => match &*c.func {
            syn::Expr::Path(p) if p.qself.is_none() => Some(format!("{}()", base_path(&p.path))),
            _ => None,
        },
        syn::Expr::Try(t) => Some(format!("{}.?", chain_of(&t.expr)?)),
        syn::Expr::Reference(r) => chain_of(&r.expr),
        syn::Expr::Paren(p) => chain_of(&p.expr),
        syn::Expr::Unary(u) if matches!(u.op, syn::UnOp::Deref(_)) => chain_of(&u.expr),
        // A literal's type is the language's: a string is a `str`, `5.0f64`
        // says `f64`, and an unsuffixed number is what Rust defaults it to.
        syn::Expr::Lit(l) => Some(match &l.lit {
            syn::Lit::Str(_) => "#str".into(),
            syn::Lit::ByteStr(_) => "#slice<u8>".into(),
            syn::Lit::Char(_) => "#char".into(),
            syn::Lit::Bool(_) => "#bool".into(),
            syn::Lit::Int(i) if !i.suffix().is_empty() => format!("#{}", i.suffix()),
            syn::Lit::Int(_) => "#i32".into(),
            syn::Lit::Float(f) if !f.suffix().is_empty() => format!("#{}", f.suffix()),
            syn::Lit::Float(_) => "#f64".into(),
            _ => return None,
        }),
        syn::Expr::Macro(m) => match m.mac.path.get_ident().map(ToString::to_string).as_deref() {
            Some("format") => Some("#String".into()),
            Some("vec") => Some("#Vec".into()),
            Some("env" | "concat" | "include_str" | "stringify" | "file" | "module_path") => {
                Some("#str".into())
            }
            _ => None,
        },
        _ => None,
    }
}

/// What one body does: the calls it makes and the struct literals it builds,
/// each deduplicated and in a stable order.
/// What a closure was handed to — resolved at assemble, where the callee's
/// declared `Fn(...)` bound can type the closure's parameters.
#[derive(Clone, Serialize, Deserialize)]
enum ClosureCallee {
    /// `on_all(|p| …)` / `path::to(|p| …)` — the path as written.
    Path(String),
    /// `x.on_all(|p| …)` — a method call; recv as [`chain_of`] gives it,
    /// and `body` is the closure's body when it is a chain — what a
    /// `map(|x| x.key.clone())` yields, read off the body.
    Method {
        recv: Option<String>,
        name: String,
        body: Option<String>,
    },
}

#[derive(Default)]
struct BodyFacts {
    calls: BTreeSet<Call>,
    /// `(struct path as written, first line)`.
    insts: BTreeSet<(String, u64)>,
    /// `(bare name, first line)` — a function passed as a value:
    /// `register(handler)`, `map(handler)`, `&handler`.
    fn_refs: BTreeSet<(String, u64)>,
    /// `(callee, argument index, closure param names)` — closures whose
    /// parameters the callee's signature may type.
    closures: Vec<(ClosureCallee, usize, Vec<ClosureParam>)>,
}

/// The calls that run their argument somewhere else.
///
/// Matched on the last path segment, the rule the py parser's `schedules`
/// already uses, so `tokio::spawn`, `task::spawn`, a bare imported `spawn`
/// and `handle.spawn(...)` are one rule rather than four spellings to keep up
/// with. What runs elsewhere is the closure or async block they are handed —
/// the spawner itself is called synchronously, and marking the edge to
/// something named `spawn` would only repeat its own name.
fn spawner_shape(name: &str) -> Option<&'static str> {
    match name {
        "spawn" | "spawn_local" | "spawn_pinned" => Some("spawned"),
        "spawn_blocking" => Some("blocking"),
        _ => None,
    }
}

/// The union of two departures — neither swallows the other, and a site that
/// simply waits contributes nothing.
fn union_concurrency(a: Option<String>, b: Option<String>) -> Option<String> {
    let mut kinds: BTreeSet<String> = BTreeSet::new();
    for k in [a, b].into_iter().flatten() {
        kinds.extend(k.split(", ").map(str::to_string));
    }
    (!kinds.is_empty()).then(|| kinds.into_iter().collect::<Vec<_>>().join(", "))
}

fn call_names(block: &syn::Block) -> BodyFacts {
    struct Calls<'a> {
        facts: &'a mut BodyFacts,
        /// The spawners this visit is inside, innermost last. A
        /// `rayon::scope(|s| s.spawn(...))` nests, and the inner one is what
        /// the call actually runs under.
        spawns: Vec<&'static str>,
    }
    impl Calls<'_> {
        /// Record a call, carrying whatever it runs under, and fold it into
        /// any earlier call to the same callee: first line wins, departures
        /// union.
        fn note(&mut self, mut call: Call) {
            call.concurrent = self.spawns.last().map(|s| (*s).to_string());
            if let Some(prev) = self.facts.calls.take(&call) {
                call.line = prev.line;
                call.concurrent = union_concurrency(prev.concurrent, call.concurrent);
            }
            self.facts.calls.insert(call);
        }
    }
    impl<'ast> Visit<'ast> for Calls<'_> {
        fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
            let mut spawner = None;
            if let syn::Expr::Path(p) = &*node.func
                && let Some(last) = p.path.segments.last()
            {
                spawner = spawner_shape(&last.ident.to_string());
                self.note(Call {
                    name: last.ident.to_string(),
                    path: Some(path_of(&p.path)),
                    recv: None,
                    line: line_of(node),
                    concurrent: None,
                });
            }
            for (i, arg) in node.args.iter().enumerate() {
                if let Some(name) = bare_fn_arg(arg) {
                    self.facts.fn_refs.insert((name, line_of(node)));
                }
                if let (Some(params), syn::Expr::Path(p)) = (closure_params(arg), &*node.func)
                    && p.qself.is_none()
                {
                    self.facts
                        .closures
                        .push((ClosureCallee::Path(path_of(&p.path)), i, params));
                }
            }
            // Everything inside a spawner's arguments runs where the spawner
            // put it, however deep the closure or async block goes.
            if let Some(shape) = spawner {
                self.spawns.push(shape);
            }
            syn::visit::visit_expr_call(self, node);
            if spawner.is_some() {
                self.spawns.pop();
            }
        }
        fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
            let spawner = spawner_shape(&node.method.to_string());
            self.note(Call {
                name: node.method.to_string(),
                path: None,
                recv: chain_of(&node.receiver),
                line: line_of(node),
                concurrent: None,
            });
            for (i, arg) in node.args.iter().enumerate() {
                if let Some(name) = bare_fn_arg(arg) {
                    self.facts.fn_refs.insert((name, line_of(node)));
                }
                if let Some(params) = closure_params(arg) {
                    self.facts.closures.push((
                        ClosureCallee::Method {
                            recv: chain_of(&node.receiver),
                            name: node.method.to_string(),
                            body: match arg {
                                syn::Expr::Closure(c) => chain_of(&c.body),
                                _ => None,
                            },
                        },
                        i,
                        params,
                    ));
                }
            }
            if let Some(shape) = spawner {
                self.spawns.push(shape);
            }
            syn::visit::visit_expr_method_call(self, node);
            if spawner.is_some() {
                self.spawns.pop();
            }
        }
        fn visit_expr_struct(&mut self, node: &'ast syn::ExprStruct) {
            if node.qself.is_none() {
                self.facts
                    .insts
                    .insert((path_of(&node.path), line_of(node)));
            }
            syn::visit::visit_expr_struct(self, node);
        }
    }
    let mut out = BodyFacts::default();
    Calls {
        facts: &mut out,
        spawns: Vec::new(),
    }
    .visit_block(block);
    out
}

/// The locals whose type this body states, one way or another.
///
/// A `let` with an annotation states it outright. Every other binding is a
/// chain and a pattern: `let x = …`, `for x in …`, `if let Some(x) = …`,
/// `while let`, `let … else`, a `match` arm, `let (a, b) = …`, `let Node {
/// key, .. } = …` — and the pattern says which steps into the chain's type
/// the name takes. First binding wins on a name bound twice: the calls it
/// might type were themselves deduplicated to their first site. `generics`
/// are the names that are type parameters here — an annotation `x: T` states
/// no type at all, and typing it as one would resolve `x.m()` to a `T::m`
/// that does not exist.
fn local_hints(
    block: &syn::Block,
    sig: &syn::Signature,
    generics: &BTreeMap<String, Option<String>>,
) -> Vec<(String, LocalHint)> {
    struct Hints<'a> {
        out: &'a mut Vec<(String, LocalHint)>,
        generics: &'a BTreeMap<String, Option<String>>,
    }
    impl Hints<'_> {
        fn bind(&mut self, ident: String, hint: LocalHint) {
            bind_hint(self.out, ident, hint);
        }

        /// Every name a pattern binds, each with the steps it takes into
        /// the value of `chain` after `steps`.
        fn bind_pattern(&mut self, pat: &syn::Pat, chain: &str, steps: &[Step]) {
            match pat {
                syn::Pat::Ident(i) => self.bind(
                    i.ident.to_string(),
                    LocalHint::Bound {
                        chain: chain.to_string(),
                        steps: steps.to_vec(),
                    },
                ),
                syn::Pat::Reference(r) => self.bind_pattern(&r.pat, chain, steps),
                syn::Pat::Paren(p) => self.bind_pattern(&p.pat, chain, steps),
                // `x: T` inside a pattern states the type; the chain is moot.
                syn::Pat::Type(t) => match (&*t.pat, written_ty(&t.ty)) {
                    (syn::Pat::Ident(i), Some(ty)) => match typed_hint(ty, self.generics) {
                        Some(hint) => self.bind(i.ident.to_string(), hint),
                        None => self.bind_pattern(&t.pat, chain, steps),
                    },
                    (inner, _) => self.bind_pattern(inner, chain, steps),
                },
                syn::Pat::Tuple(t) => {
                    for (index, elem) in t.elems.iter().enumerate() {
                        let mut deeper = steps.to_vec();
                        deeper.push(Step::Index(index));
                        self.bind_pattern(elem, chain, &deeper);
                    }
                }
                syn::Pat::TupleStruct(ts) => {
                    let path = path_of(&ts.path);
                    let mut deeper = steps.to_vec();
                    deeper.push(Step::Payload(path.clone()));
                    if matches!(path.as_str(), "Some" | "Ok" | "Err") && ts.elems.len() == 1 {
                        self.bind_pattern(&ts.elems[0], chain, &deeper);
                    } else {
                        for (index, elem) in ts.elems.iter().enumerate() {
                            let mut at = deeper.clone();
                            at.push(Step::Index(index));
                            self.bind_pattern(elem, chain, &at);
                        }
                    }
                }
                syn::Pat::Struct(st) => {
                    let mut deeper = steps.to_vec();
                    deeper.push(Step::Payload(path_of(&st.path)));
                    for field in &st.fields {
                        if let syn::Member::Named(name) = &field.member {
                            let mut at = deeper.clone();
                            at.push(Step::Field(name.to_string()));
                            self.bind_pattern(&field.pat, chain, &at);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    impl<'ast> Visit<'ast> for Hints<'_> {
        fn visit_local(&mut self, node: &'ast syn::Local) {
            match &node.pat {
                syn::Pat::Type(t) => {
                    if let (syn::Pat::Ident(i), Some(ty)) = (&*t.pat, written_ty(&t.ty))
                        && let Some(hint) = typed_hint(ty, self.generics)
                    {
                        self.bind(i.ident.to_string(), hint);
                    }
                }
                // `let (a, b) = (x, y);` — each side by side.
                syn::Pat::Tuple(pats) if matches!(node.init.as_ref().map(|i| &*i.expr), Some(syn::Expr::Tuple(t)) if t.elems.len() == pats.elems.len()) => {
                    if let Some(init) = &node.init
                        && let syn::Expr::Tuple(exprs) = &*init.expr
                    {
                        for (pat, expr) in pats.elems.iter().zip(&exprs.elems) {
                            if let Some(chain) = chain_of(expr) {
                                self.bind_pattern(pat, &chain, &[]);
                            }
                        }
                    }
                }
                pat => {
                    if let Some(init) = &node.init
                        && let Some(chain) = chain_of(&init.expr)
                    {
                        self.bind_pattern(pat, &chain, &[]);
                    }
                }
            }
            syn::visit::visit_local(self, node);
        }
        fn visit_expr_for_loop(&mut self, node: &'ast syn::ExprForLoop) {
            if let Some(chain) = chain_of(&node.expr) {
                self.bind_pattern(&node.pat, &chain, &[Step::Item]);
            }
            syn::visit::visit_expr_for_loop(self, node);
        }
        // `if let`, `while let`, and each `let` of a let-chain.
        fn visit_expr_let(&mut self, node: &'ast syn::ExprLet) {
            if let Some(chain) = chain_of(&node.expr) {
                self.bind_pattern(&node.pat, &chain, &[]);
            }
            syn::visit::visit_expr_let(self, node);
        }
        fn visit_expr_match(&mut self, node: &'ast syn::ExprMatch) {
            if let Some(chain) = chain_of(&node.expr) {
                for arm in &node.arms {
                    self.bind_pattern(&arm.pat, &chain, &[]);
                }
            }
            syn::visit::visit_expr_match(self, node);
        }
    }
    let mut out = Vec::new();
    Hints {
        out: &mut out,
        generics,
    }
    .visit_block(block);
    // The parameters last, through the same rule: a body's `let x = x?;`
    // has already claimed `x`, and the annotation is then what `x^` was.
    for (ident, hint) in param_hints(sig, generics) {
        bind_hint(&mut out, ident, hint);
    }
    out
}

/// Record one binding. First binding wins on a name bound twice — except
/// `let e = e?;`, which re-binds `e` from itself: its chain is made to read
/// from `e^`, the binding before this one, so `e` is what the re-binding
/// made it and `e^` what it was — the parameter's annotation, or an earlier
/// `let`, whichever arrives.
fn bind_hint(out: &mut Vec<(String, LocalHint)>, ident: String, mut hint: LocalHint) {
    let previous = format!("{ident}^");
    if let LocalHint::Bound { chain, .. } = &mut hint
        && chain.split('.').next() == Some(ident.as_str())
    {
        *chain = format!("{previous}{}", &chain[ident.len()..]);
        if let Some(slot) = out.iter_mut().find(|(n, _)| n == &ident) {
            let earlier = std::mem::replace(&mut slot.1, hint);
            bind_hint(out, previous, earlier);
        } else {
            out.push((ident, hint));
        }
        return;
    }
    match out.iter().find(|(n, _)| n == &ident) {
        None => out.push((ident, hint)),
        // A name already bound from itself: this later hint is the one it
        // was re-bound *from*.
        Some((_, LocalHint::Bound { chain, .. }))
            if chain.split('.').next() == Some(previous.as_str()) =>
        {
            if !out.iter().any(|(n, _)| n == &previous) {
                out.push((previous, hint));
            }
        }
        Some(_) => {}
    }
}

/// The type parameters in scope for a signature — its own, and its impl
/// block's when it has one — each with the trait it is bounded by, when it
/// is. Written as a type, `T` states nothing about what the value is; but
/// `T: Tr` says every method called on it is `Tr`'s, which is exactly what
/// resolution needs. Inline bounds and `where` clauses both count.
fn generic_bounds(
    sig: &syn::Generics,
    outer: Option<&syn::Generics>,
) -> BTreeMap<String, Option<String>> {
    let mut out: BTreeMap<String, Option<String>> = BTreeMap::new();
    for g in std::iter::once(sig).chain(outer) {
        for gp in &g.params {
            if let syn::GenericParam::Type(t) = gp {
                out.entry(t.ident.to_string())
                    .or_insert_with(|| bound_written(&t.bounds));
            }
        }
        if let Some(wc) = &g.where_clause {
            for pred in &wc.predicates {
                if let syn::WherePredicate::Type(pt) = pred
                    && let syn::Type::Path(tp) = &pt.bounded_ty
                    && tp.qself.is_none()
                    && tp.path.segments.len() == 1
                    && let Some(slot) = out.get_mut(&tp.path.segments[0].ident.to_string())
                    && slot.is_none()
                {
                    *slot = bound_written(&pt.bounds);
                }
            }
        }
    }
    out
}

/// The trait a bound names, as [`written_ty`] would write it: `Tr`,
/// `AsRef<std::path::Path>`, and `Iterator<Node>` for any `Iterator<Item =
/// Node>`-shaped bound, since what an iterator yields is all typing reads of
/// it. Markers (`Send`, `Sized`, `Clone`, …) say nothing about methods and
/// are passed over; a `Fn(…)` bound is a closure, which is nobody's type.
fn bound_written(
    bounds: &syn::punctuated::Punctuated<syn::TypeParamBound, syn::token::Plus>,
) -> Option<String> {
    const MARKERS: &[&str] = &[
        "Send",
        "Sync",
        "Sized",
        "Unpin",
        "Copy",
        "Clone",
        "Debug",
        "Default",
        "PartialEq",
        "Eq",
        "PartialOrd",
        "Ord",
        "Hash",
        "Any",
        "Serialize",
        "Deserialize",
        "DeserializeOwned",
    ];
    const ITERATORS: &[&str] = &[
        "Iterator",
        "IntoIterator",
        "DoubleEndedIterator",
        "ExactSizeIterator",
        "FusedIterator",
    ];
    for b in bounds {
        let syn::TypeParamBound::Trait(t) = b else {
            continue;
        };
        let last = t.path.segments.last()?;
        let name = last.ident.to_string();
        if matches!(name.as_str(), "Fn" | "FnMut" | "FnOnce") {
            return None;
        }
        if MARKERS.contains(&name.as_str()) {
            continue;
        }
        if ITERATORS.contains(&name.as_str()) {
            let item = match &last.arguments {
                syn::PathArguments::AngleBracketed(a) => a.args.iter().find_map(|g| match g {
                    syn::GenericArgument::AssocType(at) if at.ident == "Item" => written_ty(&at.ty),
                    _ => None,
                }),
                _ => None,
            };
            return Some(match item {
                Some(item) => format!("Iterator<{item}>"),
                None => "Iterator".into(),
            });
        }
        let base = base_path(&t.path);
        let args: Vec<String> = match &last.arguments {
            syn::PathArguments::AngleBracketed(a) => a
                .args
                .iter()
                .filter_map(|g| match g {
                    syn::GenericArgument::Type(ty) => {
                        Some(written_ty(ty).unwrap_or_else(|| "_".into()))
                    }
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        return Some(if args.is_empty() {
            base
        } else {
            format!("{base}<{}>", args.join(", "))
        });
    }
    None
}

/// An annotation as a hint: the type it writes — or, when that is a generic
/// parameter, the trait the parameter is bounded by; nothing when the
/// parameter is unbounded.
fn typed_hint(written: String, generics: &BTreeMap<String, Option<String>>) -> Option<LocalHint> {
    match generics.get(base_of(&written)) {
        None => Some(LocalHint::Typed(written)),
        Some(Some(bound)) => Some(LocalHint::Typed(bound.clone())),
        Some(None) => None,
    }
}

/// A signature's parameters as type-known locals: the annotation is written
/// in the signature, which is as declared as typing gets. Body bindings are
/// consumed first, so a shadowing `let` beats its parameter. A parameter
/// typed by a generic (`x: T`) states nothing — see [`local_hints`].
fn param_hints(
    sig: &syn::Signature,
    generics: &BTreeMap<String, Option<String>>,
) -> Vec<(String, LocalHint)> {
    sig.inputs
        .iter()
        .filter_map(|arg| match arg {
            syn::FnArg::Typed(t) => match (&*t.pat, written_ty(&t.ty)) {
                (syn::Pat::Ident(i), Some(ty)) => {
                    typed_hint(ty, generics).map(|hint| (i.ident.to_string(), hint))
                }
                _ => None,
            },
            syn::FnArg::Receiver(_) => None,
        })
        .collect()
}

/// The type a written type's methods belong to — `Txn`, `db::Txn`,
/// `&mut Txn<'a>`, and `Vec` for `Vec<Txn>` — or nothing.
///
/// Arguments are dropped: `Option<Txn>` is an `Option`, and its methods are
/// std's, not `Txn`'s, so the base path is what a method call on it needs.
/// References are looked through, as method resolution does; so are `Box`,
/// `Arc`, `Rc` and `Cow`, whose methods are the pointee's by deref — for
/// those the pointee is the answer, and a pointee that is not a path (`Box<dyn
/// Tr>`) is no answer at all. A slice or array is the primitive std documents
/// it as. `Fn(…)` sugar, tuples and `impl`/`dyn` types are not paths, and
/// yield nothing.
fn plain_type_path(ty: &syn::Type) -> Option<String> {
    const DEREF_TO_ARG: &[&str] = &["Box", "Arc", "Rc", "Cow"];
    match ty {
        syn::Type::Reference(r) => plain_type_path(&r.elem),
        syn::Type::Paren(p) => plain_type_path(&p.elem),
        // The primitives std documents under those names: `slice::len`.
        syn::Type::Slice(_) => Some("slice".into()),
        syn::Type::Array(_) => Some("array".into()),
        syn::Type::Path(p) if p.qself.is_none() => {
            let last = p.path.segments.last()?;
            if let syn::PathArguments::Parenthesized(_) = last.arguments {
                return None;
            }
            if DEREF_TO_ARG.contains(&last.ident.to_string().as_str())
                && let syn::PathArguments::AngleBracketed(a) = &last.arguments
            {
                let inner = a.args.iter().find_map(|g| match g {
                    syn::GenericArgument::Type(t) => Some(t),
                    _ => None,
                })?;
                return plain_type_path(inner);
            }
            Some(
                p.path
                    .segments
                    .iter()
                    .map(|s| s.ident.to_string())
                    .collect::<Vec<_>>()
                    .join("::"),
            )
        }
        _ => None,
    }
}

/// A written type with the arguments that matter kept: `Vec<Node>`,
/// `HashMap<String, PropDesc>`, `Option<Txn>`, `(usize, Node)`, `slice<u8>`
/// for `&[u8]` — the text [`split_written`] reads back. Lifetimes are
/// dropped, references and the deref pointers (`Box`, `Arc`, `Rc`, `Cow`)
/// looked through as [`plain_type_path`] looks through them, and an argument
/// this parser cannot read (`dyn Tr`, `impl Fn(…)`, a `_`) is written `_`.
/// A top-level type that is not a path yields nothing.
fn written_ty(ty: &syn::Type) -> Option<String> {
    const DEREF_TO_ARG: &[&str] = &["Box", "Arc", "Rc", "Cow"];
    fn args_of(seg: &syn::PathSegment) -> Vec<String> {
        match &seg.arguments {
            syn::PathArguments::AngleBracketed(a) => a
                .args
                .iter()
                .filter_map(|g| match g {
                    syn::GenericArgument::Type(t) => {
                        Some(written_ty(t).unwrap_or_else(|| "_".into()))
                    }
                    syn::GenericArgument::Lifetime(_) => None,
                    _ => Some("_".into()),
                })
                .collect(),
            _ => Vec::new(),
        }
    }
    match ty {
        syn::Type::Reference(r) => written_ty(&r.elem),
        syn::Type::Paren(p) => written_ty(&p.elem),
        // `impl Tr` / `dyn Tr`: the value is whatever it is, but every
        // method called on it is `Tr`'s.
        syn::Type::ImplTrait(it) => bound_written(&it.bounds),
        syn::Type::TraitObject(to) => bound_written(&to.bounds),
        syn::Type::Slice(s) => Some(format!(
            "slice<{}>",
            written_ty(&s.elem).unwrap_or_else(|| "_".into())
        )),
        syn::Type::Array(a) => Some(format!(
            "array<{}>",
            written_ty(&a.elem).unwrap_or_else(|| "_".into())
        )),
        syn::Type::Tuple(t) => Some(format!(
            "({})",
            t.elems
                .iter()
                .map(|e| written_ty(e).unwrap_or_else(|| "_".into()))
                .collect::<Vec<_>>()
                .join(", ")
        )),
        syn::Type::Path(p) if p.qself.is_none() => {
            let last = p.path.segments.last()?;
            if let syn::PathArguments::Parenthesized(_) = last.arguments {
                return None;
            }
            let args = args_of(last);
            if DEREF_TO_ARG.contains(&last.ident.to_string().as_str()) {
                let inner = args.into_iter().next()?;
                return (inner != "_").then_some(inner);
            }
            let base = p
                .path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect::<Vec<_>>()
                .join("::");
            Some(if args.is_empty() {
                base
            } else {
                format!("{base}<{}>", args.join(", "))
            })
        }
        _ => None,
    }
}

/// A written type back into its base and its arguments: `Vec<Node>` →
/// (`Vec`, [`Node`]); `(usize, Node)` → (`(`, [`usize`, `Node`]); a bare
/// path → itself and nothing. Arguments nest, so only the top level splits.
/// Every name a written type mentions, outermost first:
/// `HashMap<String, Vec<Job>>` yields `HashMap`, `String`, `Vec`, `Job`.
///
/// A type reference is not only its head — `mpsc::Sender<Job>` is how most
/// code depends on a `Job`, and stopping at `Sender` would miss exactly the
/// dependency worth recording. The synthetic heads [`written_ty`] writes for
/// shapes that are not paths (`(`, `slice`, `array`, `_`) name no type and
/// are stepped over rather than resolved.
fn named_types(written: &str, out: &mut Vec<String>) {
    const NOT_A_NAME: &[&str] = &["(", "_", "slice", "array", ""];
    let (base, args) = split_written(written);
    if !NOT_A_NAME.contains(&base) {
        out.push(base.to_string());
    }
    for arg in args {
        named_types(arg, out);
    }
}

fn split_written(written: &str) -> (&str, Vec<&str>) {
    let written = written.trim();
    let (open, close) = if written.starts_with('(') {
        (0, written.len().saturating_sub(1))
    } else {
        match written.find('<') {
            Some(i) if written.ends_with('>') => (i, written.len() - 1),
            _ => return (written, Vec::new()),
        }
    };
    let base = if written.starts_with('(') {
        "("
    } else {
        &written[..open]
    };
    let inner = &written[open + 1..close];
    let mut args = Vec::new();
    let (mut depth, mut start) = (0usize, 0usize);
    for (i, c) in inner.char_indices() {
        match c {
            '<' | '(' => depth += 1,
            '>' | ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                args.push(inner[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    let last = inner[start..].trim();
    if !last.is_empty() {
        args.push(last);
    }
    (base, args)
}

/// The base of a written type — `Vec` of `Vec<Node>`.
fn base_of(written: &str) -> &str {
    split_written(written).0
}

/// A written type with every base path rewritten — through a file's imports,
/// or `Self` to the impl's type — arguments included, since `Vec<Self>` says
/// `Self` too.
fn expand_written(written: &str, expand: &dyn Fn(&str) -> String) -> String {
    let (base, args) = split_written(written);
    let args: Vec<String> = args.iter().map(|a| expand_written(a, expand)).collect();
    if base == "(" {
        return format!("({})", args.join(", "));
    }
    let base = if base == "_" {
        base.to_string()
    } else {
        expand(base)
    };
    if args.is_empty() {
        base
    } else {
        format!("{base}<{}>", args.join(", "))
    }
}

/// Names bound by `let` in this body, deduplicated and sorted.
///
/// A flat list rather than nodes: statement-level *structure* is what §11
/// rules out, and a mid-sized repository would add hundreds of thousands of
/// nodes. The names themselves are cheap and make "which function binds
/// `write_gate`" answerable.
fn bindings_of(block: &syn::Block) -> String {
    struct Locals<'a>(&'a mut BTreeSet<String>);
    impl<'ast> Visit<'ast> for Locals<'_> {
        fn visit_local(&mut self, node: &'ast syn::Local) {
            collect_pat(&node.pat, self.0);
            syn::visit::visit_local(self, node);
        }
    }
    fn collect_pat(pat: &syn::Pat, out: &mut BTreeSet<String>) {
        match pat {
            syn::Pat::Ident(i) => {
                out.insert(i.ident.to_string());
                if let Some((_, sub)) = &i.subpat {
                    collect_pat(sub, out);
                }
            }
            syn::Pat::Tuple(t) => t.elems.iter().for_each(|p| collect_pat(p, out)),
            syn::Pat::TupleStruct(t) => t.elems.iter().for_each(|p| collect_pat(p, out)),
            syn::Pat::Struct(s) => s.fields.iter().for_each(|f| collect_pat(&f.pat, out)),
            syn::Pat::Slice(s) => s.elems.iter().for_each(|p| collect_pat(p, out)),
            syn::Pat::Reference(r) => collect_pat(&r.pat, out),
            syn::Pat::Type(t) => collect_pat(&t.pat, out),
            syn::Pat::Or(o) => o.cases.iter().for_each(|p| collect_pat(p, out)),
            _ => {}
        }
    }
    let mut names = BTreeSet::new();
    Locals(&mut names).visit_block(block);
    names.into_iter().collect::<Vec<_>>().join(", ")
}

/// Every re-export in the tree, as `facade path -> what it republishes`.
///
/// Built before anything else is resolved, since a facade declared in `lib.rs`
/// is used by files parsed long before it.
///
/// In two passes, because a re-export's *target* is written relative to the
/// module that wrote it and may itself be another facade: `lib.rs` says
/// `pub use compute::{Expr}` while `compute/mod.rs` says `pub use expr::Expr`,
/// so the first target only makes sense once the second alias is known. One
/// pass would settle `compute::Expr` against declared items alone, find
/// nothing, and leave a relative path naming no node at all.
fn build_aliases(files: &[FileFacts], declared: &BTreeSet<String>) -> BTreeMap<String, String> {
    let raw: Vec<(String, &String, String)> = files
        .iter()
        .flat_map(|f| {
            let imports = import_index(&f.imports);
            f.reexports.iter().map(move |(module, name, written)| {
                (
                    format!("{module}::{name}"),
                    module,
                    expand_path(written, &imports, module),
                )
            })
        })
        .collect();

    let alias_keys: BTreeSet<&str> = raw.iter().map(|(key, _, _)| key.as_str()).collect();
    let known = |p: &str| declared.contains(p) || alias_keys.contains(p);
    raw.iter()
        .map(|(key, module, expanded)| {
            let nested = format!("{module}::{expanded}");
            let target = match (known(expanded), known(&nested)) {
                (true, _) => expanded.clone(),
                (false, true) => nested,
                _ => expanded.clone(),
            };
            (key.clone(), target)
        })
        .collect()
}

/// Resolve calls and impl blocks across every file, then fold the facts.
///
/// In phases, because each one needs the last to be complete: an `impl` block
/// cannot be resolved until every type is known, its methods are not functions
/// that calls can resolve against until the block is, and an edge may not be
/// emitted until both its endpoints exist — a bulk write refuses a dangling one.
pub fn assemble(parsed: Vec<FileFacts>) -> Assembled {
    let mut unparsed = Vec::new();
    let mut files = Vec::new();
    for f in parsed {
        match f.unparsed {
            Some(why) => unparsed.push(why),
            None => files.push(f),
        }
    }

    // ---- phase 1: index what the files declare ----------------------------
    //
    // simple name → the keys defining it. Ambiguity is left unresolved rather
    // than guessed at.
    let mut fns: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut traits: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut types: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut scopes: BTreeMap<String, String> = BTreeMap::new();
    // Every key declared anywhere, so a path written out in full can be matched
    // exactly rather than by its last segment.
    let mut declared: BTreeSet<String> = BTreeSet::new();
    // The struct names that are also callable — see [`FileFacts::tuple_structs`].
    let mut tuple_structs: BTreeSet<String> = BTreeSet::new();
    for f in &files {
        scopes.extend(f.scopes.iter().map(|(k, v)| (k.clone(), v.clone())));
        tuple_structs.extend(f.tuple_structs.iter().cloned());
        for n in &f.nodes {
            declared.insert(n.key.clone());
            let Some(simple) = n.key.rsplit("::").next() else {
                continue;
            };
            match n.label.as_str() {
                // Both kinds are callable and both are reachable by name, so
                // the split into `Function`/`Method` must not reach resolution.
                "Function" | "Method" => fns.entry(simple.into()).or_default().push(n.key.clone()),
                "Trait" => traits.entry(simple.into()).or_default().push(n.key.clone()),
                "Struct" | "Enum" | "Union" | "TypeAlias" => {
                    types.entry(simple.into()).or_default().push(n.key.clone())
                }
                _ => {}
            }
        }
    }

    // ---- phase 2: resolve impl blocks -------------------------------------
    let mut external: BTreeMap<String, Node> = BTreeMap::new();
    let mut impl_nodes: Vec<Node> = Vec::new();
    let mut impl_edges: Vec<Edge> = Vec::new();
    let mut impl_calls: Vec<(String, String, Call)> = Vec::new();
    // module key -> what its `use` lines resolved to, replacing the as-written
    // list the parse put on the node.
    let mut import_targets: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut macro_invocations = 0usize;
    // Receiver typing (P3): every function's declared return shape, and every
    // body's type-known locals — paths expanded through the writing file's
    // imports here, while that file is still in hand.
    let mut returns_map: BTreeMap<String, String> = BTreeMap::new();
    // Type aliases: key → (parameters, target as written and expanded, the
    // module it was written in), so a method call on the alias is a call on
    // the target with the use site's arguments filled in.
    let mut alias_map: BTreeMap<String, (Vec<String>, String, String)> = BTreeMap::new();
    // Constants and statics: name → keys, and key → (type as written, module),
    // so a body's `PATTERNS.iter()` is typed by the declaration it names.
    let mut consts_by_name: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut const_types: BTreeMap<String, (String, String)> = BTreeMap::new();
    // Enum variants: (enum key, variant) → (fields as (name, type as
    // written), module), what a `Variant(x)` pattern reaches into.
    let mut variant_map: BTreeMap<(String, String), (VariantFields, String)> = BTreeMap::new();
    let mut local_inits: BTreeMap<(String, String), LocalHint> = BTreeMap::new();
    // Field types per (type key, field name), expanded through the writing
    // file's imports — what a dotted receiver walks. The module rides along
    // for scope-narrowed resolution of the written type.
    let mut fields_map: BTreeMap<(String, String), (String, String)> = BTreeMap::new();
    // `(fn key, parameter name, type as written)` — the one type position no
    // other pass needed, collected for `USES_TYPE`.
    let mut param_types: Vec<(String, String, String)> = Vec::new();
    // Trait conformance, for method resolution: which traits a type
    // implements, and where its trait-impl methods actually live.
    let mut impls_of: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut trait_impl_methods: BTreeMap<(String, String), String> = BTreeMap::new();
    // Struct literals, caller-attributed, paths expanded like calls.
    let mut pending_insts: Vec<(String, String, u64)> = Vec::new();
    // Functions passed as values, caller-attributed.
    let mut pending_fn_refs: Vec<(String, String, u64)> = Vec::new();
    // Closure typing: what each callable declares for its closure args
    // (types resolved in the DECLARING file's context), and where closures
    // were handed over.
    let mut closure_sig_map: BTreeMap<(String, usize), Vec<String>> = BTreeMap::new();
    let mut pending_closures: Vec<(String, ClosureCallee, usize, Vec<ClosureParam>)> = Vec::new();

    // `#[cfg]` alternatives can state one derive twice; one edge is the fact.
    let mut derive_seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut derives_recorded = 0usize;
    let mut annotations_recorded = 0usize;

    let aliases = build_aliases(&files, &declared);

    for f in &files {
        let imports = import_index(&f.imports);

        let through_imports = |p: &str| expand_path(p, &imports, &f.module);
        for (key, ret) in &f.returns {
            returns_map.insert(key.clone(), expand_written(ret, &through_imports));
        }
        for (key, name, written) in &f.param_types {
            param_types.push((
                key.clone(),
                name.clone(),
                expand_written(written, &through_imports),
            ));
        }
        for (key, written) in &f.consts {
            if let Some(name) = key.rsplit("::").next() {
                consts_by_name
                    .entry(name.to_string())
                    .or_default()
                    .push(key.clone());
            }
            const_types.insert(
                key.clone(),
                (expand_written(written, &through_imports), f.module.clone()),
            );
        }
        for (enum_key, variant, fields) in &f.variant_fields {
            variant_map.insert(
                (enum_key.clone(), variant.clone()),
                (
                    fields
                        .iter()
                        .map(|(n, w)| (n.clone(), expand_written(w, &through_imports)))
                        .collect(),
                    f.module.clone(),
                ),
            );
        }
        for (key, params, target) in &f.type_aliases {
            alias_map.insert(
                key.clone(),
                (
                    params.clone(),
                    expand_written(target, &through_imports),
                    f.module.clone(),
                ),
            );
        }
        for (caller, ident, hint) in &f.local_hints {
            let expanded = expand_hint(hint, &through_imports);
            local_inits
                .entry((caller.clone(), ident.clone()))
                .or_insert(expanded);
        }
        for (type_key, field, written) in &f.field_types {
            fields_map
                .entry((type_key.clone(), field.clone()))
                .or_insert((expand_path(written, &imports, &f.module), f.module.clone()));
        }
        for (caller, path, line) in &f.insts {
            pending_insts.push((
                caller.clone(),
                expand_path(path, &imports, &f.module),
                *line,
            ));
        }
        for (caller, name, line) in &f.fn_refs {
            pending_fn_refs.push((caller.clone(), name.clone(), *line));
        }
        for (fn_key, idx, args) in &f.closure_sigs {
            closure_sig_map.insert(
                (fn_key.clone(), *idx),
                args.iter()
                    .map(|a| expand_written(a, &through_imports))
                    .collect(),
            );
        }
        for (caller, callee, idx, params) in &f.closure_uses {
            let callee = match callee {
                ClosureCallee::Path(p) => ClosureCallee::Path(expand_path(p, &imports, &f.module)),
                ClosureCallee::Method { recv, name, body } => ClosureCallee::Method {
                    recv: recv.as_deref().map(|r| expand_chain(r, &through_imports)),
                    name: name.clone(),
                    body: body.as_deref().map(|b| expand_chain(b, &through_imports)),
                },
            };
            pending_closures.push((
                caller.clone(),
                callee,
                *idx,
                expand_params(params, &through_imports),
            ));
        }
        for (trait_key, written, line) in &f.trait_bases {
            let target = resolve(
                written,
                &f.module,
                &imports,
                &traits,
                &declared,
                &scopes,
                "Trait",
                &mut external,
            );
            impl_edges.push(edge_at(trait_key, &target, "EXTENDS", *line));
        }
        // A derive is an impl the compiler writes. Recorded as the same edge
        // a hand-written one makes — with `derived` on it, because which of
        // the two spellings was used is a fact about the source, not a
        // different relation — and resolved the same way, so an external
        // trait lands on the stand-in a written `impl Clone for T` already
        // lands on.
        for (type_key, written, line) in &f.derives {
            let target = resolve(
                written,
                &f.module,
                &imports,
                &traits,
                &declared,
                &scopes,
                "Trait",
                &mut external,
            );
            if !derive_seen.insert((type_key.clone(), target.clone())) {
                continue; // two `#[cfg]` arms can state one derive twice
            }
            let mut e = edge_at(type_key, &target, "IMPLEMENTS", *line);
            e.props.insert(
                "derived".into(),
                json!({
                    "$desc": "the compiler wrote this impl from a `#[derive(...)]`, rather than the source spelling it out",
                    "$value": Value::Bool(true),
                }),
            );
            impl_edges.push(e);
            derives_recorded += 1;
        }
        // What an attribute says about the item it sits on. On a service the
        // attributes are the architecture — the same argument the java plugin
        // makes for Spring — and the whole attribute rides on the edge so a
        // reader gets the route, not just the word `get`.
        for (owner, path, text, line) in &f.annotations {
            let target = resolve(
                path,
                &f.module,
                &imports,
                &traits,
                &declared,
                &scopes,
                "Macro",
                &mut external,
            );
            let mut e = edge_at(owner, &target, "ANNOTATED_BY", *line);
            if text != path {
                e.props.insert(
                    "arguments".into(),
                    json!({
                        "$desc": "the attribute as written, arguments included",
                        "$value": Value::String(text.clone()),
                    }),
                );
            }
            impl_edges.push(e);
            annotations_recorded += 1;
        }

        // Every `use` becomes an edge to what it names. An import that lands
        // on something this tree declares points at the real node; one that
        // does not — `std::collections::BTreeMap`, a dependency's type — gets
        // a node of its own, marked external and holding nothing but its path.
        // We do not read that code, so the path *is* the fact.
        //
        // A glob names no single target, so it stays in the `imports` property
        // and produces no edge rather than an invented one.
        // Grouped by the module that wrote the `use`, not by the file: an
        // inline `mod tests` has imports of its own, and they belong to it.
        for (module, _, written, line) in &f.imports {
            if written.ends_with("*") {
                // A glob names no single target, so it produces no edge. It
                // stays in the list as written, because the file did write it.
                import_targets
                    .entry(module.clone())
                    .or_default()
                    .push(written.clone());
                continue;
            }
            let target = resolve_path(
                &expand_path(written, &imports, module),
                module,
                &aliases,
                &declared,
            );
            // The list records the path **resolved**, not as written. A reader
            // of `crate::cache::GraphReader` cannot tell which crate's it is,
            // and — the reason this matters — a key that is not the node's key
            // cannot be followed to it. `as_written` on the edge keeps the
            // original for anyone who wants it.
            import_targets
                .entry(module.clone())
                .or_default()
                .push(target.clone());
            if !declared.contains(&target) {
                // `External` alone: a `use` says a name was brought into scope,
                // not whether it is a type, a trait, a function or a module. A
                // call site proves more, and says so.
                note_external(&mut external, &target, None);
            }
            let mut e = edge_at(module, &target, "IMPORTS", *line);
            if written != &target {
                e.props
                    .insert("as_written".into(), Value::String(written.clone()));
            }
            impl_edges.push(e);
        }

        // Item-position macro invocations: an edge to the macro, so the places
        // where unseen items are defined are findable rather than invisible.
        for (module, path, args, line) in &f.macro_calls {
            let target = resolve_path(
                &expand_path(path, &imports, module),
                module,
                &aliases,
                &declared,
            );
            if !declared.contains(&target) {
                note_external(&mut external, &target, Some("Macro"));
            }
            let mut e = edge_at(module, &target, "INVOKES", *line);
            if !args.is_empty() {
                e.props.insert(
                    "arguments".into(),
                    json!({
                        "$desc": "what the macro was given — usually the shape of what it generated",
                        "$value": Value::String(args.clone()),
                    }),
                );
            }
            impl_edges.push(e);
            macro_invocations += 1;
        }

        for b in &f.impls {
            let self_key = resolve(
                &b.self_name,
                &b.module,
                &imports,
                &types,
                &declared,
                &scopes,
                "Type",
                &mut external,
            );

            if let (Some(base), Some(full)) = (&b.trait_base, &b.trait_full) {
                let trait_key = resolve(
                    base,
                    &b.module,
                    &imports,
                    &traits,
                    &declared,
                    &scopes,
                    "Trait",
                    &mut external,
                );
                impls_of
                    .entry(self_key.clone())
                    .or_default()
                    .push(trait_key.clone());
                let mut e = edge_at(&self_key, &trait_key, "IMPLEMENTS", b.line);
                // `From<i64>` on the edge rather than as a second `From` node:
                // which implementation is a fact about this impl, not a new
                // word in the vocabulary.
                if full != base {
                    e.props.insert("impl".into(), Value::String(full.clone()));
                }
                impl_edges.push(e);
            }

            for m in &b.methods {
                // `<Type as Trait>::method` for a trait impl — real qualified
                // path syntax, and the only thing keeping six `impl From<…> for
                // PropValue` blocks from all claiming the key `PropValue::from`.
                // An inherent impl needs no qualifying: there is only one.
                let mkey = match &b.trait_full {
                    Some(t) => format!("<{self_key} as {t}>::{}", m.name),
                    None => format!("{self_key}::{}", m.name),
                };
                impl_nodes.push(Node {
                    key: mkey.clone(),
                    label: m.label.clone(),
                    extra_labels: Vec::new(),
                    props: m.props.clone(),
                });
                impl_edges.push(edge_at(&self_key, &mkey, "HAS_METHOD", m.line));
                scopes.insert(mkey.clone(), b.module.clone());
                fns.entry(m.name.clone()).or_default().push(mkey.clone());
                // Into `declared` too: a method is as much a path target as a
                // free function, and both path- and receiver-resolution end
                // on a declared-key check.
                declared.insert(mkey.clone());
                if b.trait_full.is_some() {
                    // Where the type's implementation of a trait method
                    // lives — the `<T as Tr>::m` key nothing can spell from
                    // a call site.
                    trait_impl_methods
                        .entry((self_key.clone(), m.name.clone()))
                        .or_insert(mkey.clone());
                }
                for (path, line) in &m.insts {
                    let expanded = match path.strip_prefix("Self") {
                        Some("") => self_key.clone(),
                        Some(rest) => format!("{self_key}{rest}"),
                        None => expand_path(path, &imports, &b.module),
                    };
                    pending_insts.push((mkey.clone(), expanded, *line));
                }
                for (name, line) in &m.fn_refs {
                    pending_fn_refs.push((mkey.clone(), name.clone(), *line));
                }
                // `Self` means this block's type, and only this block knows
                // which — rewritten here for paths, returns and locals alike.
                let deself = |p: &str| match p.strip_prefix("Self::") {
                    Some(rest) => format!("{self_key}::{rest}"),
                    None => expand_path(p, &imports, &b.module),
                };
                // `Self` as a whole type is this impl's type; `Self::x` is a
                // path into it — see `deself`.
                let self_or = |t: &str| {
                    if t == "Self" {
                        self_key.clone()
                    } else {
                        deself(t)
                    }
                };
                if let Some(ret) = &m.ret {
                    returns_map.insert(mkey.clone(), expand_written(ret, &self_or));
                }
                for (name, written) in &m.param_types {
                    param_types.push((
                        mkey.clone(),
                        name.clone(),
                        expand_written(written, &self_or),
                    ));
                }
                for (path, text) in &m.annotations {
                    let target = resolve(
                        path,
                        &b.module,
                        &imports,
                        &traits,
                        &declared,
                        &scopes,
                        "Macro",
                        &mut external,
                    );
                    let mut e = edge_at(&mkey, &target, "ANNOTATED_BY", m.line);
                    if text != path {
                        e.props.insert(
                            "arguments".into(),
                            json!({
                                "$desc": "the attribute as written, arguments included",
                                "$value": Value::String(text.clone()),
                            }),
                        );
                    }
                    impl_edges.push(e);
                    annotations_recorded += 1;
                }
                for (ident, hint) in &m.locals {
                    local_inits
                        .entry((mkey.clone(), ident.clone()))
                        .or_insert_with(|| expand_hint(hint, &self_or));
                }
                for (idx, args) in &m.closure_sigs {
                    closure_sig_map.insert(
                        (mkey.clone(), *idx),
                        args.iter().map(|a| expand_written(a, &self_or)).collect(),
                    );
                }
                for (callee, idx, params) in &m.closures {
                    let callee = match callee {
                        ClosureCallee::Path(p) => ClosureCallee::Path(deself(p)),
                        ClosureCallee::Method { recv, name, body } => ClosureCallee::Method {
                            recv: recv.as_deref().map(|r| expand_chain(r, &self_or)),
                            name: name.clone(),
                            body: body.as_deref().map(|b| expand_chain(b, &self_or)),
                        },
                    };
                    pending_closures.push((
                        mkey.clone(),
                        callee,
                        *idx,
                        expand_params(params, &self_or),
                    ));
                }
                impl_calls.extend(m.calls.iter().map(|c| {
                    let path = c.path.as_deref().map(&deself);
                    (
                        mkey.clone(),
                        f.path.clone(),
                        Call {
                            path,
                            name: c.name.clone(),
                            recv: c.recv.as_deref().map(|r| expand_chain(r, &deself)),
                            line: c.line,
                            concurrent: c.concurrent.clone(),
                        },
                    )
                }));
            }
        }
    }

    // ---- phase 3: fold the nodes ------------------------------------------
    let mut out = Assembled::default();
    let (mut unresolved, mut ambiguous, mut cfg_dupes) = (0usize, 0usize, 0usize);
    let mut external_calls = 0usize;
    let mut seen: BTreeSet<String> = BTreeSet::new();

    // A key seen twice is nearly always two `#[cfg]` alternatives of one item —
    // `const DISK` for each backend, say. That is ordinary Rust, not the
    // plugin-versus-plugin collision the router reports, so it is settled here
    // and merely counted.
    for mut n in files
        .iter_mut()
        .flat_map(|f| std::mem::take(&mut f.nodes))
        .chain(impl_nodes)
    {
        // Swap the as-written import list for the resolved one, now that every
        // file is known and each `use` has a target. Done here so the property
        // and the `IMPORTS` edges cannot drift: both come from the same pass.
        if let Some(targets) = import_targets.get(&n.key)
            && n.label == "Module"
        {
            n.props
                .insert("imports".into(), Value::String(targets.join(", ")));
        }
        if seen.insert(n.key.clone()) {
            out.nodes.push(n);
        } else {
            cfg_dupes += 1;
        }
    }
    // ---- phase 4: resolve calls -------------------------------------------
    for f in &mut files {
        out.edges.append(&mut f.edges);
    }
    out.edges.extend(impl_edges);

    // Expand each written path while the file that wrote it is still known:
    // `fs::read()` means `std::fs::read` only to the file that said `use
    // std::fs`. Impl methods arrive already expanded, for the same reason.
    let mut pending: Vec<(String, String, Call)> = Vec::new();
    for f in &mut files {
        let imports = import_index(&f.imports);
        let module = f.module.clone();
        let file = f.path.clone();
        for (caller, c) in std::mem::take(&mut f.calls) {
            let path = c.path.as_deref().map(|p| expand_path(p, &imports, &module));
            // The receiver's path hops too: `Database::in_memory().unwrap()`
            // names a type this file imported, and only this file knows
            // from where.
            let recv = c
                .recv
                .as_deref()
                .map(|r| expand_chain(r, &|p| expand_path(p, &imports, &module)));
            pending.push((caller, file.clone(), Call { path, recv, ..c }));
        }
    }
    pending.extend(impl_calls);

    // The unresolved ledger (P1): what could not be resolved becomes a
    // queryable UnresolvedRef node per (caller file, name) — attributed to
    // the caller's file so an incremental fold owns it — with a CALLS edge
    // per site carrying the reason. The report still counts; the graph now
    // also *shows*, so an agent's context names its blind spots.
    let mut unresolved_nodes: BTreeMap<String, Node> = BTreeMap::new();
    // One CALLS edge per (caller, target): `recv` joining call identity means
    // `a.m()` and `b.m()` both survive parse, and whichever resolution they
    // take must still fold to a single fact.
    let mut emitted: BTreeSet<(String, String)> = BTreeSet::new();
    let unresolved_edge = |nodes: &mut BTreeMap<String, Node>,
                           edges: &mut Vec<Edge>,
                           emitted: &mut BTreeSet<(String, String)>,
                           caller: &str,
                           file: &str,
                           call: &Call,
                           reason: String| {
        let key = format!("?::{file}::{}", call.name);
        nodes.entry(key.clone()).or_insert_with(|| Node {
            key: key.clone(),
            label: "UnresolvedRef".into(),
            extra_labels: Vec::new(),
            props: props([("name", call.name.clone()), ("file", file.to_string())]),
        });
        if !emitted.insert((caller.to_string(), key.clone())) {
            return;
        }
        let written = call.path.clone().unwrap_or_else(|| call.name.clone());
        let mut e = edge_at(caller, &key, "CALLS", call.line);
        stamp(&mut e, "unresolved", "none", &written);
        mark_concurrent(&mut e, call);
        e.props.insert("_reason".into(), Value::String(reason));
        // The receiver as it was read, so the blind spot names its cause:
        // `v.iter()` untyped means `v` is, and the next fix knows where.
        if let Some(recv) = &call.recv {
            e.props.insert("_recv".into(), Value::String(recv.clone()));
        }
        edges.push(e);
    };

    /// Say how control departs from waiting for this callee, when it does.
    ///
    /// The same property go and py put on a CALLS edge, in the same words: a
    /// reader who cannot tell a spawned call from a synchronous one is
    /// reading a different program.
    fn mark_concurrent(e: &mut Edge, call: &Call) {
        if let Some(shape) = &call.concurrent {
            e.props.insert(
                "concurrent".into(),
                json!({
                    "$desc": "how control departs from waiting for this callee: `spawned` runs it on another task or thread, `blocking` on the blocking pool",
                    "$value": shape.clone(),
                }),
            );
        }
    }

    // Deterministic receiver typing (P3). `self.m()` is the impl's own type;
    // `x.m()` types x through its annotation or its initializer — a call
    // whose declared return is on record, or a method chain through locals
    // already typed the same way, `?`/`.unwrap()` peeling a `Result`/`Option`
    // along the way. Every step reads something the source wrote — a miss at
    // any of them falls to the ledger, never to a guess.
    // Closure parameters typed by the callee's declared bound (the model_pbt
    // gap): resolve where each closure went, read the `Fn(...)` types its
    // signature states, and hand them to the caller's locals — computed
    // against a first Typing view, merged, then the view rebuilt.
    // What each closure yields, when its body is a chain — keyed by the call
    // it was handed to (`v.iter().map()` in the caller), so typing that hop
    // can read the body instead of giving up on the closure's result.
    let closure_bodies: BTreeMap<(String, String), String> = pending_closures
        .iter()
        .filter_map(|(caller, callee, _, _)| match callee {
            ClosureCallee::Method {
                recv: Some(recv),
                name,
                body: Some(body),
            } => Some(((caller.clone(), format!("{recv}.{name}()")), body.clone())),
            _ => None,
        })
        .collect();

    // Closure parameters typed by what the closure was handed to: a declared
    // callee's `Fn(...)` bound, or — for `v.iter().map(|x| …)`,
    // `opt.map(|x| …)`, `res.map_err(|e| …)` — what std passes, read from the
    // receiver's type arguments. A closure's parameters can type the
    // receiver of a closure inside it, so this runs until a round adds
    // nothing; three rounds cover any nesting a body actually has.
    for _round in 0..3 {
        let typing = Typing {
            declared: &declared,
            fns: &fns,
            types: &types,
            traits: &traits,
            scopes: &scopes,
            aliases: &aliases,
            returns_map: &returns_map,
            alias_map: &alias_map,
            closure_bodies: &closure_bodies,
            consts_by_name: &consts_by_name,
            const_types: &const_types,
            variant_map: &variant_map,
            local_inits: &local_inits,
            fields_map: &fields_map,
            impls_of: &impls_of,
            trait_impl_methods: &trait_impl_methods,
        };
        let mut derived: Vec<((String, String), LocalHint)> = Vec::new();
        for (caller, callee, idx, params) in &pending_closures {
            let module = scopes.get(caller).cloned().unwrap_or_default();
            // An annotation on the parameter itself beats any inference.
            for p in params {
                if let Some(w) = &p.written {
                    derived.push((
                        (caller.clone(), p.name.clone()),
                        LocalHint::Typed(w.clone()),
                    ));
                }
            }
            let declared_key = |p: &str| {
                if declared.contains(p) {
                    Some(p.to_string())
                } else if p.contains("::") {
                    resolve_relative(p, &module, &aliases, &declared)
                } else {
                    fns.get(p)
                        .and_then(|cands| pick_in_scope(&module, cands, &scopes))
                        .cloned()
                }
            };
            let from_bound = |fn_key: &str| -> Option<Vec<Ty>> {
                let args = closure_sig_map.get(&(fn_key.to_string(), *idx))?;
                let fmodule = scopes.get(fn_key).cloned().unwrap_or_default();
                Some(
                    args.iter()
                        .map(|w| typing.written_type(w, &fmodule).unwrap_or(Ty::Unknown))
                        .collect(),
                )
            };
            let types: Option<Vec<Ty>> = match callee {
                ClosureCallee::Path(p) => declared_key(p).and_then(|k| from_bound(&k)),
                ClosureCallee::Method { recv, name, .. } => recv
                    .as_deref()
                    .and_then(|r| typing.chain_type(caller, r, 24))
                    .and_then(|ty| match &ty {
                        Ty::Declared(t, _) => typing
                            .method_target(t, name)
                            .and_then(|(k, _)| from_bound(&k))
                            .or_else(|| closure_param_types(&ty, name, *idx)),
                        _ => closure_param_types(&ty, name, *idx),
                    }),
            };
            let Some(types) = types else { continue };
            for p in params {
                let Some(ty) = types.get(p.position) else {
                    continue;
                };
                let ty = match p.index {
                    Some(i) => ty.arg(i),
                    None => ty.clone(),
                };
                if ty != Ty::Unknown {
                    derived.push(((caller.clone(), p.name.clone()), LocalHint::Known(ty)));
                }
            }
        }
        let mut added = false;
        for (key, hint) in derived {
            if let std::collections::btree_map::Entry::Vacant(slot) = local_inits.entry(key) {
                slot.insert(hint);
                added = true;
            }
        }
        if !added {
            break;
        }
    }

    let typing = Typing {
        declared: &declared,
        fns: &fns,
        types: &types,
        traits: &traits,
        scopes: &scopes,
        aliases: &aliases,
        returns_map: &returns_map,
        alias_map: &alias_map,
        closure_bodies: &closure_bodies,
        consts_by_name: &consts_by_name,
        const_types: &const_types,
        variant_map: &variant_map,
        local_inits: &local_inits,
        fields_map: &fields_map,
        impls_of: &impls_of,
        trait_impl_methods: &trait_impl_methods,
    };
    // Making a value is not calling a function. `Ok(v)`, `Mine::A(v)`,
    // `Meters(1.0)` and `Wrapping(n)` all read as calls, and every one of
    // them constructs a type — the graph said `CALLS` to an invented
    // `Function` node named `Ok`, while the enum it names sat in the graph
    // unlinked. Every construction resolves to the *type*, with the variant
    // on the edge, so "who builds a `Mine`?" is one hop and no node is
    // fabricated for a constructor that is not an item.
    // A path nothing here declares is read by its casing, which is all that
    // is left to read — so the doubtful case stays a call: minting a node is
    // the more expensive way to be wrong.
    let construction_of = |p: &str, caller: &str, module: &str| -> Option<Construction> {
        // The prelude four. They are variants like any other, and naming
        // their enum is what makes `Result` and `Option` reachable at all.
        if let Some(e) = match p {
            "Ok" | "Err" => Some("Result"),
            "Some" | "None" => Some("Option"),
            _ => None,
        } {
            return Some(Construction::variant_of(e, p, true));
        }
        // `Self` names the type this impl is for, and nothing else. Left as
        // written it would mint a node called `Self` shared by every impl in
        // the tree — so it resolves against the caller's owner or not at all.
        let resolved;
        let p = match p == "Self" || p.starts_with("Self::") {
            false => p,
            true => {
                let owner = owner_of(caller).filter(|o| declared.contains(o))?;
                resolved = p.replacen("Self", &owner, 1);
                resolved.as_str()
            }
        };
        let (owner, leaf) = match p.rsplit_once("::") {
            Some((o, l)) => (Some(o), l),
            None => (None, p),
        };
        // Only a type-cased last segment can name a constructor: `Vec::new`
        // and `String::from` are functions.
        if !leaf.starts_with(|c: char| c.is_uppercase()) {
            return None;
        }
        // A declared enum's variant, when the segment before the leaf names
        // one — including a variant a `use Mine::A` brought into scope, which
        // arrives here already expanded back to `Mine::A`.
        if let Some(owner) = owner {
            let enum_key = if declared.contains(owner) {
                Some(owner.to_string())
            } else {
                resolve_relative(owner, module, &aliases, &declared).or_else(|| {
                    types
                        .get(owner.rsplit("::").next().unwrap_or(owner))
                        .and_then(|c| pick_in_scope(module, c, &scopes))
                        .cloned()
                })
            };
            if let Some(enum_key) = enum_key
                && variant_map.contains_key(&(enum_key.clone(), leaf.to_string()))
            {
                return Some(Construction::variant_of(&enum_key, leaf, false));
            }
        }
        // A declared tuple struct called by its own name.
        let whole = if declared.contains(p) {
            Some(p.to_string())
        } else {
            resolve_relative(p, module, &aliases, &declared).or_else(|| {
                types
                    .get(leaf)
                    .and_then(|c| pick_in_scope(module, c, &scopes))
                    .cloned()
            })
        };
        if let Some(key) = whole {
            return tuple_structs
                .contains(&key)
                .then(|| Construction::whole(&key, false));
        }
        // Nothing here declares it, so casing is all that is left to read —
        // and `SCREAMING` is a constant, not a type. Being wrong here mints a
        // node, so the doubtful case stays a call.
        if !leaf.contains(char::is_lowercase) {
            return None;
        }
        // A type-cased segment before the leaf makes the leaf that type's
        // variant — `Ordering::Less`; otherwise the whole path is the type —
        // `std::num::Wrapping`.
        match owner.and_then(|o| o.rsplit("::").next().map(|s| (o, s))) {
            Some((owner, last)) if last.starts_with(|c: char| c.is_uppercase()) => {
                Some(Construction::variant_of(owner, leaf, true))
            }
            _ => Some(Construction::whole(p, true)),
        }
    };
    // One INSTANTIATES edge per (caller, type, variant): a body that builds
    // two variants of one enum states two facts, not one.
    let mut inst_seen: BTreeSet<(String, String, String)> = BTreeSet::new();

    // Or why not: the reason the ledger edge carries.
    let typed_target =
        |caller: &str, call: &Call| -> Result<(String, &'static str, &'static str), String> {
            let recv = call.recv.as_deref().ok_or_else(|| {
                "receiver is not a name, field, call or literal chain".to_string()
            })?;
            let name = call.name.as_str();
            // `let r = r.unwrap();` — this call is the re-binding: its
            // receiver is what `r` was, which the hint filed under `r^`.
            let rebound = format!("{recv}^.{name}()");
            let recv = match typing
                .local_inits
                .get(&(caller.to_string(), recv.to_string()))
            {
                Some(LocalHint::Bound { chain, .. }) if chain == &rebound => format!("{recv}^"),
                _ => recv.to_string(),
            };
            let recv = recv.as_str();
            let ty = typing.chain_type_why(caller, recv, 24)?;
            match &ty {
                Ty::Declared(key, _) => {
                    if let Some((mk, how)) = typing.method_target(key, name) {
                        return Ok((mk, if recv == "self" { "self" } else { how }, "high"));
                    }
                    // No such method declared: a std trait's, when the name is
                    // one — `#[derive(Clone)]` writes no `fn clone` anywhere.
                    // Medium, because the impl is inferred from the name, not
                    // read.
                    let (tr, _) = std_trait(name).ok_or_else(|| {
                        format!("`{key}` declares no `{name}`, and it is no std trait's")
                    })?;
                    Ok((format!("{tr}::{name}"), "std-trait", "medium"))
                }
                Ty::External(path, _) => {
                    let answered = std_method(&ty, name).is_some();
                    if !answered && let Some((tr, _)) = std_trait(name) {
                        return Ok((format!("{tr}::{name}"), "std-trait", "medium"));
                    }
                    // `Range::collect` is a lie: `Range` declares no
                    // `collect`, it *is* an `Iterator` and `Iterator` declares
                    // it. Keyed by the receiver, one method scatters into a
                    // node per concrete iterator a tree happens to build —
                    // `Range::map`, `Lines::map`, `Chars::map` — and "who
                    // calls `Iterator::map`?" answers with a fraction of
                    // them. The node is the type that *answers* the call, so
                    // one method is one node. An inherent method a concrete
                    // iterator does declare (`Chars::as_str`) is answered by
                    // nothing in the iterator table and keeps its own name.
                    if answered && as_iterator(&ty).is_some() {
                        return Ok((format!("Iterator::{name}"), "std-iterator", "high"));
                    }
                    Ok((format!("{path}::{name}"), "external-receiver", "high"))
                }
                Ty::Tuple(_) | Ty::Unknown => {
                    let (tr, _) = std_trait(name).ok_or_else(|| {
                        format!(
                            "receiver is a `{}`, which declares no `{name}`",
                            ty.display()
                        )
                    })?;
                    Ok((format!("{tr}::{name}"), "std-trait", "medium"))
                }
            }
        };

    for (caller, file, call) in pending {
        let written = call.path.clone().unwrap_or_else(|| call.name.clone());
        let emit = |out: &mut Assembled,
                    emitted: &mut BTreeSet<(String, String)>,
                    target: &str,
                    strategy: &str,
                    band: &str| {
            if emitted.insert((caller.clone(), target.to_string())) {
                let mut e = edge_at(&caller, target, "CALLS", call.line);
                stamp(&mut e, strategy, band, &written);
                mark_concurrent(&mut e, &call);
                out.edges.push(e);
            }
        };

        if let Some(p) = &call.path {
            // Construction first: `Ok(v)` and `Meters(1.0)` are spelled like
            // calls and are not calls, and reading them as one is what put a
            // `Function` named `Ok` in the graph.
            let module = scopes.get(&caller).cloned().unwrap_or_default();
            if let Some(built) = construction_of(p, &caller, &module) {
                emit_instantiation(
                    &mut out.edges,
                    &mut external,
                    &mut inst_seen,
                    &caller,
                    &built,
                    call.line,
                );
                continue;
            }
            // A path written out in full names its target exactly, which beats
            // matching on a last segment two functions may share.
            if declared.contains(p) {
                emit(&mut out, &mut emitted, p, "path", "high");
                continue;
            }
            if p.contains("::") {
                // A qualified path is the whole claim: read it relative to the
                // caller's module, follow re-exports, and when neither lands
                // on a declaration the callee is external. The last segment is
                // deliberately NOT matched against local names — that fallback
                // is what once bound `store::remove(…)` to a same-named local
                // method as a false self-edge.
                let module = scopes.get(&caller).cloned().unwrap_or_default();
                match resolve_relative(p, &module, &aliases, &declared) {
                    Some(target) => emit(&mut out, &mut emitted, &target, "path", "high"),
                    // Nothing here defines it: `std::fs::read`, or a
                    // dependency's. We do not read that code and have no
                    // signature for it, so the node is the path and the fact
                    // that it was called — which is what "this crate uses
                    // that" needs.
                    None => {
                        note_external(&mut external, p, Some("Function"));
                        emit(&mut out, &mut emitted, p, "external-path", "high");
                        external_calls += 1;
                    }
                }
                continue;
            }
            // A bare name: the innermost scope holding exactly one candidate.
            match fns.get(&call.name) {
                Some(cands) => match pick(&caller, cands, &scopes) {
                    Some(one) => {
                        let one = one.clone();
                        emit(&mut out, &mut emitted, &one, "scope", "medium");
                    }
                    // Defined here under this name, but more than one
                    // candidate is equally close — a guess would be worse
                    // than a gap.
                    None => {
                        ambiguous += 1;
                        unresolved_edge(
                            &mut unresolved_nodes,
                            &mut out.edges,
                            &mut emitted,
                            &caller,
                            &file,
                            &call,
                            format!("ambiguous: {} candidates", cands.len()),
                        );
                    }
                },
                // Nothing declares the name: a prelude or glob-imported
                // function, external and keyed by all we have — the name.
                None => {
                    note_external(&mut external, p, Some("Function"));
                    emit(&mut out, &mut emitted, p, "external-path", "high");
                    external_calls += 1;
                }
            }
            continue;
        }

        // A method call writes only `.read()`. When the body states the
        // receiver's type it resolves — into std and other crates too, as an
        // external node that is the path and the fact it was called;
        // otherwise it is counted, never guessed — but shown, as an
        // UnresolvedRef the graph can answer for.
        let why = match typed_target(&caller, &call) {
            Ok((target, strategy, band)) => {
                if matches!(strategy, "external-receiver" | "std-trait" | "std-iterator") {
                    note_external(&mut external, &target, Some("Method"));
                    external_calls += 1;
                }
                emit(&mut out, &mut emitted, &target, strategy, band);
                continue;
            }
            Err(why) => why,
        };
        unresolved += 1;
        unresolved_edge(
            &mut unresolved_nodes,
            &mut out.edges,
            &mut emitted,
            &caller,
            &file,
            &call,
            format!("method call: receiver type unknown — {why}"),
        );
    }
    // Struct literals: a `Widget { .. }` uses the type as surely as a call
    // uses a function — resolved the same way, emitted as INSTANTIATES. A
    // `Mine::B { .. }` is the same act on an enum, and points at the enum.
    for (caller, p, line) in pending_insts {
        let module = scopes.get(&caller).cloned().unwrap_or_default();
        let found = if declared.contains(&p) {
            Some(Construction::whole(&p, false))
        } else if let Some(hit) = resolve_relative(&p, &module, &aliases, &declared) {
            Some(Construction::whole(&hit, false))
        } else if let Some(hit) = construction_of(&p, &caller, &module) {
            Some(hit)
        } else if p.contains("::") {
            Some(Construction::whole(&p, true))
        } else {
            types
                .get(&p)
                .and_then(|cands| pick_in_scope(&module, cands, &scopes))
                .map(|k| Construction::whole(k, false))
        };
        if let Some(built) = found {
            emit_instantiation(
                &mut out.edges,
                &mut external,
                &mut inst_seen,
                &caller,
                &built,
                line,
            );
        }
    }

    // Functions passed as values (C4, narrow): a bare in-tree name in
    // argument position becomes a REFERENCES edge — same innermost-unique
    // discipline as bare calls, silent on a miss (most idents are values),
    // never a self-loop.
    let mut ref_seen: BTreeSet<(String, String)> = BTreeSet::new();
    for (caller, name, line) in pending_fn_refs {
        let Some(cands) = fns.get(&name) else {
            continue;
        };
        let Some(target) = pick(&caller, cands, &scopes) else {
            continue;
        };
        if *target == caller || !ref_seen.insert((caller.clone(), target.clone())) {
            continue;
        }
        let mut e = edge_at(&caller, target, "REFERENCES", line);
        stamp(&mut e, "fn-ref", "high", &name);
        out.edges.push(e);
    }

    // A declared type is a dependency as surely as a call is. Until this
    // existed, `impact` on a type saw only who *built* one — never who takes
    // it as a parameter, returns it, or holds it in a field — and answered a
    // fraction of the blast radius with no sign that it had.
    //
    // Every source below already existed for receiver typing, with paths
    // expanded and `Self` resolved; parameters were the one position nothing
    // else needed. Only types this tree declares get an edge: a foreign one
    // stays in `signature`/`fields` as text, which is all this parser knows
    // about it anyway.
    let mut type_refs: Vec<(&str, String, String, String)> = Vec::new(); // role, owner, name, written
    for ((owner, field), (written, _)) in &fields_map {
        type_refs.push(("field", owner.clone(), field.clone(), written.clone()));
    }
    for ((owner, variant), (fields, _)) in &variant_map {
        for (_, written) in fields {
            type_refs.push(("variant", owner.clone(), variant.clone(), written.clone()));
        }
    }
    for (owner, written) in &returns_map {
        type_refs.push(("return", owner.clone(), String::new(), written.clone()));
    }
    for (owner, (_, target, _)) in &alias_map {
        type_refs.push(("alias", owner.clone(), String::new(), target.clone()));
    }
    for (owner, name, written) in &param_types {
        type_refs.push(("param", owner.clone(), name.clone(), written.clone()));
    }

    // One edge per (owner, type) carrying the union of the roles, the way a
    // repeated call folds to one edge: a function taking a `Cfg` and
    // returning one states two facts about one dependency.
    let mut uses: BTreeMap<(String, String), (BTreeSet<&str>, String)> = BTreeMap::new();
    let mut foreign_type_refs = 0usize;
    for (role, owner, name, written) in type_refs {
        let module = scopes.get(&owner).cloned().unwrap_or_default();
        let mut named = Vec::new();
        named_types(&written, &mut named);
        let mut landed = false;
        for base in named {
            let Some(target) = typing.type_key(&base, &module) else {
                continue;
            };
            landed = true;
            // A type that mentions itself (`next: Option<Box<Node>>`) is a
            // real shape and a useless edge — the same call `REFERENCES`
            // makes.
            if target == owner {
                continue;
            }
            let slot = uses
                .entry((owner.clone(), target))
                .or_insert_with(|| (BTreeSet::new(), name.clone()));
            slot.0.insert(role);
        }
        if !landed {
            foreign_type_refs += 1;
        }
    }
    let type_ref_edges = uses.len();
    for ((owner, target), (roles, name)) in uses {
        let mut e = edge_at(&owner, &target, "USES_TYPE", 0);
        e.props.remove("line");
        e.props.insert(
            "role".into(),
            json!({
                "$desc": "the type position this declaration names the type in",
                "$value": roles.iter().copied().collect::<Vec<_>>().join(", "),
            }),
        );
        if !name.is_empty() {
            e.props.insert("name".into(), Value::String(name));
        }
        out.edges.push(e);
    }

    out.nodes.extend(unresolved_nodes.into_values());

    // Last, because resolving calls is itself a source of external nodes: a
    // path into std is discovered at its call site, not before. Never written
    // over a real one — a type this crate defines is not external, and a
    // stand-in carrying its key would replace it.
    out.nodes
        .extend(external.into_values().filter(|n| !seen.contains(&n.key)));

    // Say what was not emitted, so a thin call graph is explained rather than
    // taken for the whole truth.
    if unresolved > 0 {
        out.notes.push(format!(
            "{unresolved} method call(s) left unresolved: a call written `.read()` \
             names no path, and nothing in the body states the receiver's type"
        ));
    }
    if external_calls > 0 {
        out.notes.push(format!(
            "{external_calls} call(s) into other crates and the standard library, \
             recorded as external nodes carrying the path and nothing else"
        ));
    }
    if ambiguous > 0 {
        out.notes.push(format!(
            "{ambiguous} call(s) matched more than one function by name and were \
             left out: resolving them needs import and generic resolution"
        ));
    }
    if derives_recorded > 0 {
        out.notes.push(format!(
            "{derives_recorded} derived impl(s) recorded as `IMPLEMENTS` with `derived` on \
             the edge — what `#[derive(...)]` states, which a hand-written `impl` has \
             always recorded"
        ));
    }
    if annotations_recorded > 0 {
        out.notes.push(format!(
            "{annotations_recorded} attribute(s) recorded as `ANNOTATED_BY`, the whole \
             attribute on the edge; lint, codegen and `cfg` attributes are not among them"
        ));
    }
    if type_ref_edges > 0 || foreign_type_refs > 0 {
        out.notes.push(format!(
            "{type_ref_edges} type reference(s) recorded as `USES_TYPE` — what a \
             declaration's fields, parameters, returns and variants are typed by; \
             {foreign_type_refs} more name types this tree does not declare and are \
             left as written text on the node"
        ));
    }
    if macro_invocations > 0 {
        out.notes.push(format!(
            "{macro_invocations} macro invocation(s) at item level were not expanded — \
             whatever items they define are absent from the graph, and an `INVOKES` \
             edge marks where"
        ));
    }
    if cfg_dupes > 0 {
        out.notes.push(format!(
            "{cfg_dupes} item(s) shared a key with an earlier one and the first \
             was kept — usually two `#[cfg]` alternatives of the same item"
        ));
    }
    if !unparsed.is_empty() {
        out.skipped += unparsed.len();
        out.notes
            .push(format!("{} file(s) did not parse", unparsed.len()));
        out.notes.extend(unparsed);
    }
    out
}

/// A file's `use` statements as `simple name → path as written`.
///
/// This is the nearest thing to name resolution a parser has. `syn` sees one
/// file's tokens: `impl Database` is the bare identifier `Database` and nothing
/// more, and turning that into `dr_strange_core::api::Database` is name
/// resolution — the module tree, `use` in scope, globs, re-exports — which is a
/// compiler's job. But a file that says `use super::Database` has *written down*
/// where its `Database` comes from, and that is worth reading.
fn import_index(imports: &[(String, String, String, u64)]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (_, name, path, _) in imports {
        if name != "*" {
            // First writing wins: a later glob or a re-import should not
            // displace an explicit one.
            out.entry(name.clone()).or_insert_with(|| path.clone());
        }
    }
    out
}

/// Rewrite a written `use` path into this graph's key namespace.
///
/// `crate::`, `super::` and `self::` are relative to the file that wrote them,
/// and the key namespace is rooted at the package name instead.
fn absolute(written: &str, module: &str) -> String {
    let crate_root = module.split("::").next().unwrap_or(module);
    match written.split_once("::") {
        Some(("crate", rest)) => format!("{crate_root}::{rest}"),
        Some(("self", rest)) => format!("{module}::{rest}"),
        Some(("super", rest)) => match module.rsplit_once("::") {
            Some((parent, _)) => format!("{parent}::{rest}"),
            None => format!("{crate_root}::{rest}"),
        },
        _ => written.to_string(),
    }
}

/// The node a written path names, as far as this tree can tell.
///
/// Two readings are tried, in order: the path as expanded, then the same path
/// read as relative to the module that wrote it — `use snapshot::SnapshotStats`
/// in `api/mod.rs` names a child module, which neither an import nor a `crate::`
/// prefix will expand. Each is followed through re-exports, since a facade is
/// just as good an answer as a declaration and is far more common at the top of
/// a crate.
///
/// Falls back to the expanded path, which becomes an external node: better a
/// node keyed by what the source wrote than an edge pointing nowhere.
fn resolve_path(
    expanded: &str,
    module: &str,
    aliases: &BTreeMap<String, String>,
    declared: &BTreeSet<String>,
) -> String {
    [expanded.to_string(), format!("{module}::{expanded}")]
        .into_iter()
        .find_map(|candidate| follow_reexports(&candidate, aliases, declared))
        .unwrap_or_else(|| expanded.to_string())
}

/// Follow `pub use` re-exports until the path names something declared.
///
/// A facade is the normal shape of a Rust crate: `lib.rs` writes
/// `pub use api::cache;` and from then on everything says `crate::cache::…`,
/// while the items are declared under `api::cache`. Nothing is declared at the
/// facade path, so without following it every such import becomes a node that
/// looks external and is not — the crate's own type, filed under a foreign key.
///
/// Bounded rather than recursive-until-fixpoint: re-exports chain through
/// several layers in a large crate, and a mutual pair must not spin.
fn follow_reexports(
    key: &str,
    aliases: &BTreeMap<String, String>,
    declared: &BTreeSet<String>,
) -> Option<String> {
    let mut current = key.to_string();
    for _ in 0..8 {
        if declared.contains(&current) {
            return Some(current);
        }
        // The longest matching prefix, so a re-export of a module beats one of
        // its parent.
        let mut probe = current.as_str();
        let rewritten = loop {
            if let Some(target) = aliases.get(probe) {
                break Some(format!("{target}{}", &current[probe.len()..]));
            }
            match probe.rsplit_once("::") {
                Some((parent, _)) => probe = parent,
                None => break None,
            }
        };
        current = rewritten?;
    }
    None
}

/// A path as the file that wrote it meant it.
///
/// `fs::read` is `std::fs::read` only to a file that said `use std::fs`, and
/// `crate::` is relative to whoever wrote it. Anything already absolute —
/// `std::fs::read` written out in full — passes through untouched.
fn expand_path(path: &str, imports: &BTreeMap<String, String>, module: &str) -> String {
    if path.starts_with("crate::") || path.starts_with("super::") || path.starts_with("self::") {
        return absolute(path, module);
    }
    let (head, rest) = path.split_once("::").unwrap_or((path, ""));
    match imports.get(head) {
        Some(written) => {
            let base = absolute(written, module);
            if rest.is_empty() {
                base
            } else {
                format!("{base}::{rest}")
            }
        }
        None => path.to_string(),
    }
}

/// The key an `impl` block's type or trait refers to.
///
/// Three answers, in descending order of how much the source actually told us:
/// what the file imported, then the nearest declaration of that name, then a
/// node marked `external` — because `impl Display for X` is worth recording
/// even though `Display` lives in std, and because an edge whose endpoint does
/// not exist is refused by the write path rather than merely being imprecise.
#[allow(clippy::too_many_arguments)]
fn resolve(
    name: &str,
    module: &str,
    imports: &BTreeMap<String, String>,
    by_name: &BTreeMap<String, Vec<String>>,
    declared: &BTreeSet<String>,
    scopes: &BTreeMap<String, String>,
    external_label: &str,
    external: &mut BTreeMap<String, Node>,
) -> String {
    // Written out in full, or imported: either way the source named it.
    let simple = name.rsplit("::").next().unwrap_or(name);
    for candidate in [
        Some(absolute(name, module)),
        imports.get(simple).map(|w| absolute(w, module)),
    ]
    .into_iter()
    .flatten()
    {
        if declared.contains(&candidate) {
            return candidate;
        }
    }

    if let Some(cands) = by_name.get(simple)
        && let Some(one) = pick_in_scope(module, cands, scopes)
    {
        return one.clone();
    }

    // Nothing here declares it, so it becomes a stand-in keyed by the **path**,
    // expanded through this file's imports exactly as a call into another crate
    // is. `HashSet` alone would merge every crate's `HashSet` into one node and
    // would not match `std::collections::HashSet` written out at another site.
    let key = expand_path(name, imports, module);
    note_external(external, &key, Some(external_label));
    key
}

/// The declared key a written path names when read from `module`: the path
/// itself, then prefixed with `module` and each of its ancestors — innermost
/// first, the way scope narrowing reads — each probe followed through
/// re-exports. The ancestor walk is what makes `Database::in_memory()` under
/// a `use super::*` glob resolve: the glob puts the parent's names in scope,
/// and every segment of the written path still has to match a declared key,
/// so this stays a path match, never a bare-name guess.
fn resolve_relative(
    path: &str,
    module: &str,
    aliases: &BTreeMap<String, String>,
    declared: &BTreeSet<String>,
) -> Option<String> {
    if let Some(hit) = follow_reexports(path, aliases, declared) {
        return Some(hit);
    }
    let mut scope = module;
    loop {
        if let Some(hit) = follow_reexports(&format!("{scope}::{path}"), aliases, declared) {
            return Some(hit);
        }
        match scope.rsplit_once("::") {
            Some((parent, _)) => scope = parent,
            None => return None,
        }
    }
}

/// The type an impl-method key belongs to: `<Type as Trait>::m` and
/// `path::Type::m` both name it.
fn owner_of(key: &str) -> Option<String> {
    match key.strip_prefix('<') {
        Some(rest) => rest.split(" as ").next().map(str::to_string),
        None => key.rsplit_once("::").map(|(o, _)| o.to_string()),
    }
}

/// Everything receiver typing reads, together so it can recurse: typing
/// `txn` in `let txn = plane.write().unwrap()` means typing `plane` first.
struct Typing<'a> {
    declared: &'a BTreeSet<String>,
    fns: &'a BTreeMap<String, Vec<String>>,
    types: &'a BTreeMap<String, Vec<String>>,
    traits: &'a BTreeMap<String, Vec<String>>,
    scopes: &'a BTreeMap<String, String>,
    aliases: &'a BTreeMap<String, String>,
    returns_map: &'a BTreeMap<String, String>,
    alias_map: &'a BTreeMap<String, (Vec<String>, String, String)>,
    closure_bodies: &'a BTreeMap<(String, String), String>,
    consts_by_name: &'a BTreeMap<String, Vec<String>>,
    const_types: &'a BTreeMap<String, (String, String)>,
    variant_map: &'a BTreeMap<(String, String), (VariantFields, String)>,
    local_inits: &'a BTreeMap<(String, String), LocalHint>,
    fields_map: &'a BTreeMap<(String, String), (String, String)>,
    impls_of: &'a BTreeMap<String, Vec<String>>,
    trait_impl_methods: &'a BTreeMap<(String, String), String>,
}

/// A receiver's type, as far as this parser can know it — with the type
/// arguments the source wrote, since `Vec<Node>` is what makes `for n in
/// &v` type `n`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum Ty {
    /// A type this tree declares — its methods are declared keys.
    Declared(String, Vec<Ty>),
    /// A type it does not — std's, a dependency's — known by its path alone.
    /// Its methods are external stand-ins, `path::method`.
    External(String, Vec<Ty>),
    /// `(K, V)`: what a map iterates, what `enumerate` yields.
    Tuple(Vec<Ty>),
    /// Known to be something, but not what: a `_`, a generic parameter, the
    /// result of a closure nobody declared.
    Unknown,
}

impl Ty {
    fn ext(path: &str, args: Vec<Ty>) -> Ty {
        Ty::External(path.to_string(), args)
    }

    /// The path, for a message; a tuple and an unknown say what they are.
    fn path(&self) -> &str {
        match self {
            Ty::Declared(p, _) | Ty::External(p, _) => p,
            Ty::Tuple(_) => "(tuple)",
            Ty::Unknown => "_",
        }
    }

    /// The last segment — what the std tables are keyed by.
    fn short(&self) -> &str {
        let p = self.path();
        p.rsplit("::").next().unwrap_or(p)
    }

    fn args(&self) -> &[Ty] {
        match self {
            Ty::Declared(_, a) | Ty::External(_, a) | Ty::Tuple(a) => a,
            Ty::Unknown => &[],
        }
    }

    /// Argument `i`, or unknown when the source did not write one.
    fn arg(&self, i: usize) -> Ty {
        self.args().get(i).cloned().unwrap_or(Ty::Unknown)
    }

    fn known(self) -> Option<Ty> {
        (self != Ty::Unknown).then_some(self)
    }

    /// As a reader would write it: `Vec<Node>`, `(usize, Node)`.
    fn display(&self) -> String {
        let args = |a: &[Ty]| a.iter().map(Ty::display).collect::<Vec<_>>().join(", ");
        match self {
            Ty::Declared(p, a) | Ty::External(p, a) if a.is_empty() => p.clone(),
            Ty::Declared(p, a) | Ty::External(p, a) => format!("{p}<{}>", args(a)),
            Ty::Tuple(a) => format!("({})", args(a)),
            Ty::Unknown => "_".into(),
        }
    }
}

/// What `*x` is when `x` is a smart pointer or an owning type: the type its
/// methods fall through to. `String` → `str`, `Vec<T>` → `slice<T>`,
/// `PathBuf` → `Path`; anything else is itself.
fn deref(ty: &Ty) -> Ty {
    match ty.short() {
        "String" => Ty::ext("str", vec![]),
        "Vec" => Ty::ext("slice", vec![ty.arg(0)]),
        "PathBuf" => Ty::ext("std::path::Path", vec![]),
        "OsString" => Ty::ext("std::ffi::OsStr", vec![]),
        _ => ty.clone(),
    }
}

/// What `v[i]` is, by `v`'s type: a map's value, a string's slice, a
/// sequence's item.
fn index_of(ty: &Ty) -> Option<Ty> {
    Some(match ty.short() {
        "HashMap" | "BTreeMap" | "Map" => ty.arg(1),
        "str" | "String" => Ty::ext("str", vec![]),
        "Value" => Ty::ext("Value", vec![]),
        _ => return item_of(ty),
    })
}

/// A std type that *is* an iterator — `ReadDir`, `Lines`, `Chars`, a
/// `Range` — as the `Iterator` the tables are keyed by, its item from
/// [`item_of`].
fn as_iterator(ty: &Ty) -> Option<Ty> {
    const ITERATORS: &[&str] = &[
        "ReadDir",
        "Lines",
        "Chars",
        "Split",
        "SplitWhitespace",
        "Keys",
        "Values",
        "Iter",
        "IterMut",
        "IntoIter",
        "Range",
        "RangeInclusive",
        "Drain",
        "Bytes",
        "CharIndices",
    ];
    ITERATORS
        .contains(&ty.short())
        .then(|| Ty::ext("Iterator", vec![item_of(ty).unwrap_or(Ty::Unknown)]))
}

/// What iterating a value yields — `for x in v` — by the value's type.
fn item_of(ty: &Ty) -> Option<Ty> {
    Some(match ty.short() {
        "Vec" | "VecDeque" | "slice" | "array" | "HashSet" | "BTreeSet" | "BinaryHeap"
        | "Option" | "Result" | "Iterator" | "IntoIterator" | "LinkedList" => ty.arg(0),
        "HashMap" | "BTreeMap" => Ty::Tuple(vec![ty.arg(0), ty.arg(1)]),
        "ReadDir" => Ty::ext(
            "Result",
            vec![Ty::ext("std::fs::DirEntry", vec![]), Ty::Unknown],
        ),
        "Lines" | "Split" | "SplitWhitespace" => Ty::ext("str", vec![]),
        "Chars" => Ty::ext("char", vec![]),
        "Bytes" => Ty::ext("u8", vec![]),
        "Range" | "RangeInclusive" | "Keys" | "Values" | "Iter" | "IterMut" | "IntoIter"
        | "Drain" => ty.arg(0),
        _ => return None,
    })
}

/// The inherent methods of the std types a body most often holds, with what
/// each returns — the facts that let a chain go on past a hop into std, and
/// that make `Option::map` the target of `.map()` on an `Option` rather than
/// a guess. Keyed by the type's short name; `Iterator` stands for every
/// iterator, since the concrete adapter types are std's business and never
/// spelled by the source. Type arguments carry through: `Vec<Node>::first()`
/// is an `Option<Node>`, and `.unwrap()` on that is a `Node`.
///
/// `None` is a method not in the table — the type is still known, so a
/// call on it resolves to `Type::method`, but the chain stops there.
/// `Some(Ty::Unknown)` is a method that is std's but returns what this
/// parser cannot say: a closure's result, a `collect` target.
fn std_method(recv: &Ty, method: &str) -> Option<Ty> {
    use Ty::Unknown;
    let as_iter = as_iterator(recv);
    let recv = as_iter.as_ref().unwrap_or(recv);
    let a = |i: usize| recv.arg(i);
    let ext = |p: &str| Ty::ext(p, vec![]);
    let iter = |t: Ty| Ty::ext("Iterator", vec![t]);
    let opt = |t: Ty| Ty::ext("Option", vec![t]);
    let res = |t: Ty, e: Ty| Ty::ext("Result", vec![t, e]);
    let usize_ = || ext("usize");
    let bool_ = || ext("bool");
    let string = || ext("String");
    let str_ = || ext("str");
    let same = || recv.clone();
    let name = recv.short();
    Some(match (name, method) {
        (_, "iter" | "iter_mut" | "into_iter" | "drain") if name != "Iterator" => {
            iter(item_of(recv).unwrap_or(Unknown))
        }
        ("Iterator", m) => match m {
            "filter" | "take" | "skip" | "chain" | "rev" | "cloned" | "copied" | "peekable"
            | "take_while" | "skip_while" | "inspect" | "step_by" | "fuse" | "by_ref"
            | "sorted" | "dedup" => same(),
            "map" | "filter_map" | "flat_map" | "scan" | "map_while" => iter(Unknown),
            "enumerate" => iter(Ty::Tuple(vec![usize_(), a(0)])),
            "zip" => iter(Ty::Tuple(vec![a(0), Unknown])),
            "flatten" => iter(item_of(&a(0)).unwrap_or(Unknown)),
            "next" | "last" | "nth" | "find" | "max" | "min" | "max_by_key" | "min_by_key"
            | "max_by" | "min_by" | "reduce" | "next_back" => opt(a(0)),
            "find_map" => opt(Unknown),
            "position" | "rposition" => opt(usize_()),
            "count" => usize_(),
            "any" | "all" => bool_(),
            "collect" | "sum" | "product" | "fold" | "unzip" | "partition" | "for_each"
            | "try_for_each" | "try_fold" => Unknown,
            _ => return None,
        },
        ("Option", m) => match m {
            "map" | "and_then" => opt(Unknown),
            "filter" | "take" | "replace" | "or" | "or_else" | "xor" | "inspect" | "cloned"
            | "copied" | "as_ref" | "as_mut" | "take_if" => same(),
            "as_deref" | "as_deref_mut" => opt(deref(&a(0))),
            "zip" => opt(Ty::Tuple(vec![a(0), Unknown])),
            "flatten" => a(0),
            "ok_or" | "ok_or_else" => res(a(0), Unknown),
            "is_some" | "is_none" | "is_some_and" | "is_none_or" => bool_(),
            "unwrap" | "expect" | "unwrap_or" | "unwrap_or_else" | "unwrap_or_default"
            | "unwrap_unchecked" | "get_or_insert_with" | "get_or_insert" | "insert" => a(0),
            "map_or" | "map_or_else" | "transpose" => Unknown,
            _ => return None,
        },
        ("Result", m) => match m {
            "map" | "and_then" => res(Unknown, a(1)),
            "map_err" | "or_else" | "context" | "with_context" => res(a(0), Unknown),
            "as_ref" | "as_mut" | "inspect" | "inspect_err" => same(),
            "as_deref" => res(deref(&a(0)), a(1)),
            "ok" => opt(a(0)),
            "err" => opt(a(1)),
            "is_ok" | "is_err" | "is_ok_and" | "is_err_and" => bool_(),
            "unwrap" | "expect" | "unwrap_or" | "unwrap_or_else" | "unwrap_or_default"
            | "unwrap_unchecked" => a(0),
            "unwrap_err" | "expect_err" => a(1),
            "map_or" | "map_or_else" => Unknown,
            _ => return None,
        },
        ("Vec" | "VecDeque" | "slice" | "array" | "BinaryHeap", m) => match m {
            "windows" | "chunks" | "chunks_exact" | "rchunks" | "split" | "splitn" => {
                iter(Ty::ext("slice", vec![a(0)]))
            }
            "first" | "last" | "get" | "get_mut" | "first_mut" | "last_mut" | "pop"
            | "pop_front" | "pop_back" | "front" | "back" | "peek" => opt(a(0)),
            "remove" | "swap_remove" => a(0),
            "binary_search" | "binary_search_by" | "binary_search_by_key" => {
                res(usize_(), usize_())
            }
            "len" | "capacity" => usize_(),
            "is_empty" | "contains" | "starts_with" | "ends_with" | "is_sorted" => bool_(),
            "join" if name != "VecDeque" => string(),
            "to_vec" | "split_off" | "into_sorted_vec" | "into_vec" => Ty::ext("Vec", vec![a(0)]),
            "as_slice" | "as_mut_slice" => Ty::ext("slice", vec![a(0)]),
            "push"
            | "push_back"
            | "push_front"
            | "insert"
            | "sort"
            | "sort_by"
            | "sort_by_key"
            | "sort_unstable"
            | "sort_unstable_by"
            | "sort_unstable_by_key"
            | "dedup"
            | "dedup_by_key"
            | "dedup_by"
            | "reverse"
            | "clear"
            | "truncate"
            | "extend"
            | "extend_from_slice"
            | "retain"
            | "retain_mut"
            | "resize"
            | "reserve"
            | "shrink_to_fit"
            | "fill"
            | "swap"
            | "copy_from_slice"
            | "clone_from_slice"
            | "rotate_left"
            | "rotate_right"
            | "append" => Unknown,
            _ => return None,
        },
        ("HashMap" | "BTreeMap", m) => match m {
            "get" | "get_mut" | "remove" | "insert" => opt(a(1)),
            "get_key_value" | "remove_entry" | "first_key_value" | "last_key_value"
            | "pop_first" | "pop_last" => opt(Ty::Tuple(vec![a(0), a(1)])),
            "keys" | "into_keys" => iter(a(0)),
            "values" | "values_mut" | "into_values" => iter(a(1)),
            "range" | "range_mut" => iter(Ty::Tuple(vec![a(0), a(1)])),
            "len" => usize_(),
            "contains_key" | "is_empty" => bool_(),
            "entry" => Ty::ext("Entry", vec![a(0), a(1)]),
            "clear" | "extend" | "retain" | "reserve" | "append" | "split_off" => Unknown,
            _ => return None,
        },
        ("Entry", m) => match m {
            "or_insert" | "or_insert_with" | "or_default" | "or_insert_with_key" => a(1),
            "and_modify" => same(),
            "key" => a(0),
            _ => return None,
        },
        ("HashSet" | "BTreeSet", m) => match m {
            "union" | "intersection" | "difference" | "symmetric_difference" | "range" => {
                iter(a(0))
            }
            "insert" | "remove" | "contains" | "is_empty" | "is_subset" | "is_superset"
            | "is_disjoint" => bool_(),
            "len" => usize_(),
            "get" | "take" | "first" | "last" | "pop_first" | "pop_last" => opt(a(0)),
            "clear" | "extend" | "retain" | "reserve" | "append" | "split_off" => Unknown,
            _ => return None,
        },
        ("str" | "String", m) => match m {
            "trim" | "trim_start" | "trim_end" | "trim_matches" | "trim_start_matches"
            | "trim_end_matches" | "as_str" | "as_mut_str" => str_(),
            "to_lowercase" | "to_uppercase" | "to_ascii_lowercase" | "to_ascii_uppercase"
            | "replace" | "replacen" | "repeat" => string(),
            "chars" => iter(ext("char")),
            "bytes" => iter(ext("u8")),
            "lines"
            | "split"
            | "rsplit"
            | "splitn"
            | "rsplitn"
            | "split_whitespace"
            | "split_ascii_whitespace"
            | "split_terminator"
            | "matches"
            | "split_inclusive" => iter(str_()),
            "char_indices" => iter(Ty::Tuple(vec![usize_(), ext("char")])),
            "match_indices" => iter(Ty::Tuple(vec![usize_(), str_()])),
            "encode_utf16" => iter(ext("u16")),
            "find" | "rfind" => opt(usize_()),
            "strip_prefix" | "strip_suffix" | "get" => opt(str_()),
            "split_once" | "rsplit_once" => opt(Ty::Tuple(vec![str_(), str_()])),
            "pop" => opt(ext("char")),
            "parse" => res(Unknown, Unknown),
            "len" | "capacity" => usize_(),
            "is_empty"
            | "contains"
            | "starts_with"
            | "ends_with"
            | "eq_ignore_ascii_case"
            | "is_char_boundary"
            | "is_ascii" => bool_(),
            "as_bytes" => Ty::ext("slice", vec![ext("u8")]),
            "into_bytes" => Ty::ext("Vec", vec![ext("u8")]),
            "into_boxed_str" => str_(),
            "push"
            | "push_str"
            | "clear"
            | "truncate"
            | "insert"
            | "insert_str"
            | "extend"
            | "retain"
            | "reserve"
            | "remove"
            | "make_ascii_lowercase"
            | "make_ascii_uppercase" => Unknown,
            _ => return None,
        },
        ("Path" | "PathBuf", m) => match m {
            "join" | "with_extension" | "with_file_name" | "to_path_buf" => {
                ext("std::path::PathBuf")
            }
            "parent" => opt(ext("std::path::Path")),
            "file_name" | "extension" | "file_stem" => opt(ext("std::ffi::OsStr")),
            "to_str" => opt(str_()),
            // `Cow<str>`, whose methods are `str`'s.
            "to_string_lossy" => str_(),
            "exists" | "is_file" | "is_dir" | "is_absolute" | "is_relative" | "starts_with"
            | "ends_with" | "has_root" | "is_symlink" => bool_(),
            "components" | "ancestors" => iter(Unknown),
            "as_path" => ext("std::path::Path"),
            "strip_prefix" => res(ext("std::path::Path"), Unknown),
            "canonicalize" => res(ext("std::path::PathBuf"), Unknown),
            "metadata" | "read_dir" => res(Unknown, Unknown),
            "display" | "as_os_str" | "push" | "pop" | "set_extension" | "set_file_name" => Unknown,
            _ => return None,
        },
        // A value typed by a std trait bound answers that trait's method
        // with what the bound wrote: `impl AsRef<Path>` is a `Path` after
        // `.as_ref()`.
        ("AsRef", "as_ref")
        | ("AsMut", "as_mut")
        | ("Into", "into")
        | ("Borrow", "borrow")
        | ("BorrowMut", "borrow_mut") => a(0),
        ("TryInto", "try_into") => res(a(0), Unknown),
        ("OsStr" | "OsString", m) => match m {
            "to_str" => opt(str_()),
            "to_string_lossy" => str_(),
            "to_os_string" | "to_owned" => ext("std::ffi::OsString"),
            "len" => usize_(),
            "is_empty" => bool_(),
            "as_os_str" => ext("std::ffi::OsStr"),
            _ => return None,
        },
        ("DirEntry", "path") => ext("std::path::PathBuf"),
        ("DirEntry", "file_name") => ext("std::ffi::OsString"),
        ("DirEntry", "metadata") => res(ext("std::fs::Metadata"), Unknown),
        ("DirEntry", "file_type") => res(ext("std::fs::FileType"), Unknown),
        ("Metadata", "file_type") => ext("std::fs::FileType"),
        ("Metadata", "modified" | "accessed" | "created") => {
            res(ext("std::time::SystemTime"), Unknown)
        }
        ("Metadata" | "FileType", "is_file" | "is_dir" | "is_symlink") => bool_(),
        ("Metadata", "len") => ext("u64"),
        ("Metadata", "permissions") => ext("std::fs::Permissions"),
        ("SystemTime", "duration_since" | "elapsed") => res(ext("std::time::Duration"), Unknown),
        ("Instant", "elapsed" | "duration_since") => ext("std::time::Duration"),
        ("Duration", "as_secs" | "as_millis" | "as_micros" | "as_nanos") => ext("u64"),
        ("Duration", "as_secs_f64" | "as_secs_f32") => ext("f64"),
        ("TempDir", "path") => ext("std::path::Path"),
        ("TempDir", "into_path" | "keep") => ext("std::path::PathBuf"),
        ("TempDir", "close") => res(Ty::Tuple(vec![]), Unknown),
        // serde_json's tree, which every JSON-handling body walks.
        ("Value", m) => match m {
            "get" | "get_mut" | "pointer" | "pointer_mut" => opt(ext("Value")),
            "as_str" => opt(str_()),
            "as_array" | "as_array_mut" => opt(Ty::ext("Vec", vec![ext("Value")])),
            "as_object" | "as_object_mut" => opt(Ty::ext("Map", vec![string(), ext("Value")])),
            "as_u64" => opt(ext("u64")),
            "as_i64" => opt(ext("i64")),
            "as_f64" => opt(ext("f64")),
            "as_bool" => opt(bool_()),
            "is_null" | "is_string" | "is_array" | "is_object" | "is_number" | "is_boolean"
            | "is_u64" | "is_i64" | "is_f64" => bool_(),
            "take" => ext("Value"),
            _ => return None,
        },
        ("Map", m) => match m {
            "get" | "get_mut" | "remove" | "insert" => opt(a(1)),
            "keys" => iter(a(0)),
            "values" | "values_mut" => iter(a(1)),
            "contains_key" | "is_empty" => bool_(),
            "len" => usize_(),
            "entry" | "clear" | "retain" => Unknown,
            _ => return None,
        },
        // A guard derefs to what it guards, so its methods are the payload's.
        ("Mutex", "lock" | "try_lock")
        | ("RwLock", "read" | "write" | "try_read" | "try_write") => res(a(0), Unknown),
        ("Mutex" | "RwLock", "into_inner") => res(a(0), Unknown),
        ("Mutex" | "RwLock" | "RefCell" | "Cell", "get_mut") => a(0),
        ("RefCell", "borrow" | "borrow_mut") => a(0),
        ("RefCell", "try_borrow" | "try_borrow_mut") => res(a(0), Unknown),
        ("RefCell" | "Cell", "into_inner" | "take" | "replace") => a(0),
        ("Cell", "get") => a(0),
        ("Cell" | "RefCell", "set" | "swap") => Unknown,
        ("OnceLock" | "OnceCell", "get") => opt(a(0)),
        ("OnceLock" | "OnceCell" | "LazyLock" | "LazyCell", "get_or_init" | "force") => a(0),
        ("OnceLock" | "OnceCell", "set") => res(Unknown, Unknown),
        _ => return None,
    })
}

/// What a call written as a path into std or a well-known crate returns,
/// by the owner's short name and the function's: `fs::read(…)` is a
/// `Result`, `String::from_utf8(…)` is a `Result`, and by the conventions
/// every type honours, `T::try_from(…)`/`T::from_str(…)` are too. The
/// receiver of the `.unwrap()`, `?` or `.map_err(…)` that follows is then
/// known — the single most common shape left in the ledger otherwise.
/// A path with its turbofish split off: `mpsc::channel::<Job>` is the
/// function `mpsc::channel` applied to `Job`. Only the first type argument
/// is returned — it is the one a channel carries.
fn split_turbofish(path: &str) -> (&str, Option<&str>) {
    let Some((head, args)) = path.split_once("::<") else {
        return (path, None);
    };
    let args = args.strip_suffix('>').unwrap_or(args);
    let first = match args.split_once(',') {
        // Only a top-level comma splits arguments; `HashMap<K, V>` as the
        // first argument carries its own.
        Some((a, _)) if a.matches('<').count() == a.matches('>').count() => a,
        _ => args,
    };
    let first = first.trim();
    (head, (!first.is_empty()).then_some(first))
}

/// The channel constructors, whose whole point is the pair they return.
///
/// `let (tx, rx) = mpsc::channel();` is how every Rust channel starts, and
/// until this table existed neither half had a type: `channel()` is external,
/// so it had no declared return, so the tuple had no element types, so `tx`
/// and `rx` were untyped and every `tx.send(v)` and `rx.recv()` in the tree
/// fell into the unresolved ledger. A whole repository's message passing was
/// invisible for want of one return type.
///
/// Keyed by the owner as written and expanded — `tokio::sync::mpsc::Sender`,
/// not a bare `Sender` — so the node matches what an annotated
/// `let tx: mpsc::Sender<T>` resolves to, instead of minting a second
/// spelling of the same type.
fn std_channel(owner: &str, function: &str, elem: impl Fn() -> Ty) -> Option<Ty> {
    let short = owner.rsplit("::").next().unwrap_or(owner);
    let pair = |s: &str, r: &str, arg: Ty| {
        Ty::Tuple(vec![
            Ty::ext(&format!("{owner}::{s}"), vec![arg.clone()]),
            Ty::ext(&format!("{owner}::{r}"), vec![arg]),
        ])
    };
    Some(match (short, function) {
        // std::sync::mpsc, tokio::sync::mpsc, and the bounded/sync forms.
        ("mpsc", "channel" | "sync_channel") => pair("Sender", "Receiver", elem()),
        ("mpsc", "unbounded_channel") => pair("UnboundedSender", "UnboundedReceiver", elem()),
        // tokio's one-shot, broadcast and watch pairs.
        ("oneshot" | "broadcast" | "watch", "channel") => pair("Sender", "Receiver", elem()),
        // crossbeam-channel and flume: `channel::unbounded()`, `flume::bounded()`.
        ("channel" | "flume" | "crossbeam", "unbounded" | "bounded") => {
            pair("Sender", "Receiver", elem())
        }
        _ => return None,
    })
}

fn std_path_return(owner: &str, function: &str) -> Option<Ty> {
    let res = || Ty::ext("Result", vec![Ty::Unknown, Ty::Unknown]);
    Some(match (owner, function) {
        (_, "try_from" | "from_str") => res(),
        (
            "fs",
            "write" | "create_dir" | "create_dir_all" | "remove_file" | "remove_dir"
            | "remove_dir_all" | "canonicalize" | "rename" | "copy" | "read_link" | "hard_link",
        ) => res(),
        ("fs", "read_to_string") => Ty::ext("Result", vec![Ty::ext("String", vec![]), Ty::Unknown]),
        ("fs", "read_dir") => Ty::ext(
            "Result",
            vec![Ty::ext("std::fs::ReadDir", vec![]), Ty::Unknown],
        ),
        ("fs", "metadata" | "symlink_metadata") => Ty::ext(
            "Result",
            vec![Ty::ext("std::fs::Metadata", vec![]), Ty::Unknown],
        ),
        ("fs", "read") => Ty::ext(
            "Result",
            vec![Ty::ext("Vec", vec![Ty::ext("u8", vec![])]), Ty::Unknown],
        ),
        ("File", "open" | "create" | "create_new") => res(),
        ("env", "var") => Ty::ext("Result", vec![Ty::ext("String", vec![]), Ty::Unknown]),
        ("env", "current_dir" | "current_exe") => Ty::ext(
            "Result",
            vec![Ty::ext("std::path::PathBuf", vec![]), Ty::Unknown],
        ),
        ("env", "var_os") => Ty::ext("Option", vec![Ty::ext("std::ffi::OsString", vec![])]),
        ("env", "temp_dir") => Ty::ext("std::path::PathBuf", vec![]),
        ("str", "from_utf8") => Ty::ext("Result", vec![Ty::ext("str", vec![]), Ty::Unknown]),
        ("String", "from_utf8") => Ty::ext("Result", vec![Ty::ext("String", vec![]), Ty::Unknown]),
        ("String", "from_utf8_lossy") => Ty::ext("str", vec![]),
        ("tempfile", "tempdir" | "tempdir_in") => Ty::ext(
            "Result",
            vec![Ty::ext("tempfile::TempDir", vec![]), Ty::Unknown],
        ),
        ("Value" | "serde_json", "from") if owner == "Value" => Ty::ext("Value", vec![]),
        (
            "serde_json" | "serde_yaml" | "toml" | "rmp_serde" | "postcard" | "bincode",
            "to_string" | "to_string_pretty" | "to_vec" | "to_vec_pretty" | "to_writer"
            | "to_value" | "from_slice" | "from_reader" | "from_value",
        ) => res(),
        _ => return None,
    })
}

/// What std hands a closure passed to `method` on `recv` at argument
/// `idx`, per parameter: `v.iter().map(|x| …)` gets the item, `opt.map(|x|
/// …)` the payload, `res.map_err(|e| …)` the error, `map.retain(|k, v| …)`
/// the pair. Read from the receiver's type arguments, so a `Vec<Node>`
/// types `x` as `Node`.
fn closure_param_types(recv: &Ty, method: &str, idx: usize) -> Option<Vec<Ty>> {
    let as_iter = as_iterator(recv);
    let recv = as_iter.as_ref().unwrap_or(recv);
    let a = |i: usize| recv.arg(i);
    Some(match (recv.short(), method) {
        (
            "Iterator",
            "map" | "filter" | "for_each" | "any" | "all" | "find" | "position" | "filter_map"
            | "flat_map" | "take_while" | "skip_while" | "inspect" | "max_by_key" | "min_by_key"
            | "partition" | "find_map" | "map_while" | "try_for_each" | "rposition",
        ) => vec![a(0)],
        ("Iterator", "fold" | "try_fold" | "scan") if idx == 1 => vec![Ty::Unknown, a(0)],
        ("Iterator", "max_by" | "min_by" | "is_sorted_by") => vec![a(0), a(0)],
        (
            "Option",
            "map" | "and_then" | "filter" | "is_some_and" | "is_none_or" | "inspect" | "take_if",
        ) => {
            vec![a(0)]
        }
        ("Option", "map_or" | "map_or_else") if idx == 1 => vec![a(0)],
        ("Result", "map" | "and_then" | "is_ok_and" | "inspect") => vec![a(0)],
        ("Result", "map_or" | "map_or_else") if idx == 1 => vec![a(0)],
        ("Result", "map_err" | "or_else" | "unwrap_or_else" | "inspect_err" | "is_err_and") => {
            vec![a(1)]
        }
        (
            "Vec" | "VecDeque" | "slice" | "array",
            "retain"
            | "retain_mut"
            | "sort_by_key"
            | "sort_unstable_by_key"
            | "sort_by_cached_key"
            | "dedup_by_key"
            | "binary_search_by"
            | "binary_search_by_key"
            | "partition_point",
        ) => vec![a(0)],
        ("Vec" | "VecDeque" | "slice" | "array", "sort_by" | "sort_unstable_by" | "dedup_by") => {
            vec![a(0), a(0)]
        }
        ("HashMap" | "BTreeMap", "retain") => vec![a(0), a(1)],
        ("HashSet" | "BTreeSet", "retain") => vec![a(0)],
        ("Entry", "and_modify") => vec![a(1)],
        ("Entry", "or_insert_with_key") => vec![a(0)],
        (
            "str" | "String",
            "split" | "rsplit" | "splitn" | "split_terminator" | "trim_matches"
            | "trim_start_matches" | "trim_end_matches" | "find" | "rfind" | "matches"
            | "starts_with" | "ends_with" | "contains" | "strip_prefix" | "strip_suffix"
            | "replace" | "retain",
        ) => vec![Ty::ext("char", vec![])],
        _ => return None,
    })
}

/// What a std trait method hands back, relative to its receiver.
enum TraitRet {
    /// The receiver's own type — `clone`, `max`.
    Same,
    /// A named type, spelled as the prelude spells it.
    Of(&'static str),
    /// Depends on a type argument this parser does not track.
    Unknown,
}

/// Methods every type answers through a std trait — by `derive`, by a
/// blanket impl, or by hand — so a call on a typed receiver that declares no
/// such method is that trait's: `x.clone()` on a `#[derive(Clone)]` struct
/// is `Clone::clone`. Named after the trait, because that is where the
/// method is declared and one node is what "who calls `Clone::clone`" wants.
/// `fmt` is left out: `Debug::fmt` and `Display::fmt` share the name, and a
/// call site does not say which.
fn std_trait(method: &str) -> Option<(&'static str, TraitRet)> {
    use TraitRet::{Of, Same, Unknown};
    Some(match method {
        "clone" | "clone_from" => ("Clone", Same),
        "to_owned" => ("ToOwned", Unknown),
        "to_string" => ("ToString", Of("String")),
        "into" => ("Into", Unknown),
        "try_into" => ("TryInto", Of("Result")),
        "as_ref" => ("AsRef", Unknown),
        "as_mut" => ("AsMut", Unknown),
        "borrow" => ("Borrow", Unknown),
        "borrow_mut" => ("BorrowMut", Unknown),
        "eq" | "ne" => ("PartialEq", Of("bool")),
        "lt" | "le" | "gt" | "ge" => ("PartialOrd", Of("bool")),
        "partial_cmp" => ("PartialOrd", Of("Option")),
        "cmp" => ("Ord", Unknown),
        "max" | "min" | "clamp" => ("Ord", Same),
        "hash" => ("Hash", Unknown),
        _ => return None,
    })
}

/// What a std method returns on `recv`: the inherent table for a std type,
/// then the blanket traits — `to_owned` on a `str` is `String` by the
/// first, `clone` on anything is itself by the second. `Ty::Unknown` when
/// the method is known and its return is not.
fn std_return(recv: &Ty, method: &str) -> Option<Ty> {
    // A declared type's methods are its own; only what std adds to *every*
    // type — the blanket traits — applies to it.
    if let Ty::External(..) | Ty::Tuple(_) = recv
        && let Some(ret) = std_method(recv, method)
    {
        return Some(ret);
    }
    if recv.short() == "str" && method == "to_owned" {
        return Some(Ty::ext("String", vec![]));
    }
    Some(match std_trait(method)?.1 {
        TraitRet::Same => recv.clone(),
        TraitRet::Of(t) => Ty::ext(t, vec![]),
        TraitRet::Unknown => Ty::Unknown,
    })
}

/// A written type that is nobody's declaration, as an external type — or
/// nothing, when the name could not be a type at all.
///
/// A primitive is one; so is any path whose last segment is capitalised the
/// way a type is. `Self` is not: it names this impl's type, resolved where the
/// impl is known. A lone capital (`T`, `E1`) is a generic parameter by every
/// convention, and a generic states no type — the signature's own parameters
/// are already filtered by name, and this catches the ones declared further
/// out. `slice` and `array` are the primitives std documents them as.
fn external_type(written: &str) -> Option<String> {
    const PRIMITIVES: &[&str] = &[
        "str", "bool", "char", "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32",
        "i64", "i128", "isize", "f32", "f64", "slice", "array",
    ];
    let last = written.rsplit("::").next()?;
    if PRIMITIVES.contains(&last) {
        return Some(written.to_string());
    }
    if last == "Self" || !last.starts_with(|c: char| c.is_ascii_uppercase()) {
        return None;
    }
    if last[1..].chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(written.to_string())
}

/// A chain's path-call hops (`fs::read()`) and cast heads (`#Vec<Node>`)
/// expanded through the writing file's imports, the way a call's own path
/// is — every other hop is a name that means the same everywhere.
fn expand_chain(chain: &str, expand: &dyn Fn(&str) -> String) -> String {
    chain
        .split('.')
        .map(|hop| match hop.strip_suffix("()") {
            Some(call) if call.contains('<') => {
                let (method, generics) = split_written(call);
                let generics: Vec<String> =
                    generics.iter().map(|g| expand_written(g, expand)).collect();
                format!("{method}<{}>()", generics.join(", "))
            }
            Some(p) if p.contains("::") => format!("{}()", expand(p)),
            _ => match hop.strip_prefix('#') {
                Some(written) => format!("#{}", expand_written(written, expand)),
                None => hop.to_string(),
            },
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// A local's hint with its written paths expanded — see [`expand_written`]
/// and [`expand_chain`].
fn expand_hint(hint: &LocalHint, expand: &dyn Fn(&str) -> String) -> LocalHint {
    match hint {
        LocalHint::Bound { chain, steps } => LocalHint::Bound {
            chain: expand_chain(chain, expand),
            steps: steps.clone(),
        },
        LocalHint::Typed(t) => LocalHint::Typed(expand_written(t, expand)),
        known @ LocalHint::Known(_) => known.clone(),
    }
}

/// Closure parameters with their annotations expanded.
fn expand_params(params: &[ClosureParam], expand: &dyn Fn(&str) -> String) -> Vec<ClosureParam> {
    params
        .iter()
        .map(|p| ClosureParam {
            written: p.written.as_deref().map(|w| expand_written(w, expand)),
            ..p.clone()
        })
        .collect()
}

impl Typing<'_> {
    /// The declared key a written type — or a trait, since `impl Tr` and
    /// `T: Tr` are typed by their trait — resolves to, or nothing.
    fn type_key(&self, written: &str, module: &str) -> Option<String> {
        if self.declared.contains(written) {
            return Some(written.to_string());
        }
        if written.contains("::") {
            return resolve_relative(written, module, self.aliases, self.declared);
        }
        self.types
            .get(written)
            .or_else(|| self.traits.get(written))
            .and_then(|cands| pick_in_scope(module, cands, self.scopes))
            .cloned()
    }

    /// A written type as a [`Ty`]: the declaration it resolves to — through
    /// a `type` alias to what that names — else the external type it names,
    /// arguments resolved the same way. `Some(Ty::Unknown)` for a `_` or a
    /// generic; `None` when the base is no type at all.
    fn written_type(&self, written: &str, module: &str) -> Option<Ty> {
        self.written_type_within(written, module, 4, &BTreeMap::new())
    }

    /// `subst` fills an alias's parameters from its use site: reading the
    /// target of `type Result<T> = std::result::Result<T, Error>` for a
    /// written `Result<Txn>` binds `T` to `Txn`.
    fn written_type_within(
        &self,
        written: &str,
        module: &str,
        depth: usize,
        subst: &BTreeMap<String, Ty>,
    ) -> Option<Ty> {
        let (base, args) = split_written(written);
        if args.is_empty()
            && let Some(bound) = subst.get(base)
        {
            return Some(bound.clone());
        }
        let args: Vec<Ty> = args
            .iter()
            .map(|a| {
                self.written_type_within(a, module, depth, subst)
                    .unwrap_or(Ty::Unknown)
            })
            .collect();
        if base == "(" {
            return Some(Ty::Tuple(args));
        }
        if base == "_" || base.is_empty() {
            return Some(Ty::Unknown);
        }
        if let Some(key) = self.type_key(base, module) {
            // `type Properties = HashMap<String, PropDesc>;` — a name for
            // its target, whose methods are the target's. Bounded: an alias
            // of an alias is fine, a cycle is not.
            if depth > 0
                && let Some((params, target, tmodule)) = self.alias_map.get(&key)
            {
                let bound: BTreeMap<String, Ty> = params
                    .iter()
                    .cloned()
                    .zip(args.iter().cloned().chain(std::iter::repeat(Ty::Unknown)))
                    .collect();
                if let Some(ty) = self.written_type_within(target, tmodule, depth - 1, &bound) {
                    return Some(ty);
                }
            }
            return Some(Ty::Declared(key, args));
        }
        if let Some(path) = external_type(base) {
            return Some(Ty::External(path, args));
        }
        // A generic parameter or a lowercase name that is no primitive.
        (base.len() <= 2
            || base
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()))
        .then_some(Ty::Unknown)
    }

    /// A callable's declared return, resolved in the callable's own module.
    fn returned_type(&self, callable: &str) -> Option<Ty> {
        let written = self.returns_map.get(callable)?;
        let module = self.scopes.get(callable).map(String::as_str).unwrap_or("");
        self.written_type(written, module)
    }

    /// The declared type of a field, resolved — one hop of a dotted
    /// receiver (`o.inner`); position `n` of a tuple (`pair.0`). A field of
    /// an external type is unknown: we do not read that code.
    fn field_type(&self, ty: &Ty, field: &str) -> Option<Ty> {
        match ty {
            Ty::Tuple(_) => field.parse::<usize>().ok().map(|i| ty.arg(i)),
            Ty::Declared(key, _) => {
                let (written, module) = self.fields_map.get(&(key.clone(), field.to_string()))?;
                self.written_type(written, module)
            }
            _ => None,
        }
    }

    /// What a callable named by path returns, the callable resolved from the
    /// caller's module: a declaration's stated return, or — for a path into
    /// std or a dependency — the type a constructor by convention returns:
    /// `Vec::new()`, `String::from(…)`, `HashMap::with_capacity(…)` are that
    /// type, and beyond that only what [`std_path_return`] knows.
    fn call_type(&self, callee: &str, module: &str) -> Option<Ty> {
        const CONSTRUCTORS: &[&str] = &[
            "new",
            "default",
            "with_capacity",
            "from",
            "now",
            "from_iter",
        ];
        let declared = if self.declared.contains(callee) {
            Some(callee.to_string())
        } else if callee.contains("::") {
            resolve_relative(callee, module, self.aliases, self.declared)
        } else {
            self.fns
                .get(callee)
                .and_then(|cands| pick_in_scope(module, cands, self.scopes))
                .cloned()
        };
        if let Some(fk) = declared {
            return self.returned_type(&fk);
        }
        // `channel::<Item>()` is the function `channel` applied to `Item`;
        // split at the last `::` with the turbofish still attached and the
        // "function" comes out as `<Item>`, matching nothing.
        let (path, targ) = split_turbofish(callee);
        let (owner, function) = path.rsplit_once("::")?;
        if CONSTRUCTORS.contains(&function) {
            // Through an alias too: `Properties::new()` is a `HashMap`.
            return self.written_type(owner, module).and_then(Ty::known);
        }
        if let Some(ty) = std_channel(owner, function, || {
            targ.and_then(|t| self.written_type(t, module))
                .unwrap_or(Ty::Unknown)
        }) {
            return Some(ty);
        }
        let short = owner.rsplit("::").next().unwrap_or(owner);
        std_path_return(short, function)
    }

    /// What a method on `recv` returns: the declared method's stated return
    /// when the type is declared here and has one, else what std fixes for
    /// the name. Nothing when the return exists but has no type here.
    fn method_return(&self, recv: &Ty, method: &str) -> Option<Ty> {
        if let Ty::Declared(k, _) = recv
            && let Some((mk, _)) = self.method_target(k, method)
        {
            return self.returned_type(&mk);
        }
        std_return(recv, method).and_then(Ty::known)
    }

    /// The type a chain — see [`chain_of`] — ends on, hop by hop from its
    /// head. A hop typing cannot read ends the chain with nothing: a partial
    /// answer would type the wrong receiver.
    fn chain_type(&self, caller: &str, chain: &str, depth: usize) -> Option<Ty> {
        self.chain_type_why(caller, chain, depth).ok()
    }

    /// [`Self::chain_type`], with the hop that stopped it when it stops:
    /// what the ledger edge says, so a blind spot names its cause.
    fn chain_type_why(&self, caller: &str, chain: &str, depth: usize) -> Result<Ty, String> {
        // Spends the budget rather than assuming one is left: a closure body
        // that chains back to its own hop recurses without this, and `depth - 1`
        // below wraps at zero, which a release build turns into no bound at all.
        if depth == 0 {
            return Err(format!("`{chain}` is typed through too many hops"));
        }
        let module = self.scopes.get(caller).map(String::as_str).unwrap_or("");
        let mut hops = chain.split('.');
        let head = hops.next().ok_or_else(|| "empty receiver".to_string())?;
        let mut ty = if head == "self" {
            let owner = owner_of(caller).ok_or_else(|| "`self` outside an impl".to_string())?;
            if self.declared.contains(&owner) {
                Ty::Declared(owner, vec![])
            } else {
                Ty::External(owner, vec![])
            }
        } else if let Some(written) = head.strip_prefix('#') {
            self.written_type(written, module)
                .and_then(Ty::known)
                .ok_or_else(|| format!("`{written}` names no type here"))?
        } else if let Some(callee) = head.strip_suffix("()") {
            self.call_type(callee, module)
                .and_then(Ty::known)
                .ok_or_else(|| format!("`{callee}()` returns nothing this parser has a type for"))?
        } else {
            match self.local_type_why(caller, head, depth) {
                Ok(ty) => ty,
                // Not a local: a constant or static this tree declares,
                // named the way a bare call is resolved — nearest scope.
                Err(why) => self
                    .consts_by_name
                    .get(head)
                    .and_then(|cands| pick_in_scope(module, cands, self.scopes))
                    .and_then(|key| self.const_types.get(key))
                    .and_then(|(written, cmodule)| self.written_type(written, cmodule))
                    .and_then(Ty::known)
                    .ok_or(why)?,
            }
        };
        let mut prefix = head.to_string();
        for hop in hops {
            prefix.push('.');
            prefix.push_str(hop);
            ty = if let Some(call) = hop.strip_suffix("()") {
                let (method, generics) = split_written(call);
                // `collect::<Vec<_>>()`: the turbofish says what comes out.
                let by_turbofish = match (method, generics.first()) {
                    ("collect" | "sum" | "product" | "into" | "from_iter", Some(g)) => {
                        self.written_type(g, module)
                    }
                    ("parse" | "try_into", Some(g)) => self
                        .written_type(g, module)
                        .map(|t| Ty::ext("Result", vec![t, Ty::Unknown])),
                    _ => None,
                };
                // `map(|x| x.key.clone())`: the closure's body says what the
                // hop yields, where the table can only say "something".
                let by_body = || {
                    let body = self
                        .closure_bodies
                        .get(&(caller.to_string(), prefix.clone()))?;
                    let out = self.chain_type_why(caller, body, depth - 1).ok()?;
                    let wrap = |t: Ty| Ty::ext(ty.short(), vec![t]);
                    Some(match (ty.short(), method) {
                        ("Iterator", "map") => wrap(out),
                        ("Iterator", "filter_map" | "map_while") => wrap(out.arg(0)),
                        ("Iterator", "flat_map") => wrap(item_of(&out)?),
                        ("Option", "map") => wrap(out),
                        ("Option" | "Result", "and_then" | "or_else") => out,
                        ("Result", "map") => Ty::ext("Result", vec![out, ty.arg(1)]),
                        ("Result", "map_err") => Ty::ext("Result", vec![ty.arg(0), out]),
                        _ => return None,
                    })
                };
                // The body first where there is one: the table can only say
                // "an iterator of something" for a `map`.
                by_turbofish
                    .and_then(Ty::known)
                    .or_else(by_body)
                    .or_else(|| self.method_return(&ty, method))
                    .ok_or_else(|| {
                        format!(
                            "`{method}()` on `{}` returns nothing this parser has a type for",
                            ty.display()
                        )
                    })?
            } else if hop == "?" {
                match ty.short() {
                    "Option" | "Result" => ty.arg(0).known().ok_or_else(|| {
                        format!(
                            "`?` peels `{}`, whose payload is not known here",
                            ty.display()
                        )
                    })?,
                    _ => {
                        return Err(format!(
                            "`?` on `{}`, which is neither Option nor Result",
                            ty.display()
                        ));
                    }
                }
            } else if hop == "[]" {
                index_of(&ty).and_then(Ty::known).ok_or_else(|| {
                    format!(
                        "indexing `{}` yields nothing this parser has a type for",
                        ty.display()
                    )
                })?
            } else {
                self.field_type(&ty, hop)
                    .and_then(Ty::known)
                    .ok_or_else(|| {
                        format!("field `{hop}` of `{}` has no declared type", ty.display())
                    })?
            };
        }
        Ok(ty)
    }

    /// The fields of the enum variant a pattern names — `Statement::Write`
    /// resolved from the caller's module, or a bare `Write` looked up on the
    /// scrutinee's own type — with the module their types are written in.
    fn variant_fields(
        &self,
        scrutinee: &Ty,
        path: &str,
        module: &str,
    ) -> Option<(VariantFields, String)> {
        let (enum_written, variant) = path.rsplit_once("::").unwrap_or(("", path));
        let enum_key = if enum_written.is_empty() {
            match scrutinee {
                Ty::Declared(key, _) => key.clone(),
                _ => return None,
            }
        } else {
            match self.written_type(enum_written, module)? {
                Ty::Declared(key, _) => key,
                _ => return None,
            }
        };
        self.variant_map
            .get(&(enum_key, variant.to_string()))
            .cloned()
    }

    /// The method a type answers `name` with: its own inherent method, the
    /// impl of a trait method (`<T as Tr>::m` — a key no call site can
    /// spell), or a default method on a trait it declares it implements.
    fn method_target(&self, ty: &str, name: &str) -> Option<(String, &'static str)> {
        let inherent = format!("{ty}::{name}");
        if self.declared.contains(&inherent) {
            return Some((inherent, "receiver"));
        }
        if let Some(mk) = self
            .trait_impl_methods
            .get(&(ty.to_string(), name.to_string()))
        {
            return Some((mk.clone(), "receiver"));
        }
        for tr in self.impls_of.get(ty).into_iter().flatten() {
            let default = format!("{tr}::{name}");
            if self.declared.contains(&default) {
                return Some((default, "trait"));
            }
        }
        None
    }

    /// The type of a body's local, through its annotation or its binding,
    /// chaining through other locals up to `depth` hops — or why not.
    fn local_type_why(&self, caller: &str, ident: &str, depth: usize) -> Result<Ty, String> {
        if depth == 0 {
            return Err(format!("`{ident}` is typed through too many other locals"));
        }
        let module = self.scopes.get(caller).map(String::as_str).unwrap_or("");
        let hint = self
            .local_inits
            .get(&(caller.to_string(), ident.to_string()))
            .ok_or_else(|| {
                format!("`{ident}` is bound by nothing this parser reads a type from")
            })?;
        match hint {
            LocalHint::Typed(t) => self
                .written_type(t, module)
                .and_then(Ty::known)
                .ok_or_else(|| format!("`{ident}` is annotated `{t}`, which names no type here")),
            LocalHint::Known(t) => Ok(t.clone()),
            LocalHint::Bound { chain, steps } => {
                let mut ty = self
                    .chain_type_why(caller, chain, depth - 1)
                    .map_err(|why| format!("`{ident}` is bound to `{chain}`: {why}"))?;
                // The fields of the variant the last `Payload` step reached,
                // for the `Index`/`Field` step that follows it.
                let mut variant: Option<(VariantFields, String)> = None;
                for step in steps {
                    let fields = variant.take();
                    ty = match step {
                        Step::Item => item_of(&ty).and_then(Ty::known).ok_or_else(|| {
                            format!("`{ident}` iterates `{chain}`, a `{}` this parser takes no item from", ty.display())
                        })?,
                        Step::Payload(path) => match (ty.short(), path.as_str()) {
                            ("Option", "Some") | ("Result", "Ok") => ty.arg(0).known(),
                            ("Result", "Err") => ty.arg(1).known(),
                            _ => match self.variant_fields(&ty, path, module) {
                                // A declared variant: the value stays the
                                // enum, its fields wait for the next step.
                                Some(found) => {
                                    variant = Some(found);
                                    Some(ty.clone())
                                }
                                // A struct's own name, or a variant nothing
                                // here declares: the value as it is.
                                None => Some(ty.clone()),
                            },
                        }
                        .ok_or_else(|| {
                            format!("`{ident}` is the payload of `{chain}`, a `{}` whose payload is not known here", ty.display())
                        })?,
                        Step::Index(i) => match (&fields, &ty) {
                            (Some((fields, vmodule)), _) => fields
                                .get(*i)
                                .and_then(|(_, w)| self.written_type(w, vmodule))
                                .and_then(Ty::known)
                                .ok_or_else(|| {
                                    format!("`{ident}` is field {i} of a variant of `{chain}`, with no type this parser reads")
                                })?,
                            (None, Ty::Tuple(_)) => ty.arg(*i).known().ok_or_else(|| {
                                format!("`{ident}` is position {i} of `{chain}`, a `{}`", ty.display())
                            })?,
                            // A tuple struct's field, by position.
                            (None, Ty::Declared(..)) => self
                                .field_type(&ty, &i.to_string())
                                .and_then(Ty::known)
                                .ok_or_else(|| {
                                    format!("`{ident}` is field {i} of `{chain}`, a `{}`, with no declared type", ty.display())
                                })?,
                            _ => {
                                return Err(format!(
                                    "`{ident}` is position {i} of `{chain}`, a `{}` that is no tuple",
                                    ty.display()
                                ));
                            }
                        },
                        Step::Field(f) => match &fields {
                            Some((fields, vmodule)) => fields
                                .iter()
                                .find(|(n, _)| n == f)
                                .and_then(|(_, w)| self.written_type(w, vmodule))
                                .and_then(Ty::known)
                                .ok_or_else(|| {
                                    format!("`{ident}` is field `{f}` of a variant of `{chain}`, with no type this parser reads")
                                })?,
                            None => self.field_type(&ty, f).and_then(Ty::known).ok_or_else(|| {
                                format!("`{ident}` is field `{f}` of `{chain}`, a `{}`, with no declared type", ty.display())
                            })?,
                        },
                    };
                }
                Ok(ty)
            }
        }
    }
}

/// Choose among same-named functions by locality.
///
/// A bare name is resolved against the caller's own module first, then each
/// enclosing one, taking the innermost scope that holds exactly one candidate.
/// That is roughly what a reader does, and it is what turns the hundreds of
/// `new`, `from` and `len` in a workspace from ambiguous into edges.
///
/// A name still ambiguous in the narrowest scope that has it is left
/// unresolved. Choosing between two equally close candidates needs import and
/// generic resolution — a compiler's job — and a guessed edge is worse than a
/// missing one, because nothing downstream can tell it apart from a known fact.
fn pick<'a>(
    caller: &str,
    cands: &'a [String],
    scopes: &BTreeMap<String, String>,
) -> Option<&'a String> {
    if let [one] = cands {
        return Some(one);
    }
    pick_in_scope(scopes.get(caller)?, cands, scopes)
}

/// The same narrowing, starting from a module rather than from a caller.
fn pick_in_scope<'a>(
    from: &str,
    cands: &'a [String],
    scopes: &BTreeMap<String, String>,
) -> Option<&'a String> {
    if let [one] = cands {
        return Some(one);
    }
    let mut scope = from;
    loop {
        let within: Vec<&String> = cands
            .iter()
            .filter(|c| {
                scopes
                    .get(*c)
                    .is_some_and(|s| s == scope || s.starts_with(&format!("{scope}::")))
            })
            .collect();
        match within.as_slice() {
            [one] => return Some(one),
            [] => scope = scope.rsplit_once("::")?.0,
            _ => return None,
        }
    }
}

// ---- module paths ---------------------------------------------------------

/// Resolve the crate name for every `src/` root in this batch, once each.
///
/// The declared `[package] name` is preferred over the directory name: cargo
/// convention makes them agree, but the manifest is what decides, and a crate
/// whose directory disagrees would otherwise get keys nobody could match.
fn crate_names(files: &[(String, String)], host: &dyn Files) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for (path, _) in files {
        let Some(dir) = crate_dir(path) else { continue };
        if out.contains_key(&dir) {
            continue;
        }
        let manifest = if dir.is_empty() {
            "Cargo.toml".to_string()
        } else {
            format!("{dir}/Cargo.toml")
        };
        let name = host
            .read(&manifest)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .and_then(|t| package_name(&t))
            .or_else(|| dir.rsplit('/').next().map(str::to_string))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "crate".into());
        out.insert(dir, name.replace('-', "_"));
    }
    out
}

/// The cargo target directories a `.rs` file can sit under.
const TARGET_ROOTS: &[&str] = &["src/", "benches/", "tests/", "examples/"];

/// Split a path into its package directory, the target root, and the rest.
///
/// The **leftmost** root wins, not the rightmost: `crates/foo/src/tests/mod.rs`
/// is a module called `tests` inside the library, not an integration test, and
/// matching from the right would call it one.
fn target_root(path: &str) -> Option<(String, &'static str, &str)> {
    let mut best: Option<(usize, &'static str)> = None;
    for root in TARGET_ROOTS {
        let mut from = 0;
        while let Some(rel) = path[from..].find(root) {
            let at = from + rel;
            // Only at a segment boundary, so `mysrc/` is not a target root.
            if at == 0 || path.as_bytes()[at - 1] == b'/' {
                if best.is_none_or(|(b, _)| at < b) {
                    best = Some((at, root));
                }
                break;
            }
            from = at + 1;
        }
    }
    let (at, root) = best?;
    Some((
        path[..at].trim_end_matches('/').to_string(),
        root,
        &path[at + root.len()..],
    ))
}

/// The directory holding a file's target root, or `None` when it has none.
fn crate_dir(path: &str) -> Option<String> {
    target_root(path).map(|(dir, _, _)| dir)
}

/// The path as an editor at the crate root would open it.
///
/// A digest rooted at a crate's `src/` hands paths like `compute/cache.rs`;
/// module resolution already treats those as crate source (that is what the
/// fallback crate name means), and cargo's convention puts crate source under
/// `src/` — so the recorded path says so too. A path that carries its own
/// target root (`crates/foo/src/…` from a workspace digest) is already
/// rooted, and stays as written.
fn source_path(path: &str) -> String {
    if target_root(path).is_some() {
        path.to_string()
    } else {
        format!("src/{path}")
    }
}

/// `name = "..."` from a manifest's `[package]` section.
///
/// A line scan rather than a TOML parse: this crate has no TOML dependency and
/// the alternative is adding one to read a single field.
fn package_name(manifest: &str) -> Option<String> {
    let mut in_package = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if in_package
            && let Some(rest) = line.strip_prefix("name")
            && let Some(v) = rest.trim_start().strip_prefix('=')
        {
            return Some(v.trim().trim_matches('"').to_string());
        }
    }
    None
}

/// The module path a file defines: `crates/foo/src/a/b.rs` → `foo::a::b`.
///
/// `lib.rs`, `main.rs` and `mod.rs` name their parent rather than themselves,
/// which is the whole point of those filenames.
fn module_path(path: &str, crates: &BTreeMap<String, String>, fallback: &str) -> String {
    let split = target_root(path);
    let mut out = split
        .as_ref()
        .and_then(|(dir, _, _)| crates.get(dir).cloned())
        .unwrap_or_else(|| fallback.to_string());

    // A lone file under no target root — its own stem is the best name there is.
    let (root, rest) = split.map_or(("", path), |(_, r, rest)| (r, rest));
    let rest = rest.strip_suffix(".rs").unwrap_or(rest);

    let mut segs: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    if matches!(segs.last(), Some(&"lib") | Some(&"main") | Some(&"mod")) {
        segs.pop();
    }

    // A bench, test or example is its own crate root to rustc, so nothing in it
    // has a path from the library at all. Keyed under the package and the
    // target directory instead: not a path rustc would accept, but it keeps two
    // packages' `benches/graph.rs` apart and puts them where a reader looks.
    if !root.is_empty() && root != "src/" {
        out.push_str("::");
        out.push_str(root.trim_end_matches('/'));
    }

    for s in segs {
        out.push_str("::");
        out.push_str(s);
    }
    out
}

// ---- small helpers --------------------------------------------------------

/// A named item whose declaration line says nothing the key does not.
///
/// No `signature`: for a struct, enum, union or trait it would be the bare
/// name, which the key already ends with — a property repeating the identity
/// of the node it sits on. What is worth recording about these is their
/// *contents* (an enum's `variants`) or their promises (`non_exhaustive`), and
/// those are attached by the caller.
fn simple(
    f: &mut FileFacts,
    parent: &str,
    ident: &syn::Ident,
    label: &str,
    attrs: &[syn::Attribute],
    vis: &syn::Visibility,
) {
    let key = format!("{parent}::{ident}");
    node(
        f,
        parent,
        key,
        label,
        attrs,
        vis,
        String::new(),
        line_of(ident),
    );
}

/// Attributes that say nothing about structure, and stay out.
///
/// `derive` leaves by the other door (IMPLEMENTS); `doc` and `non_exhaustive`
/// are already properties; the `#[test]` family is `test_flag`; the lint and
/// codegen attributes are instructions to the compiler about this item, not
/// facts about the program's shape. `cfg` is on everything and the parser's
/// policy on it is stated elsewhere.
const ATTR_NOISE: &[&str] = &[
    "doc",
    "derive",
    "non_exhaustive",
    "test",
    "bench",
    "rstest",
    "allow",
    "warn",
    "deny",
    "expect",
    "forbid",
    "inline",
    "must_use",
    "repr",
    "cfg",
    "cfg_attr",
    "automatically_derived",
];

/// The traits a `#[derive(...)]` list names, as written.
fn derived_traits(attrs: &[syn::Attribute]) -> Vec<String> {
    let mut out = Vec::new();
    for attr in attrs.iter().filter(|a| a.path().is_ident("derive")) {
        let _ = attr.parse_nested_meta(|meta| {
            out.push(path_of(&meta.path));
            // A derive helper takes no arguments here; consuming any that
            // appear keeps the parse from failing the whole list.
            Ok(())
        });
    }
    out
}

/// The attributes worth recording, as `(path, the whole attribute as written)`.
fn annotations_of(attrs: &[syn::Attribute]) -> Vec<(String, String)> {
    attrs
        .iter()
        .filter(|a| {
            let last = a.path().segments.last().map(|s| s.ident.to_string());
            !last.is_some_and(|l| ATTR_NOISE.contains(&l.as_str()))
        })
        .map(|a| {
            let path = path_of(a.path());
            // As written, minus the `#[` `]`: what the source says is what a
            // reader needs, and re-printing tokens is the only way to get it
            // from syn.
            let text = tidy(&a.meta.to_token_stream().to_string());
            (path, text)
        })
        .collect()
}

/// Record what this item's attributes state: the traits a derive implements,
/// and every other attribute as an annotation.
fn collect_attrs(f: &mut FileFacts, key: &str, attrs: &[syn::Attribute], line: u64) {
    for trait_written in derived_traits(attrs) {
        f.derives.push((key.to_string(), trait_written, line));
    }
    for (path, text) in annotations_of(attrs) {
        f.annotations.push((key.to_string(), path, text, line));
    }
}

#[allow(clippy::too_many_arguments)]
fn node(
    f: &mut FileFacts,
    parent: &str,
    key: String,
    label: &str,
    attrs: &[syn::Attribute],
    vis: &syn::Visibility,
    signature: String,
    line: u64,
) {
    let mut props = props([
        ("signature", signature),
        ("doc_comment", docs_of(attrs)),
        ("visibility", visibility(vis)),
    ]);
    props.insert("line".into(), Value::from(line));
    collect_attrs(f, &key, attrs, line);
    f.nodes.push(Node {
        key: key.clone(),
        label: label.into(),
        extra_labels: Vec::new(),
        props,
    });
    f.scopes.insert(key.clone(), parent.to_string());
    f.edges.push(edge_at(parent, &key, "CONTAINS", line));
}

/// A node standing in for something this tree does not declare.
///
/// Two labels rather than a property: `["Trait", "External"]` says both what it
/// is and that it is not ours, and both are things a reader asks for by label —
/// `MATCH (t:Trait)` should find `Display`, and `MATCH (n:External)` should find
/// everything foreign whatever its kind.
///
/// `External` is the whole answer when only a `use` was seen, since bringing a
/// name into scope says nothing about what kind of thing it is.
///
/// No `path` property: for a stand-in the key *is* the path, and a property
/// repeating the key states the same fact twice.
/// Record an external stand-in, **strengthening** one already recorded.
///
/// The same path is reached from several places, and they do not know the same
/// amount: a `use` says only that a name was brought into scope, while an
/// `impl` block or a call site says what kind of thing it is. Since the import
/// pass runs first, plain insertion would let the site that knows least win and
/// leave `std::collections::HashSet` labelled `External` alone even though an
/// `impl` proved it a type.
///
/// Order-independent by construction: a kind, once known, replaces no-kind and
/// nothing else, so which pass ran first cannot change the result.
/// One construction, as an `INSTANTIATES` edge to the type built — the
/// variant named on the edge rather than as a node, because a variant is not
/// an item: it has no body, no signature and nothing to say beyond which
/// shape of its enum this site chose. The enum node already carries them all
/// under `variants`.
///
/// A type nothing here declares gets a node minted for it, labelled by what
/// the construction proves: a path with a variant is an enum, a path called
/// by its own name is a struct.
/// One construction site's target: the type built, which of its variants
/// when it is an enum's, and whether the type is nobody's declaration here
/// and so needs a node minted for it.
struct Construction {
    target: String,
    variant: Option<String>,
    external: bool,
}

impl Construction {
    /// The type itself — a struct literal, or a tuple struct by its own name.
    fn whole(target: &str, external: bool) -> Self {
        Self {
            target: target.to_string(),
            variant: None,
            external,
        }
    }

    /// One variant of an enum, named as the site wrote it.
    fn variant_of(target: &str, variant: &str, external: bool) -> Self {
        Self {
            target: target.to_string(),
            variant: Some(variant.rsplit("::").next().unwrap_or(variant).to_string()),
            external,
        }
    }
}

fn emit_instantiation(
    edges: &mut Vec<Edge>,
    external: &mut BTreeMap<String, Node>,
    seen: &mut BTreeSet<(String, String, String)>,
    caller: &str,
    built: &Construction,
    line: u64,
) {
    let Construction {
        target,
        variant,
        external: is_external,
    } = built;
    if !seen.insert((
        caller.to_string(),
        target.clone(),
        variant.clone().unwrap_or_default(),
    )) {
        return;
    }
    if *is_external {
        note_external(
            external,
            target,
            Some(if variant.is_some() { "Enum" } else { "Struct" }),
        );
    }
    let mut e = edge_at(caller, target, "INSTANTIATES", line);
    if let Some(variant) = variant {
        e.props.insert(
            "variant".into(),
            json!({
                "$desc": "which of the enum's variants this site built",
                "$value": variant,
            }),
        );
    }
    edges.push(e);
}

fn note_external(external: &mut BTreeMap<String, Node>, key: &str, kind: Option<&str>) {
    match external.get_mut(key) {
        Some(existing) if existing.extra_labels.is_empty() && kind.is_some() => {
            *existing = external_node(key, kind);
        }
        Some(_) => {}
        None => {
            external.insert(key.to_string(), external_node(key, kind));
        }
    }
}

fn external_node(key: &str, kind: Option<&str>) -> Node {
    Node {
        key: key.to_string(),
        label: kind.unwrap_or("External").into(),
        extra_labels: kind
            .map(|_| vec!["External".to_string()])
            .unwrap_or_default(),
        props: Props::new(),
    }
}

/// One field: `pub id: NodeId`, `count: usize`, or `pub 0: i64` for a tuple
/// struct, whose fields are positions rather than names.
///
/// Visibility is included, unlike an enum variant's fields — which of a
/// struct's fields are `pub` is a large part of what the struct *is*, while a
/// variant's fields have no visibility of their own to state.
fn field_text(index: usize, field: &syn::Field) -> String {
    let name = field
        .ident
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| index.to_string());
    let vis = visibility(&field.vis);
    let head = if vis.is_empty() {
        name
    } else {
        format!("{vis} {name}")
    };
    format!("{head}: {}", ty_of(&field.ty))
}

/// Attach the fields of the struct or union just emitted.
///
/// The same argument as an enum's variants: a node saying only `NodeRecord`
/// says almost nothing, while its fields are the shape of it. A unit struct
/// has none, and an empty list property is noise on every read.
fn set_fields<'a>(f: &mut FileFacts, fields: impl Iterator<Item = &'a syn::Field>) {
    let items: Vec<Value> = fields
        .enumerate()
        .map(|(i, field)| Value::String(field_text(i, field)))
        .collect();
    if items.is_empty() {
        return;
    }
    if let Some(n) = f.nodes.last_mut() {
        n.props.insert(
            "fields".into(),
            json!({
                "$desc": "the fields it declares, each with its visibility and type as written",
                "$value": Value::Array(items),
            }),
        );
    }
}

/// One variant, with whatever it carries: `Unit`, `Lit(i64)`,
/// `Prop { name: String }`, `A = 1`.
fn variant_of(v: &syn::Variant) -> String {
    let mut out = v.ident.to_string();
    match &v.fields {
        syn::Fields::Unit => {}
        syn::Fields::Unnamed(fields) => {
            let tys: Vec<String> = fields.unnamed.iter().map(|f| ty_of(&f.ty)).collect();
            out.push_str(&format!("({})", tys.join(", ")));
        }
        syn::Fields::Named(fields) => {
            let named: Vec<String> = fields
                .named
                .iter()
                .map(|f| {
                    let name = f
                        .ident
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_default();
                    format!("{name}: {}", ty_of(&f.ty))
                })
                .collect();
            out.push_str(&format!(" {{ {} }}", named.join(", ")));
        }
    }
    if let Some((_, expr)) = &v.discriminant {
        out.push_str(&format!(" = {}", tidy(&expr.to_token_stream().to_string())));
    }
    out
}

/// Mark the item just emitted `#[non_exhaustive]`, when it is.
///
/// Recorded because it is a promise about the future rather than a detail of
/// the present: it says this type may gain variants or fields, so a downstream
/// `match` must keep a wildcard arm and the type cannot be constructed
/// literally outside its crate. Absent means exhaustive, following the
/// convention `visibility` and `is_async` already use.
const CFG_TEST_DESC: &str =
    "test code: inside a `#[cfg(test)]` module, which is compiled out of the library build";

/// The `#[test]`-family attributes: the bare one, the runtimes' own
/// (`#[tokio::test]`, `#[async_std::test]`, `#[actix_web::test]` — the last
/// segment is what says it), `#[bench]`, and `#[rstest]`. An attribute that
/// merely *contains* the word, `#[test_case(…)]` among them, is left alone:
/// this parser marks what the harness runs, not what looks related.
fn is_test_attr(attr: &syn::Attribute) -> bool {
    let last = attr.path().segments.last().map(|s| s.ident.to_string());
    matches!(last.as_deref(), Some("test" | "bench" | "rstest"))
}

/// Whether a `#[cfg(…)]` names the `test` configuration — `cfg(test)`,
/// `cfg(all(test, …))`, `cfg(any(test, …))`.
///
/// Reads tokens rather than text, which settles both traps for free:
/// `cfg(feature = "test")` carries a string literal and not the identifier,
/// and `cfg(not(test))` means the opposite, so its group is skipped.
fn cfg_says_test(attr: &syn::Attribute) -> bool {
    attr.path().is_ident("cfg")
        && attr
            .meta
            .require_list()
            .is_ok_and(|l| tokens_say_test(l.tokens.clone()))
}

fn tokens_say_test(tokens: proc_macro2::TokenStream) -> bool {
    let mut it = tokens.into_iter().peekable();
    while let Some(tree) = it.next() {
        match tree {
            proc_macro2::TokenTree::Ident(id) => {
                if id == "test" {
                    return true;
                }
                if id == "not" && matches!(it.peek(), Some(proc_macro2::TokenTree::Group(_))) {
                    it.next();
                }
            }
            proc_macro2::TokenTree::Group(g) if tokens_say_test(g.stream()) => return true,
            _ => {}
        }
    }
    false
}

/// Mark a node as test code: what the evidence was, and — in the companion
/// `_` property the renderers keep out of sight and the embedder out of its
/// vectors — how much that evidence is worth. Two properties rather than one
/// because the second is for a reader weighing the first, not for display.
///
/// First writer wins. Every rule here is definitive, so a later pass can only
/// restate what a narrower one already said: a `#[test]` fn inside a
/// `#[cfg(test)]` module keeps `attribute`, the more specific of the two.
fn set_test_flag(props: &mut Props, kind: &str, desc: &str, confidence: &str) {
    if props.contains_key("test_flag") {
        return;
    }
    props.insert("test_flag".into(), json!({ "$desc": desc, "$value": kind }));
    props.insert(
        "_test_flag_confidence".into(),
        Value::String(confidence.into()),
    );
}

fn set_non_exhaustive(f: &mut FileFacts, attrs: &[syn::Attribute]) {
    if !attrs.iter().any(|a| a.path().is_ident("non_exhaustive")) {
        return;
    }
    if let Some(n) = f.nodes.last_mut() {
        n.props.insert(
            "non_exhaustive".into(),
            json!({
                "$desc": "may gain variants or fields, so a match on it needs a wildcard arm",
                "$value": Value::Bool(true),
            }),
        );
    }
}

/// Record the initializer of the item just emitted.
///
/// A `const` is the one kind of item whose value is *knowable from the source
/// alone* — that is what makes it const — so leaving the graph with its type
/// and not its value drops the only fact about it a reader usually wants. The
/// same goes for a `static`, whose initializer is equally fixed.
///
/// Written as the source wrote it: `4`, `256 * 1024`, `Duration::from_secs(2)`.
/// A parser does not evaluate — `256 * 1024` is not folded to `262144` —
/// because folding would need const evaluation, and a wrong number is worse
/// than the expression that produced it.
fn set_value(f: &mut FileFacts, expr: &impl ToTokens) {
    let value = tidy(&expr.to_token_stream().to_string());
    if let Some(n) = f.nodes.last_mut() {
        n.props.insert("value".into(), Value::String(value));
    }
}

/// 1-based, like every editor's gutter. Idents rather than whole items, so
/// a doc comment above a declaration does not move its line.
fn line_of<T: syn::spanned::Spanned>(t: &T) -> u64 {
    t.span().start().line as u64
}

/// Where a declaration stops — the *whole item's* span, unlike [`line_of`],
/// which deliberately takes the ident so documentation above a declaration
/// cannot move where the graph says it starts.
///
/// The pair is "where the name is" to "where the item ends", which is what a
/// reader wants and what lets `snippet` read a symbol instead of guessing a
/// fixed number of lines after its first.
fn end_line_of<T: syn::spanned::Spanned>(t: &T) -> u64 {
    t.span().end().line as u64
}

/// Record where the item just emitted stops. Attached to the node the caller
/// has only just pushed, the way `set_fields` and `set_non_exhaustive` are.
fn set_end<T: syn::spanned::Spanned>(f: &mut FileFacts, item: &T) {
    let end = end_line_of(item);
    if let Some(n) = f.nodes.last_mut() {
        n.props.insert("end_line".into(), Value::from(end));
    }
}

/// An edge carrying the line the relation is written on.
/// Stamp a call edge with how it was resolved — the metadata both surveyed
/// competitors carry and this parser did not: the strategy that won, a
/// coarse confidence band, and the reference as written. `_`-prefixed, so
/// the props are retrieval-only like all provenance.
fn stamp(e: &mut Edge, strategy: &str, band: &str, written: &str) {
    e.props
        .insert("_resolved_by".into(), Value::String(strategy.into()));
    e.props
        .insert("_confidence".into(), Value::String(band.into()));
    e.props.insert("_ref".into(), Value::String(written.into()));
}

fn edge_at(src: &str, dst: &str, ty: &str, line: u64) -> Edge {
    let mut e = edge(src, dst, ty);
    e.props.insert("line".into(), Value::from(line));
    e
}

/// The paths alone, for the `imports` property.
fn join_imports(imports: &[(String, String, u64)]) -> String {
    imports
        .iter()
        .map(|(_, path, _)| path.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn edge(src: &str, dst: &str, ty: &str) -> Edge {
    Edge {
        src: src.to_string(),
        dst: dst.to_string(),
        ty: ty.to_string(),
        props: Props::new(),
    }
}

/// Build a property map, dropping entries that came back empty — an absent
/// property is cheaper and truer than one holding `""`.
fn props<const N: usize>(pairs: [(&str, String); N]) -> Props {
    let mut out = Props::new();
    for (k, v) in pairs {
        if !v.is_empty() {
            out.insert(k.to_string(), Value::String(v));
        }
    }
    out
}

/// Attach the item's own source under `_code` — retrieval only. The underscore
/// keeps it out of the embedding text and the schema summary the model reads.
fn add_source(props: &mut Props, item: &impl ToTokens) {
    props.insert(
        "_code".to_string(),
        json!({
            "$desc": "source as written, for retrieval — not indexed or embedded",
            "$value": Value::String(item.to_token_stream().to_string()),
        }),
    );
}

fn visibility(vis: &syn::Visibility) -> String {
    match vis {
        syn::Visibility::Public(_) => "pub".into(),
        syn::Visibility::Restricted(r) => tidy(&r.to_token_stream().to_string()),
        syn::Visibility::Inherited => String::new(),
    }
}

fn sig_of(sig: &syn::Signature) -> String {
    tidy(&sig.to_token_stream().to_string())
}

/// What a function-like item is, and what it carries.
///
/// Returns the **label** alongside the properties, because the two decisions are
/// one: an item with a `self` receiver is a `Method` and an item without one is
/// a `Function`, which is the distinction Rust itself draws. Both are reachable
/// by name and both take part in the call graph; what changes is that "every
/// method on this type" is a label query rather than a scan for a leading
/// `self` inside a rendered signature.
///
/// Three facts are lifted out of that rendered signature and given properties
/// of their own, because a string is not a queryable thing:
///
/// - `returns` — the return type as written, absent when there is none. In Rust
///   `-> ()` and no arrow are the same function, so absence *is* the unit type.
/// - `receiver` — `self`, `&self` or `&mut self`; absent on a plain function.
///   An associated function such as `Database::open` has no receiver and so is
///   a `Function`, which is what a Rust programmer calls it.
/// - `is_async` — present and true only when the item is `async`, following the
///   same convention as `visibility`, where absent means private.
///
/// The definition's `line` is stamped here too — one place, so free
/// functions, trait items and impl methods cannot drift.
fn fn_facts(
    sig: &syn::Signature,
    attrs: &[syn::Attribute],
    vis: Option<&syn::Visibility>,
    body: Option<&syn::Block>,
) -> (&'static str, Props) {
    let receiver = match sig.inputs.first() {
        Some(syn::FnArg::Receiver(r)) => {
            let mut s = String::new();
            if r.reference.is_some() {
                s.push('&');
            }
            if r.mutability.is_some() {
                s.push_str("mut ");
            }
            s.push_str("self");
            Some(s)
        }
        _ => None,
    };

    let returns = match &sig.output {
        syn::ReturnType::Default => String::new(),
        syn::ReturnType::Type(_, ty) => ty_of(ty),
    };

    let mut props = props([
        ("signature", sig_of(sig)),
        ("returns", returns),
        ("receiver", receiver.clone().unwrap_or_default()),
        ("doc_comment", docs_of(attrs)),
        ("visibility", vis.map(visibility).unwrap_or_default()),
        ("local_bindings", body.map(bindings_of).unwrap_or_default()),
    ]);
    props.insert("line".into(), Value::from(line_of(&sig.ident)));
    props.insert(
        "end_line".into(),
        Value::from(match body {
            Some(block) => end_line_of(block),
            None => end_line_of(sig),
        }),
    );
    if sig.asyncness.is_some() {
        props.insert("is_async".into(), Value::Bool(true));
    }
    if attrs.iter().any(is_test_attr) {
        set_test_flag(
            &mut props,
            "attribute",
            "test code: a `#[test]`-family attribute, which is the language's own marker for a function the test harness runs",
            "definitive",
        );
    }

    let label = if receiver.is_some() {
        "Method"
    } else {
        "Function"
    };
    (label, props)
}

fn ty_of(ty: &syn::Type) -> String {
    tidy(&ty.to_token_stream().to_string())
}

/// A signature's declared parameter types, as `(name, type as written)`.
///
/// The receiver is not one of them: `&self` names the type this method hangs
/// off, which `HAS_METHOD` already says. A pattern that is not a plain
/// binding (`(a, b): (u8, u8)`) is named by its position, which is all a
/// reader can point at.
fn sig_params(sig: &syn::Signature) -> Vec<(String, String)> {
    sig.inputs
        .iter()
        .enumerate()
        .filter_map(|(i, arg)| {
            let syn::FnArg::Typed(t) = arg else {
                return None;
            };
            let name = match &*t.pat {
                syn::Pat::Ident(p) => p.ident.to_string(),
                _ => i.to_string(),
            };
            Some((name, written_ty(&t.ty)?))
        })
        .collect()
}

/// A signature's return type as receiver typing can use it — see
/// [`written_ty`]: `Result<Txn>` keeps its argument, which is what `?` and
/// `.unwrap()` reach. Nothing when the return is not a path.
fn ret_written(sig: &syn::Signature) -> Option<String> {
    match &sig.output {
        syn::ReturnType::Type(_, ty) => written_ty(ty),
        syn::ReturnType::Default => None,
    }
}

fn path_of(path: &syn::Path) -> String {
    tidy(&path.to_token_stream().to_string()).replace(' ', "")
}

/// The same path with its generic arguments dropped: `From<i64>` → `From`.
///
/// A trait is one thing however many ways a type implements it, so this is what
/// identifies the *node*. The arguments are kept on the edge instead, where
/// they say which implementation without minting a trait per instantiation.
fn base_path(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|s| s.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

/// `to_token_stream` spaces every token; tighten the punctuation a reader (and
/// a model) would otherwise trip over.
fn tidy(text: &str) -> String {
    text.replace(" ,", ",")
        .replace(" :", ":")
        .replace(": :", "::")
        .replace(" <", "<")
        .replace("< ", "<")
        .replace(" >", ">")
        .replace(" (", "(")
        .replace("( ", "(")
        .replace(" )", ")")
        .replace(" ;", ";")
        .replace(" !", "!")
}

/// The doc comment, as one block of text.
fn docs_of(attrs: &[syn::Attribute]) -> String {
    let mut lines = Vec::new();
    for a in attrs {
        if !a.path().is_ident("doc") {
            continue;
        }
        if let syn::Meta::NameValue(nv) = &a.meta
            && let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) = &nv.value
        {
            lines.push(s.value().trim().to_string());
        }
    }
    lines.join("\n").trim().to_string()
}

/// The bare name of a type, for `impl` blocks: `Foo<T>` → `Foo`.
/// The path an `impl` block's Self type names, generic arguments dropped.
///
/// The **whole** path, not its last segment: `impl … for
/// std::collections::HashSet<u64>` is about that type and not about any other
/// `HashSet`, and keeping only the tail both loses what the source said and
/// merges two crates' same-named types into one node. Resolution still matches
/// on the last segment, so a local `impl Database` is unaffected.
fn type_name(ty: &syn::Type) -> Option<String> {
    match ty {
        syn::Type::Path(p) => Some(base_path(&p.path)),
        syn::Type::Reference(r) => type_name(&r.elem),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
