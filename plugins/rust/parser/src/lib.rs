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
//! `.read()` names no path, and the receiver's type is what a parser cannot
//! know — those are counted, never guessed. **Properties** are JSON values in
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
    /// `(function key, declared return shape)` for every free function whose
    /// return names a usable type — what types a `let x = f();` receiver.
    returns: Vec<(String, RetShape)>,
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
    closure_uses: Vec<(String, ClosureCallee, usize, Vec<String>)>,
    /// `(fn key, arg index, declared closure-arg types)` from signatures.
    closure_sigs: Vec<(String, usize, Vec<String>)>,
    /// `(trait key, supertrait base path as written, line)` — `trait E: D`.
    trait_bases: Vec<(String, String, u64)>,
    /// `(caller key, local name, how its type is known)` — the bindings whose
    /// type a parser *can* know, kept for method-call resolution.
    local_hints: Vec<(String, String, LocalHint)>,
    /// `impl` blocks, held whole until every file is known — see [`walk_impl`].
    impls: Vec<ImplBlock>,
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
    imports: Vec<(String, String, u64)>,
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
/// `.read()`, and the receiver's type is exactly what a parser does not know,
/// so those stay unresolved rather than becoming a guess.
#[derive(Clone, Serialize, Deserialize)]
struct Call {
    /// The final segment — what resolution against local items matches on.
    name: String,
    /// The path as written, when the call site wrote one.
    path: Option<String>,
    /// A method call's receiver, when it is a bare name — `txn` in
    /// `txn.remove(…)`, `self` in `self.close()`. A local whose type the
    /// body declared is the one thing that makes such a call resolvable.
    recv: Option<String>,
    /// The call site's line. Not part of identity: the same callee named
    /// twice in one body is one fact, and the first site is the line —
    /// `BTreeSet::insert` keeps the first, and visiting order is source
    /// order.
    line: u64,
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
/// Only the two deterministic sources are kept: an annotation is the type
/// written down, and a plain-call initializer names a function whose declared
/// return type is on record. Anything else — field access, method chains,
/// generics — is a checker's job and stays out.
#[derive(Clone, Serialize, Deserialize)]
enum LocalHint {
    /// `let x = f(…);` — x's type is whatever `f` declares it returns.
    /// `unwrapped` records a `?`/`.unwrap()`/`.expect(…)` at the site, which
    /// is what licenses peeling a `Result<T>`/`Option<T>` return to its `T`.
    Returns { callee: String, unwrapped: bool },
    /// `let x = y.m(…);` — typing x means typing `y` first, then reading
    /// what `m` on y's type declares it returns.
    MethodReturns {
        recv: String,
        method: String,
        unwrapped: bool,
    },
    /// `let x: T = …;` — the annotation, as written.
    Typed(String),
}

/// A declared return type, as far as receiver typing can use it.
#[derive(Clone, Serialize, Deserialize)]
enum RetShape {
    /// `-> T` for a plain path type.
    Plain(String),
    /// `-> Result<T, …>` / `-> Option<T>` — the `T`, reachable only through
    /// an unwrapping call site.
    Wrapped(String),
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
        .map(|(path, line)| (module.to_string(), path.clone(), *line))
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
    out: &mut Vec<(String, u64)>,
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
                out.extend(named.into_iter().map(|(_, path)| (path, line)));
            }
            syn::Item::ExternCrate(e) => out.push((e.ident.to_string(), line_of(&e.ident))),
            _ => {}
        }
    }
    // Sorted by (path, line) then deduplicated by path: one import, its
    // first-written line.
    out.sort();
    out.dedup_by(|a, b| a.0 == b.0);
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
                collect_calls(&key, &func.block, f);
                if let Some(ret) = ret_shape(&func.sig) {
                    f.returns.push((key.clone(), ret));
                }
                for (idx, args) in closure_sig(&func.sig) {
                    f.closure_sigs.push((key.clone(), idx, args));
                }
                for (ident, hint) in local_hints(&func.block)
                    .into_iter()
                    .chain(param_hints(&func.sig))
                {
                    f.local_hints.push((key.clone(), ident, hint));
                }
            }
            syn::Item::Struct(s) => {
                simple(f, parent, &s.ident, "Struct", &s.attrs, &s.vis);
                set_fields(f, s.fields.iter());
                set_non_exhaustive(f, &s.attrs);
                // Plain-path field types, machine-readable: what types
                // `o.field.m()` when the receiver walks a field.
                let key = format!("{parent}::{}", s.ident);
                for field in s.fields.iter() {
                    if let (Some(id), Some(t)) = (&field.ident, plain_type_path(&field.ty)) {
                        f.field_types.push((key.clone(), id.to_string(), t));
                    }
                }
            }
            syn::Item::Enum(e) => {
                simple(f, parent, &e.ident, "Enum", &e.attrs, &e.vis);
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
            }
            syn::Item::Union(u) => {
                simple(f, parent, &u.ident, "Union", &u.attrs, &u.vis);
                // A union is a struct whose fields overlap in memory; the
                // fields are just as much its shape.
                set_fields(f, u.fields.named.iter());
                set_non_exhaustive(f, &u.attrs);
            }
            syn::Item::Const(c) => {
                let key = format!("{parent}::{}", c.ident);
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
                set_value(f, &c.expr);
            }
            syn::Item::Static(s) => {
                let key = format!("{parent}::{}", s.ident);
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
                set_value(f, &s.expr);
            }
            syn::Item::Type(t) => {
                let key = format!("{parent}::{}", t.ident);
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
                    }
                }
            }
            syn::Item::Impl(im) => walk_impl(im, parent, include_source, f),
            // An inline `mod` extends the path, which is exactly why two
            // `fn new` in sibling modules are two different nodes.
            syn::Item::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    let key = format!("{parent}::{}", m.ident);
                    let mut imports = Vec::new();
                    collect_imports(inner, &key, &mut imports, &mut f.reexports);
                    f.imports.extend(
                        imports
                            .iter()
                            .map(|(path, line)| (key.clone(), path.clone(), *line)),
                    );
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
                    ImplMethod {
                        name: m.sig.ident.to_string(),
                        label: label.to_string(),
                        props,
                        calls: body.calls,
                        insts: body.insts,
                        fn_refs: body.fn_refs,
                        closures: body.closures,
                        closure_sigs: closure_sig(&m.sig),
                        ret: ret_shape(&m.sig),
                        locals: local_hints(&m.block)
                            .into_iter()
                            .chain(param_hints(&m.sig))
                            .collect(),
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
    closures: Vec<(ClosureCallee, usize, Vec<String>)>,
    /// Declared closure-arg types per parameter position of this method.
    closure_sigs: Vec<(usize, Vec<String>)>,
    /// Declared return shape when usable; `Self` is resolved to the impl's
    /// type at assemble, where the type's key is known.
    ret: Option<RetShape>,
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

/// A closure argument's parameter names — plain idents only; patterns are
/// a checker's business.
fn closure_params(e: &syn::Expr) -> Option<Vec<String>> {
    let syn::Expr::Closure(c) = e else {
        return None;
    };
    let names = c
        .inputs
        .iter()
        .map(|pat| match pat {
            syn::Pat::Ident(i) => Some(i.ident.to_string()),
            syn::Pat::Type(t) => match &*t.pat {
                syn::Pat::Ident(i) => Some(i.ident.to_string()),
                _ => None,
            },
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    (!names.is_empty()).then_some(names)
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
                return p.inputs.iter().map(plain_type_path).collect();
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
                .map(|a| plain_type_path(&a.ty))
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

/// A method receiver as a dotted name path — `txn`, `o.inner`,
/// `self.graph` — or nothing when the receiver is any other expression.
fn recv_path(e: &syn::Expr) -> Option<String> {
    match e {
        syn::Expr::Path(p) if p.qself.is_none() && p.path.segments.len() == 1 => {
            Some(p.path.segments[0].ident.to_string())
        }
        syn::Expr::Field(f) => {
            let base = recv_path(&f.base)?;
            match &f.member {
                syn::Member::Named(id) => Some(format!("{base}.{id}")),
                syn::Member::Unnamed(_) => None,
            }
        }
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
    /// `x.on_all(|p| …)` — a method call; recv as [`recv_path`] gives it.
    Method { recv: Option<String>, name: String },
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
    closures: Vec<(ClosureCallee, usize, Vec<String>)>,
}

fn call_names(block: &syn::Block) -> BodyFacts {
    struct Calls<'a>(&'a mut BodyFacts);
    impl<'ast> Visit<'ast> for Calls<'_> {
        fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
            if let syn::Expr::Path(p) = &*node.func
                && let Some(last) = p.path.segments.last()
            {
                self.0.calls.insert(Call {
                    name: last.ident.to_string(),
                    path: Some(path_of(&p.path)),
                    recv: None,
                    line: line_of(node),
                });
            }
            for (i, arg) in node.args.iter().enumerate() {
                if let Some(name) = bare_fn_arg(arg) {
                    self.0.fn_refs.insert((name, line_of(node)));
                }
                if let (Some(params), syn::Expr::Path(p)) = (closure_params(arg), &*node.func)
                    && p.qself.is_none()
                {
                    self.0
                        .closures
                        .push((ClosureCallee::Path(path_of(&p.path)), i, params));
                }
            }
            syn::visit::visit_expr_call(self, node);
        }
        fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
            self.0.calls.insert(Call {
                name: node.method.to_string(),
                path: None,
                recv: recv_path(&node.receiver),
                line: line_of(node),
            });
            for (i, arg) in node.args.iter().enumerate() {
                if let Some(name) = bare_fn_arg(arg) {
                    self.0.fn_refs.insert((name, line_of(node)));
                }
                if let Some(params) = closure_params(arg) {
                    self.0.closures.push((
                        ClosureCallee::Method {
                            recv: recv_path(&node.receiver),
                            name: node.method.to_string(),
                        },
                        i,
                        params,
                    ));
                }
            }
            syn::visit::visit_expr_method_call(self, node);
        }
        fn visit_expr_struct(&mut self, node: &'ast syn::ExprStruct) {
            if node.qself.is_none() {
                self.0.insts.insert((path_of(&node.path), line_of(node)));
            }
            syn::visit::visit_expr_struct(self, node);
        }
    }
    let mut out = BodyFacts::default();
    Calls(&mut out).visit_block(block);
    out
}

/// The locals whose type this body states outright.
///
/// First binding wins on a name bound twice: the calls it might type were
/// themselves deduplicated to their first site.
fn local_hints(block: &syn::Block) -> Vec<(String, LocalHint)> {
    struct Hints<'a>(&'a mut Vec<(String, LocalHint)>);
    impl<'ast> Visit<'ast> for Hints<'_> {
        fn visit_local(&mut self, node: &'ast syn::Local) {
            let hinted = match &node.pat {
                syn::Pat::Type(t) => match (&*t.pat, plain_type_path(&t.ty)) {
                    (syn::Pat::Ident(i), Some(ty)) => {
                        Some((i.ident.to_string(), LocalHint::Typed(ty)))
                    }
                    _ => None,
                },
                syn::Pat::Ident(i) => node
                    .init
                    .as_ref()
                    .and_then(|init| init_hint(&init.expr, false))
                    .map(|hint| (i.ident.to_string(), hint)),
                _ => None,
            };
            if let Some((ident, hint)) = hinted
                && !self.0.iter().any(|(n, _)| n == &ident)
            {
                self.0.push((ident, hint));
            }
            syn::visit::visit_local(self, node);
        }
    }
    let mut out = Vec::new();
    Hints(&mut out).visit_block(block);
    out
}

/// What an initializer expression says about the type of the local it binds.
///
/// Only calls whose target is written down count: a plain call names a
/// function whose declared return is on record, a method call defers to the
/// receiver's type. `?`, `.unwrap()` and `.expect(…)` are looked through and
/// remembered — they are what licenses peeling a wrapped return.
fn init_hint(e: &syn::Expr, unwrapped: bool) -> Option<LocalHint> {
    match e {
        syn::Expr::Try(t) => init_hint(&t.expr, true),
        syn::Expr::MethodCall(m) if m.method == "unwrap" || m.method == "expect" => {
            init_hint(&m.receiver, true)
        }
        syn::Expr::Call(c) => match &*c.func {
            syn::Expr::Path(p) => Some(LocalHint::Returns {
                callee: path_of(&p.path),
                unwrapped,
            }),
            _ => None,
        },
        syn::Expr::MethodCall(m) => match &*m.receiver {
            syn::Expr::Path(p) if p.qself.is_none() && p.path.segments.len() == 1 => {
                Some(LocalHint::MethodReturns {
                    recv: p.path.segments[0].ident.to_string(),
                    method: m.method.to_string(),
                    unwrapped,
                })
            }
            _ => None,
        },
        _ => None,
    }
}

/// A signature's parameters as type-known locals: the annotation is written
/// in the signature, which is as declared as typing gets. Body bindings are
/// consumed first, so a shadowing `let` beats its parameter.
fn param_hints(sig: &syn::Signature) -> Vec<(String, LocalHint)> {
    sig.inputs
        .iter()
        .filter_map(|arg| match arg {
            syn::FnArg::Typed(t) => match (&*t.pat, plain_type_path(&t.ty)) {
                (syn::Pat::Ident(i), Some(ty)) => Some((i.ident.to_string(), LocalHint::Typed(ty))),
                _ => None,
            },
            syn::FnArg::Receiver(_) => None,
        })
        .collect()
}

/// A type that *is* a path — `Txn`, `db::Txn`, `&mut Txn<'a>` — or nothing.
///
/// Lifetime arguments are dropped: they never change which type it is. Type
/// arguments disqualify — `Option<Txn>` is an `Option`, and its methods are
/// std's, not `Txn`'s. References are looked through, as method resolution
/// does.
fn plain_type_path(ty: &syn::Type) -> Option<String> {
    fn lifetimes_only(seg: &syn::PathSegment) -> bool {
        match &seg.arguments {
            syn::PathArguments::None => true,
            syn::PathArguments::AngleBracketed(a) => a
                .args
                .iter()
                .all(|g| matches!(g, syn::GenericArgument::Lifetime(_))),
            syn::PathArguments::Parenthesized(_) => false,
        }
    }
    match ty {
        syn::Type::Reference(r) => plain_type_path(&r.elem),
        syn::Type::Path(p) if p.qself.is_none() && p.path.segments.iter().all(lifetimes_only) => {
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
    for f in &files {
        scopes.extend(f.scopes.iter().map(|(k, v)| (k.clone(), v.clone())));
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
    let mut returns_map: BTreeMap<String, RetShape> = BTreeMap::new();
    let mut local_inits: BTreeMap<(String, String), LocalHint> = BTreeMap::new();
    // Field types per (type key, field name), expanded through the writing
    // file's imports — what a dotted receiver walks. The module rides along
    // for scope-narrowed resolution of the written type.
    let mut fields_map: BTreeMap<(String, String), (String, String)> = BTreeMap::new();
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
    let mut pending_closures: Vec<(String, ClosureCallee, usize, Vec<String>)> = Vec::new();

    let aliases = build_aliases(&files, &declared);

    for f in &files {
        let imports = import_index(&f.imports);

        for (key, ret) in &f.returns {
            let expanded = match ret {
                RetShape::Plain(t) => RetShape::Plain(expand_path(t, &imports, &f.module)),
                RetShape::Wrapped(t) => RetShape::Wrapped(expand_path(t, &imports, &f.module)),
            };
            returns_map.insert(key.clone(), expanded);
        }
        for (caller, ident, hint) in &f.local_hints {
            let expanded = match hint {
                LocalHint::Returns { callee, unwrapped } => LocalHint::Returns {
                    callee: expand_path(callee, &imports, &f.module),
                    unwrapped: *unwrapped,
                },
                LocalHint::Typed(t) => LocalHint::Typed(expand_path(t, &imports, &f.module)),
                chained @ LocalHint::MethodReturns { .. } => chained.clone(),
            };
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
                    .map(|a| expand_path(a, &imports, &f.module))
                    .collect(),
            );
        }
        for (caller, callee, idx, params) in &f.closure_uses {
            let callee = match callee {
                ClosureCallee::Path(p) => ClosureCallee::Path(expand_path(p, &imports, &f.module)),
                m @ ClosureCallee::Method { .. } => m.clone(),
            };
            pending_closures.push((caller.clone(), callee, *idx, params.clone()));
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
        for (module, written, line) in &f.imports {
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
                if let Some(ret) = &m.ret {
                    let self_or = |t: &str| {
                        if t == "Self" {
                            self_key.clone()
                        } else {
                            deself(t)
                        }
                    };
                    let expanded = match ret {
                        RetShape::Plain(t) => RetShape::Plain(self_or(t)),
                        RetShape::Wrapped(t) => RetShape::Wrapped(self_or(t)),
                    };
                    returns_map.insert(mkey.clone(), expanded);
                }
                for (ident, hint) in &m.locals {
                    let expanded = match hint {
                        LocalHint::Typed(t) if t == "Self" => LocalHint::Typed(self_key.clone()),
                        LocalHint::Typed(t) => LocalHint::Typed(deself(t)),
                        LocalHint::Returns { callee, unwrapped } => LocalHint::Returns {
                            callee: deself(callee),
                            unwrapped: *unwrapped,
                        },
                        chained @ LocalHint::MethodReturns { .. } => chained.clone(),
                    };
                    local_inits
                        .entry((mkey.clone(), ident.clone()))
                        .or_insert(expanded);
                }
                for (idx, args) in &m.closure_sigs {
                    closure_sig_map.insert(
                        (mkey.clone(), *idx),
                        args.iter().map(|a| deself(a)).collect(),
                    );
                }
                for (callee, idx, params) in &m.closures {
                    let callee = match callee {
                        ClosureCallee::Path(p) => ClosureCallee::Path(deself(p)),
                        other => other.clone(),
                    };
                    pending_closures.push((mkey.clone(), callee, *idx, params.clone()));
                }
                impl_calls.extend(m.calls.iter().map(|c| {
                    let path = c.path.as_deref().map(&deself);
                    (
                        mkey.clone(),
                        f.path.clone(),
                        Call {
                            path,
                            name: c.name.clone(),
                            recv: c.recv.clone(),
                            line: c.line,
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
            pending.push((caller, file.clone(), Call { path, ..c }));
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
        e.props.insert("_reason".into(), Value::String(reason));
        edges.push(e);
    };

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
    let derived: Vec<((String, String), LocalHint)> = {
        let typing = Typing {
            declared: &declared,
            fns: &fns,
            types: &types,
            scopes: &scopes,
            aliases: &aliases,
            returns_map: &returns_map,
            local_inits: &local_inits,
            fields_map: &fields_map,
            impls_of: &impls_of,
            trait_impl_methods: &trait_impl_methods,
        };
        let mut derived = Vec::new();
        for (caller, callee, idx, params) in &pending_closures {
            let module = scopes.get(caller).cloned().unwrap_or_default();
            let fn_key = match callee {
                ClosureCallee::Path(p) => {
                    if declared.contains(p) {
                        Some(p.clone())
                    } else if p.contains("::") {
                        resolve_relative(p, &module, &aliases, &declared)
                    } else {
                        fns.get(p.as_str())
                            .and_then(|cands| pick_in_scope(&module, cands, &scopes))
                            .cloned()
                    }
                }
                ClosureCallee::Method { recv, name } => {
                    let ty = match recv.as_deref() {
                        Some("self") => owner_of(caller),
                        Some(r) => typing.local_type(caller, r, 6),
                        None => None,
                    };
                    ty.and_then(|t| typing.method_target(&t, name).map(|(k, _)| k))
                }
            };
            let Some(fn_key) = fn_key else { continue };
            let Some(args) = closure_sig_map.get(&(fn_key.clone(), *idx)) else {
                continue;
            };
            let fmodule = scopes.get(&fn_key).cloned().unwrap_or_default();
            for (pname, written) in params.iter().zip(args) {
                if let Some(ty) = typing.type_key(written, &fmodule) {
                    derived.push(((caller.clone(), pname.clone()), LocalHint::Typed(ty)));
                }
            }
        }
        derived
    };
    for (key, hint) in derived {
        local_inits.entry(key).or_insert(hint);
    }

    let typing = Typing {
        declared: &declared,
        fns: &fns,
        types: &types,
        scopes: &scopes,
        aliases: &aliases,
        returns_map: &returns_map,
        local_inits: &local_inits,
        fields_map: &fields_map,
        impls_of: &impls_of,
        trait_impl_methods: &trait_impl_methods,
    };
    let typed_target = |caller: &str, call: &Call| -> Option<(String, &'static str)> {
        let recv = call.recv.as_deref()?;
        // A dotted receiver walks declared field types: `o.inner.ping()`
        // types `o`, then reads what `inner` is stated to be.
        let mut segments = recv.split('.');
        let head = segments.next()?;
        let mut ty = if head == "self" {
            owner_of(caller)?
        } else {
            typing.local_type(caller, head, 8)?
        };
        for field in segments {
            ty = typing.field_type(&ty, field)?;
        }
        let (mk, how) = typing.method_target(&ty, &call.name)?;
        Some((mk, if recv == "self" { "self" } else { how }))
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
                out.edges.push(e);
            }
        };

        if let Some(p) = &call.path {
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
        // receiver's type it resolves; otherwise it is counted, never
        // guessed — but shown, as an UnresolvedRef the graph can answer for.
        if let Some((target, strategy)) = typed_target(&caller, &call) {
            emit(&mut out, &mut emitted, &target, strategy, "high");
            continue;
        }
        unresolved += 1;
        unresolved_edge(
            &mut unresolved_nodes,
            &mut out.edges,
            &mut emitted,
            &caller,
            &file,
            &call,
            "method call: receiver type unknown".into(),
        );
    }
    // Struct literals: a `Widget { .. }` uses the type as surely as a call
    // uses a function — resolved the same way, emitted as INSTANTIATES.
    let mut inst_seen: BTreeSet<(String, String)> = BTreeSet::new();
    for (caller, p, line) in pending_insts {
        let module = scopes.get(&caller).cloned().unwrap_or_default();
        let target = if declared.contains(&p) {
            Some(p.clone())
        } else if let Some(hit) = resolve_relative(&p, &module, &aliases, &declared) {
            Some(hit)
        } else if p.contains("::") {
            note_external(&mut external, &p, Some("Struct"));
            Some(p.clone())
        } else {
            types
                .get(&p)
                .and_then(|cands| pick_in_scope(&module, cands, &scopes))
                .cloned()
        };
        if let Some(target) = target
            && inst_seen.insert((caller.clone(), target.clone()))
        {
            out.edges
                .push(edge_at(&caller, &target, "INSTANTIATES", line));
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
             names no path, and the receiver's type is what a parser cannot know"
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
fn import_index(imports: &[(String, String, u64)]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (_, path, _) in imports {
        if let Some(last) = path.rsplit("::").next()
            && last != "*"
        {
            // First writing wins: a later glob or a re-import should not
            // displace an explicit one.
            out.entry(last.to_string()).or_insert_with(|| path.clone());
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
    scopes: &'a BTreeMap<String, String>,
    aliases: &'a BTreeMap<String, String>,
    returns_map: &'a BTreeMap<String, RetShape>,
    local_inits: &'a BTreeMap<(String, String), LocalHint>,
    fields_map: &'a BTreeMap<(String, String), (String, String)>,
    impls_of: &'a BTreeMap<String, Vec<String>>,
    trait_impl_methods: &'a BTreeMap<(String, String), String>,
}

impl Typing<'_> {
    /// The declared key a written type resolves to, or nothing.
    fn type_key(&self, written: &str, module: &str) -> Option<String> {
        if self.declared.contains(written) {
            return Some(written.to_string());
        }
        if written.contains("::") {
            return resolve_relative(written, module, self.aliases, self.declared);
        }
        self.types
            .get(written)
            .and_then(|cands| pick_in_scope(module, cands, self.scopes))
            .cloned()
    }

    /// A callable's declared return, peeled only when the site unwrapped it,
    /// resolved in the callable's own module.
    fn returned_type(&self, callable: &str, unwrapped: bool) -> Option<String> {
        let written = match (self.returns_map.get(callable)?, unwrapped) {
            (RetShape::Plain(t), false) => t,
            (RetShape::Wrapped(t), true) => t,
            // A `Result` nobody unwrapped is a `Result`; an `unwrap()` on a
            // plain type is std's business — neither types the local.
            _ => return None,
        };
        let module = self.scopes.get(callable).map(String::as_str).unwrap_or("");
        self.type_key(written, module)
    }

    /// The declared type of a field, resolved — one hop of a dotted
    /// receiver (`o.inner`).
    fn field_type(&self, ty: &str, field: &str) -> Option<String> {
        let (written, module) = self.fields_map.get(&(ty.to_string(), field.to_string()))?;
        self.type_key(written, module)
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

    /// The resolved type of a body's local, through its annotation or its
    /// initializer, chaining through other locals up to `depth` hops.
    fn local_type(&self, caller: &str, ident: &str, depth: usize) -> Option<String> {
        if depth == 0 {
            return None;
        }
        let module = self.scopes.get(caller).map(String::as_str).unwrap_or("");
        match self
            .local_inits
            .get(&(caller.to_string(), ident.to_string()))?
        {
            LocalHint::Typed(t) => self.type_key(t, module),
            LocalHint::Returns { callee, unwrapped } => {
                let fk = if self.declared.contains(callee) {
                    callee.clone()
                } else if callee.contains("::") {
                    resolve_relative(callee, module, self.aliases, self.declared)?
                } else {
                    self.fns
                        .get(callee.as_str())
                        .and_then(|cands| pick_in_scope(module, cands, self.scopes))?
                        .clone()
                };
                self.returned_type(&fk, *unwrapped)
            }
            LocalHint::MethodReturns {
                recv,
                method,
                unwrapped,
            } => {
                let rty = if recv == "self" {
                    owner_of(caller)?
                } else {
                    self.local_type(caller, recv, depth - 1)?
                };
                let mk = format!("{rty}::{method}");
                self.declared.contains(&mk).then_some(())?;
                self.returned_type(&mk, *unwrapped)
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
fn join_imports(imports: &[(String, u64)]) -> String {
    imports
        .iter()
        .map(|(path, _)| path.as_str())
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
    if sig.asyncness.is_some() {
        props.insert("is_async".into(), Value::Bool(true));
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

/// A signature's return type as receiver typing can use it: a plain path, or
/// the `T` inside `Result<T, …>`/`Option<T>` — which an unwrapping call site
/// reaches. Anything else yields nothing.
fn ret_shape(sig: &syn::Signature) -> Option<RetShape> {
    let syn::ReturnType::Type(_, ty) = &sig.output else {
        return None;
    };
    if let Some(plain) = plain_type_path(ty) {
        return Some(RetShape::Plain(plain));
    }
    // `Result` / `Option` by last-segment ident: `anyhow::Result<T>` and a
    // crate's own `Result<T>` alias both count — what matters is that `?`
    // and `.unwrap()` reach the `T`.
    if let syn::Type::Path(p) = &**ty
        && p.qself.is_none()
        && let Some(last) = p.path.segments.last()
        && (last.ident == "Result" || last.ident == "Option")
        && let syn::PathArguments::AngleBracketed(a) = &last.arguments
        && let Some(syn::GenericArgument::Type(inner)) = a.args.first()
    {
        return plain_type_path(inner).map(RetShape::Wrapped);
    }
    None
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
