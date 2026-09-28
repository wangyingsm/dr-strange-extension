//! The parser's own tests, running natively — the reason it is a plain
//! library under the wasm component rather than one crate with it.
//!
//! Ported from the slice-1 suite in `dr-strange-llm`; the router-level tests
//! (routing order, ignore rules, provenance stamping, the polyglot merge)
//! stayed there, because those belong to the host.

use super::*;

/// A scratch tree that cleans up after itself.
struct Tree(std::path::PathBuf);

impl Tree {
    fn new(name: &str) -> Self {
        // A serial number as well as the name: two tests may reasonably want
        // a tree called the same thing, and the tests run in parallel. Named
        // by process and name alone, the second one to start wiped the first
        // one's files mid-run and `Drop` deleted the directory it was still
        // reading — a failure that looked like the parser losing a module.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nth = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p =
            std::env::temp_dir().join(format!("drsg-parser-{name}-{}-{nth}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }

    fn write(&self, rel: &str, body: &str) -> &Self {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        self
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A plain sorted walker over a directory — what the host's `list`/`read` look
/// like from in here, without the host. Ignore rules are the host's business
/// and are tested with it.
struct TestFiles(std::path::PathBuf);

impl TestFiles {
    fn rooted(p: impl Into<std::path::PathBuf>) -> Self {
        Self(p.into())
    }
}

impl Files for TestFiles {
    fn list(&self, suffix: &str) -> Result<Vec<String>, String> {
        fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let p = entry.unwrap().path();
                if p.is_dir() {
                    walk(&p, root, out);
                } else {
                    out.push(p.strip_prefix(root).unwrap().to_string_lossy().into_owned());
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.0, &self.0, &mut out);
        out.retain(|p| p.ends_with(suffix));
        out.sort();
        Ok(out)
    }

    fn read(&self, path: &str) -> Result<Vec<u8>, String> {
        std::fs::read(self.0.join(path)).map_err(|e| e.to_string())
    }

    /// The same naming rule the host applies: the directory's own name, or its
    /// parent's when it is the `src` of something.
    fn label(&self) -> Option<String> {
        let name = |p: &std::path::Path| p.file_name()?.to_str().map(str::to_string);
        match name(&self.0).as_deref() {
            Some("src") => name(self.0.parent()?).or_else(|| name(&self.0)),
            _ => name(&self.0),
        }
    }
}

/// Parse every `.rs` in the tree and assemble — the two contract phases,
/// called the way the host calls them.
fn run(t: &Tree) -> Assembled {
    run_files(&TestFiles::rooted(t.0.clone()))
}

fn run_files(files: &TestFiles) -> Assembled {
    let paths = files.list(".rs").unwrap();
    assemble(parse_chunk(files, &paths, false))
}

fn run_with_source(t: &Tree) -> Assembled {
    let files = TestFiles::rooted(t.0.clone());
    let paths = files.list(".rs").unwrap();
    assemble(parse_chunk(&files, &paths, true))
}

fn keys(p: &Assembled) -> Vec<&str> {
    p.nodes.iter().map(|n| n.key.as_str()).collect()
}

/// A property's text, unwrapping the `{"$desc": …, "$value": …}` form a
/// described property travels in.
fn prop(p: &Assembled, key: &str, name: &str) -> Option<String> {
    let v = p.nodes.iter().find(|n| n.key == key)?.props.get(name)?;
    text_of(v)
}

fn text_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) if o.contains_key("$value") => text_of(o.get("$value")?),
        _ => None,
    }
}

const LIB: &str = r#"
//! The crate.
use std::fmt;

/// Adds.
pub fn add(a: i64, b: i64) -> i64 { let sum = a + b; sum }

pub fn caller() -> i64 { add(1, 2) }

pub const LIMIT: usize = 4;
pub static NAME: &str = "x";
pub type Pair = (i64, i64);

pub struct Thing;

impl fmt::Display for Thing {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result { Ok(()) }
}

pub trait Local {}
impl Local for Thing {}
"#;

const SHAPES: &str = r#"
pub struct T;

impl T {
    /// An associated function: no receiver, so not a method.
    pub fn open() -> T { T }
    pub fn read(&self) -> usize { 0 }
    pub fn write(&mut self, n: usize) {}
    pub fn consume(self) {}
    pub async fn fetch(&self) -> Result<usize, ()> { Ok(0) }
}

pub fn free() {}
pub async fn free_async() -> i64 { 0 }
"#;

/// The one invariant the write path enforces: an edge naming a node that does
/// not exist is refused, so a graph with one is not merely imprecise but
/// unwritable.
fn every_edge_has_endpoints(out: &Assembled) {
    let keys: std::collections::BTreeSet<&str> = out.nodes.iter().map(|n| n.key.as_str()).collect();
    for e in &out.edges {
        assert!(keys.contains(e.src.as_str()), "dangling src: {}", e.src);
        assert!(keys.contains(e.dst.as_str()), "dangling dst: {}", e.dst);
    }
}

/// An item's identity is its module path, not the file it lives in — that is
/// what a Rust programmer calls it, and what a model will recognise.
#[test]
fn keys_are_module_paths_with_the_crate_name() {
    let t = Tree::new("keys");
    t.write("Cargo.toml", "[package]\nname = \"my-crate\"\n")
        .write("src/lib.rs", LIB)
        .write("src/deep/thing.rs", "pub fn helper() {}");

    let out = run(&t);
    let k = keys(&out);

    // `-` becomes `_`, as it does in code.
    assert!(k.contains(&"my_crate::add"), "{k:?}");
    assert!(k.contains(&"my_crate::LIMIT"), "{k:?}");
    assert!(k.contains(&"my_crate::Pair"), "{k:?}");
    // `lib.rs` names the crate root rather than a module called `lib`.
    assert!(k.contains(&"my_crate"), "{k:?}");
    // A nested file is a nested module.
    assert!(k.contains(&"my_crate::deep::thing::helper"), "{k:?}");
}

/// Pointed at `…/foo/src`, the manifest is one level up and outside the grant.
/// Falling back to a bare `crate::` there would merge two crates' `api::Thing`
/// into one node the moment both are ingested into the same plane.
#[test]
fn a_src_directory_is_named_after_the_crate_holding_it() {
    let outer = Tree::new("srcroot");
    outer
        .write("my-crate/Cargo.toml", "[package]\nname = \"my-crate\"\n")
        .write("my-crate/src/lib.rs", "pub fn only() {}");

    let out = run_files(&TestFiles::rooted(outer.0.join("my-crate/src")));
    assert!(keys(&out).contains(&"my_crate::only"), "{:?}", keys(&out));
}

/// Consts, statics, type aliases and macros are interface too — a reader
/// looking for `LIMIT` should find it.
#[test]
fn items_beyond_functions_and_types_are_emitted() {
    let t = Tree::new("items");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", LIB);
    let out = run(&t);

    for (key, label) in [
        ("k::LIMIT", "Const"),
        ("k::NAME", "Static"),
        ("k::Pair", "TypeAlias"),
        ("k::Thing", "Struct"),
        ("k::Local", "Trait"),
    ] {
        let node = out.nodes.iter().find(|n| n.key == key);
        assert_eq!(node.map(|n| n.label.as_str()), Some(label), "{key}");
    }
    // Imports are recorded on the module rather than as nodes for std paths.
    assert!(
        prop(&out, "k", "imports").is_some_and(|i| i.contains("std::fmt")),
        "imports missing"
    );
}

/// `impl Display for Thing` names a trait from another crate. Assuming it lived
/// in this module would point the edge at a node that does not exist; dropping
/// it would lose the most useful thing the impl block says.
#[test]
fn a_foreign_trait_becomes_an_external_node() {
    let t = Tree::new("impls");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", LIB);
    let out = run(&t);

    let external = out
        .nodes
        .iter()
        .find(|n| n.key.ends_with("Display"))
        .expect("Display should exist as a node");
    // Two labels: what it is, and that it is not ours. Both are things a reader
    // asks for by label — `MATCH (t:Trait)` should find `Display`, and
    // `MATCH (n:External)` should find everything foreign whatever its kind.
    assert_eq!(external.label, "Trait");
    assert_eq!(external.extra_labels, vec!["External".to_string()]);
    assert!(
        out.edges
            .iter()
            .any(|e| e.ty == "IMPLEMENTS" && e.dst == external.key),
        "the IMPLEMENTS edge should point at it"
    );
    // A trait defined here resolves to its own module-path key instead.
    assert!(
        out.edges
            .iter()
            .any(|e| e.ty == "IMPLEMENTS" && e.dst == "k::Local"),
        "a local trait should resolve locally"
    );
}

#[test]
fn calls_resolve_by_name_and_locals_are_listed() {
    let t = Tree::new("calls");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", LIB);
    let out = run(&t);

    assert!(
        out.edges
            .iter()
            .any(|e| e.ty == "CALLS" && e.src == "k::caller" && e.dst == "k::add"),
        "caller -> add should be an edge"
    );
    assert_eq!(
        prop(&out, "k::add", "local_bindings").as_deref(),
        Some("sum")
    );
}

/// A return type is a fact worth querying, and it is not one while it is a
/// substring of a rendered signature.
#[test]
fn a_function_records_what_it_returns() {
    let t = Tree::new("returns");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", SHAPES);
    let out = run(&t);

    assert_eq!(prop(&out, "k::T::open", "returns").as_deref(), Some("T"));
    assert_eq!(
        prop(&out, "k::T::read", "returns").as_deref(),
        Some("usize")
    );
    assert_eq!(
        prop(&out, "k::free_async", "returns").as_deref(),
        Some("i64")
    );
    assert_eq!(
        prop(&out, "k::T::fetch", "returns").as_deref(),
        Some("Result<usize,()>")
    );
    // `-> ()` and no arrow are the same function in Rust, so absence is the
    // unit type rather than a missing answer.
    assert_eq!(prop(&out, "k::free", "returns").as_deref(), None);
    assert_eq!(prop(&out, "k::T::write", "returns").as_deref(), None);
}

/// "Every method on this type" should be a label query, not a scan for a
/// leading `self` inside a rendered signature.
#[test]
fn a_method_is_labelled_apart_from_a_function() {
    let t = Tree::new("methods");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", SHAPES);
    let out = run(&t);

    let label = |key: &str| {
        out.nodes
            .iter()
            .find(|n| n.key == key)
            .map(|n| n.label.as_str())
    };
    assert_eq!(label("k::T::read"), Some("Method"));
    assert_eq!(label("k::T::write"), Some("Method"));
    assert_eq!(label("k::T::consume"), Some("Method"));
    // An associated function is not a method, which is the distinction Rust
    // itself draws — `T::open()` takes no receiver.
    assert_eq!(label("k::T::open"), Some("Function"));
    assert_eq!(label("k::free"), Some("Function"));

    // The receiver's own form is kept, since `&self` and `&mut self` are a
    // real difference between two methods.
    assert_eq!(
        prop(&out, "k::T::read", "receiver").as_deref(),
        Some("&self")
    );
    assert_eq!(
        prop(&out, "k::T::write", "receiver").as_deref(),
        Some("&mut self")
    );
    assert_eq!(
        prop(&out, "k::T::consume", "receiver").as_deref(),
        Some("self")
    );
    assert_eq!(prop(&out, "k::T::open", "receiver").as_deref(), None);
}

/// Splitting the label must not split the call graph: both kinds are callable.
#[test]
fn methods_still_resolve_as_call_targets() {
    let t = Tree::new("method-calls-label");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        "pub struct T;\nimpl T { pub fn go(&self) { self.inner(); }\n pub fn inner(&self) {} }",
    );
    let out = run(&t);
    assert!(
        out.edges
            .iter()
            .any(|e| e.ty == "CALLS" && e.src == "k::T::go" && e.dst == "k::T::inner"),
        "a method must still be reachable as a call target"
    );
}

/// `async fn` and `fn` are different things to call, and the difference is
/// otherwise buried in the signature string.
#[test]
fn an_async_function_says_so() {
    let t = Tree::new("asyncness");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", SHAPES);
    let out = run(&t);

    let is_async = |key: &str| {
        out.nodes
            .iter()
            .find(|n| n.key == key)
            .and_then(|n| n.props.get("is_async"))
    };
    assert_eq!(is_async("k::free_async"), Some(&Value::Bool(true)));
    assert_eq!(is_async("k::T::fetch"), Some(&Value::Bool(true)));
    // Absent means synchronous, the same convention `visibility` uses for
    // private — a property that says "no" on every node is mostly noise.
    assert_eq!(is_async("k::free"), None);
    assert_eq!(is_async("k::T::read"), None);
}

/// We do not read std's code and never will, but "this crate calls that" is
/// worth knowing. A call that writes a path stops at a node holding the path.
#[test]
fn a_call_into_another_crate_stops_at_a_node() {
    let t = Tree::new("external-calls");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        "use std::fs;\npub fn go() { std::mem::swap(); fs::read(); let v = x.trim(); }",
    );

    let out = run(&t);
    let calls: Vec<&str> = out
        .edges
        .iter()
        .filter(|e| e.ty == "CALLS" && e.src == "k::go")
        .map(|e| e.dst.as_str())
        .collect();

    assert!(calls.contains(&"std::mem::swap"), "{calls:?}");
    // Written `fs::read`, but the file said `use std::fs` — so it is recorded
    // under the path that identifies it, not the abbreviation.
    assert!(calls.contains(&"std::fs::read"), "{calls:?}");
    // A method call names no path and the receiver's type is unknowable
    // here — so it lands in the unresolved ledger: a real node, attributed
    // to this file, with the reason on the edge (P1).
    assert!(calls.contains(&"?::src/lib.rs::trim"), "{calls:?}");
    let ledger = out
        .nodes
        .iter()
        .find(|n| n.key == "?::src/lib.rs::trim")
        .expect("UnresolvedRef node");
    assert_eq!(ledger.label, "UnresolvedRef");
    let trim_edge = out
        .edges
        .iter()
        .find(|e| e.dst == "?::src/lib.rs::trim")
        .unwrap();
    assert_eq!(
        trim_edge.props.get("_resolved_by"),
        Some(&Value::String("unresolved".into()))
    );
    assert!(
        matches!(trim_edge.props.get("_reason"), Some(Value::String(r)) if r.contains("receiver"))
    );
    // And the resolved edges carry their strategy stamps.
    let read_edge = out
        .edges
        .iter()
        .find(|e| e.dst == "std::fs::read" && e.src == "k::go")
        .unwrap();
    assert_eq!(
        read_edge.props.get("_resolved_by"),
        Some(&Value::String("external-path".into()))
    );
    assert_eq!(
        read_edge.props.get("_ref"),
        Some(&Value::String("std::fs::read".into()))
    );

    // A call site proves the target is callable, so it is labelled as both what
    // it is and as foreign.
    let node = out.nodes.iter().find(|n| n.key == "std::fs::read").unwrap();
    assert_eq!(node.label, "Function");
    assert_eq!(node.extra_labels, vec!["External".to_string()]);
    // No signature: we would have to read std's source to know one, and
    // inventing one is worse than its absence.
    assert!(!node.props.contains_key("signature"));
    every_edge_has_endpoints(&out);
}

/// A module's imports are edges to what they name, and the property lists the
/// same resolved keys — so each entry can be followed to its node.
#[test]
fn imports_become_edges_to_resolved_keys() {
    let t = Tree::new("imports-edges");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", "pub mod thing;\npub mod user;")
        .write("src/thing.rs", "pub struct Thing;")
        .write(
            "src/user.rs",
            "use crate::thing::Thing;\nuse std::sync::Arc;\nuse std::io::*;",
        );

    let out = run(&t);
    let imported: Vec<&str> = out
        .edges
        .iter()
        .filter(|e| e.ty == "IMPORTS" && e.src == "k::user")
        .map(|e| e.dst.as_str())
        .collect();

    // A local import lands on the real node; a foreign one on a stand-in.
    assert!(imported.contains(&"k::thing::Thing"), "{imported:?}");
    assert!(imported.contains(&"std::sync::Arc"), "{imported:?}");
    // A glob names no single target, so it is no edge.
    assert!(!imported.iter().any(|i| i.contains('*')), "{imported:?}");

    // The property carries the resolved keys, not `crate::thing::Thing` — a
    // key that is not the node's key cannot be followed to it.
    let list = prop(&out, "k::user", "imports").unwrap();
    assert!(list.contains("k::thing::Thing"), "{list}");
    assert!(!list.contains("crate::thing"), "{list}");
    // The glob is still listed: the file did write it.
    assert!(list.contains("std::io::*"), "{list}");

    // A stand-in for something only ever imported says nothing about its kind.
    let arc = out
        .nodes
        .iter()
        .find(|n| n.key == "std::sync::Arc")
        .unwrap();
    // Only ever imported, so its kind is unknown and `External` is the whole
    // honest answer — no second label invented to sit beside it.
    assert_eq!(arc.label, "External");
    assert!(arc.extra_labels.is_empty());
    every_edge_has_endpoints(&out);
}

/// A facade is the normal shape of a Rust crate: `lib.rs` republishes a module
/// and everything then refers to it by the short path. Nothing is declared
/// there, so an unfollowed re-export files the crate's own type under a
/// foreign-looking key.
#[test]
fn a_reexported_path_resolves_to_the_real_item() {
    let t = Tree::new("reexports");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", "pub mod api;\npub use api::cache;")
        .write("src/api/mod.rs", "pub mod cache;")
        // `pub(crate)` counts: it creates the facade path for the whole crate,
        // which is exactly the scope being parsed.
        .write(
            "src/api/cache/mod.rs",
            "pub mod store;\npub(crate) use store::GraphCache;\npub struct CachedReader;",
        )
        .write("src/api/cache/store.rs", "pub struct GraphCache;")
        .write(
            "src/user.rs",
            "use crate::cache::{CachedReader, GraphCache};",
        );

    let out = run(&t);
    let list = prop(&out, "k::user", "imports").unwrap();

    assert!(list.contains("k::api::cache::CachedReader"), "{list}");
    // Two hops: `crate::cache` → `api::cache`, then `GraphCache` → `store`.
    assert!(list.contains("k::api::cache::store::GraphCache"), "{list}");
    // And neither became an external stand-in for the crate's own type.
    assert!(
        !out.nodes.iter().any(|n| n.label == "External"),
        "{:?}",
        out.nodes
            .iter()
            .filter(|n| n.label == "External")
            .map(|n| &n.key)
            .collect::<Vec<_>>()
    );
    every_edge_has_endpoints(&out);
}

/// A re-export's target is written relative to the module that wrote it and
/// may itself be a facade. `lib.rs` says `pub use compute::{Expr}` while
/// `compute/mod.rs` says `pub use expr::Expr` — the first only makes sense
/// once the second is known, so one pass leaves a relative path naming nothing.
#[test]
fn a_reexport_chain_resolves_through_its_own_facades() {
    let t = Tree::new("chain");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", "pub mod compute;\npub use compute::Expr;")
        .write("src/compute/mod.rs", "pub mod expr;\npub use expr::Expr;")
        .write("src/compute/expr.rs", "pub enum Expr { Lit }")
        .write("src/user.rs", "use crate::Expr;");

    let out = run(&t);
    assert_eq!(
        prop(&out, "k::user", "imports").as_deref(),
        Some("k::compute::expr::Expr")
    );
    // The crate's own enum must not appear as somebody else's type.
    assert!(
        !out.nodes.iter().any(|n| n.label == "External"),
        "{:?}",
        keys(&out)
    );
    every_edge_has_endpoints(&out);
}

/// `use a::b::{self, C}` imports `a::b` itself. Read as a child named `self`
/// it becomes a path naming no item — a node nothing could ever be.
#[test]
fn a_self_import_names_the_module_itself() {
    let t = Tree::new("self-import");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", "pub mod algo;")
        .write("src/algo.rs", "pub struct Options;")
        .write("src/user.rs", "use crate::algo::{self, Options};");

    let out = run(&t);
    let list = prop(&out, "k::user", "imports").unwrap();
    assert!(list.contains("k::algo"), "{list}");
    assert!(!list.contains("::self"), "{list}");
    assert!(
        !keys(&out).iter().any(|k| k.ends_with("::self")),
        "{:?}",
        keys(&out)
    );
    every_edge_has_endpoints(&out);
}

/// A node saying only `Expr` says almost nothing — the variants are the shape
/// of the type, and each one's fields are part of that shape.
#[test]
fn an_enum_records_its_variants() {
    let t = Tree::new("variants");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        r#"
pub enum Expr {
    Unit,
    Lit(i64),
    Pair(i64, String),
    Prop { name: String, value: i64 },
}

#[repr(u8)]
pub enum Code { Ok = 0, Bad = 1 }
"#,
    );

    let out = run(&t);
    let variants = |key: &str| match out
        .nodes
        .iter()
        .find(|n| n.key == key)
        .and_then(|n| n.props.get("variants"))
        .map(|v| v.get("$value").unwrap_or(v))
    {
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };

    assert_eq!(
        variants("k::Expr"),
        vec![
            "Unit",
            "Lit(i64)",
            "Pair(i64, String)",
            "Prop { name: String, value: i64 }",
        ]
    );
    // A discriminant is part of what the variant is.
    assert_eq!(variants("k::Code"), vec!["Ok = 0", "Bad = 1"]);
}

/// The same argument as an enum's variants: a node saying only `NodeRecord`
/// says almost nothing, while its fields are the shape of it.
#[test]
fn a_struct_records_its_fields() {
    let t = Tree::new("fields");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        r#"
pub struct Record {
    pub id: NodeId,
    pub(crate) labels: Vec<String>,
    seq: u64,
}

pub struct Wrapper(pub i64, String);

pub struct Marker;
"#,
    );

    let out = run(&t);
    let fields = |key: &str| match out
        .nodes
        .iter()
        .find(|n| n.key == key)
        .and_then(|n| n.props.get("fields"))
        .map(|v| v.get("$value").unwrap_or(v))
    {
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };

    // Visibility is part of what a struct is — which fields a caller may touch.
    assert_eq!(
        fields("k::Record"),
        vec![
            "pub id: NodeId",
            "pub(crate) labels: Vec<String>",
            "seq: u64"
        ]
    );
    // A tuple struct's fields are positions rather than names.
    assert_eq!(fields("k::Wrapper"), vec!["pub 0: i64", "1: String"]);
    // A unit struct has none, and an empty list is noise on every read.
    assert!(
        out.nodes
            .iter()
            .find(|n| n.key == "k::Marker")
            .is_some_and(|n| !n.props.contains_key("fields")),
        "a unit struct should carry no fields property"
    );
}

/// `#[non_exhaustive]` is a promise about the future rather than a detail of
/// the present: it decides whether a downstream `match` needs a wildcard arm.
#[test]
fn non_exhaustive_is_recorded_where_it_is_written() {
    let t = Tree::new("nonexhaustive");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        r#"
#[non_exhaustive]
pub enum Open { A, B }

pub enum Closed { A }

#[non_exhaustive]
pub struct Growing { pub a: i64 }

pub struct Fixed;
"#,
    );

    let out = run(&t);
    let flagged = |key: &str| {
        out.nodes
            .iter()
            .find(|n| n.key == key)
            .and_then(|n| n.props.get("non_exhaustive"))
    };

    let truthy = |v: Option<&Value>| v.map(|x| x.get("$value").unwrap_or(x) == &Value::Bool(true));
    assert_eq!(truthy(flagged("k::Open")), Some(true));
    // The same attribute means the same thing on a struct.
    assert_eq!(truthy(flagged("k::Growing")), Some(true));
    // Absent means exhaustive, as absent `visibility` means private.
    assert_eq!(flagged("k::Closed"), None);
    assert_eq!(flagged("k::Fixed"), None);
}

/// Nothing expands macros — that is the compiler's job, and a proc macro is
/// arbitrary code that would have to be *run*. So the items an invocation
/// declares are absent, and the point of this test is that their absence is
/// marked rather than silent.
#[test]
fn an_item_macro_invocation_is_marked_not_expanded() {
    let t = Tree::new("macros");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        r#"
pub enum Expr { Lit }

macro_rules! expr_from_literal {
    ($($t:ty),*) => {$(
        impl From<$t> for Expr { fn from(v: $t) -> Self { Expr::Lit } }
    )*};
}

expr_from_literal!(bool, i64, String);
"#,
    );

    let out = run(&t);

    // The definition is an item and gets a node.
    assert!(
        keys(&out).contains(&"k::expr_from_literal"),
        "{:?}",
        keys(&out)
    );

    // The invocation is an edge to it, carrying what it was given — which is
    // the shape of what it generated.
    let invoke = out
        .edges
        .iter()
        .find(|e| e.ty == "INVOKES")
        .expect("the invocation should be recorded");
    assert_eq!(invoke.src, "k");
    assert_eq!(invoke.dst, "k::expr_from_literal");
    let args = invoke
        .props
        .get("arguments")
        .and_then(text_of)
        .unwrap_or_default();
    assert!(
        args.contains("bool") && args.contains("String"),
        "{:?}",
        invoke.props
    );

    // The three `impl From<…> for Expr` blocks it generates are *not* there,
    // and the report says so rather than leaving a reader to wonder.
    assert!(
        !out.edges.iter().any(|e| e.ty == "IMPLEMENTS"),
        "generated impls cannot be known without expanding"
    );
    assert!(
        out.notes
            .iter()
            .any(|n| n.contains("not expanded") && n.contains("INVOKES")),
        "{:?}",
        out.notes
    );
    every_edge_has_endpoints(&out);
}

/// A `macro_rules!` written inside a function body is a local helper, not an
/// item of the module — three in a crate should not become three nodes.
#[test]
fn a_macro_inside_a_body_is_not_an_item() {
    let t = Tree::new("localmacro");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        "pub fn go() { macro_rules! run { () => { 1 } } run!(); }",
    );

    let out = run(&t);
    assert!(
        !keys(&out).iter().any(|k| k.contains("run")),
        "{:?}",
        keys(&out)
    );
}

/// A trait's own items are its interface — the thing an implementor must
/// supply — and are as much a part of it as an impl block's are.
#[test]
fn a_traits_own_items_are_emitted() {
    let t = Tree::new("trait-items");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        r#"
pub trait Reader {
    /// Required.
    fn get(&self, id: u64) -> Option<u64>;
    /// Provided, with a body.
    fn count(&self) -> usize { let n = 0; n }
    fn make() -> Self where Self: Sized;
}
"#,
    );

    let out = run(&t);
    let node = |key: &str| out.nodes.iter().find(|n| n.key == key);

    // A `self` receiver makes it a method here exactly as in an impl block.
    assert_eq!(
        node("k::Reader::get").map(|n| n.label.as_str()),
        Some("Method")
    );
    assert_eq!(
        prop(&out, "k::Reader::get", "receiver").as_deref(),
        Some("&self")
    );
    assert_eq!(
        prop(&out, "k::Reader::get", "returns").as_deref(),
        Some("Option<u64>")
    );
    // An associated function without a receiver is a `Function`.
    assert_eq!(
        node("k::Reader::make").map(|n| n.label.as_str()),
        Some("Function")
    );
    // A default body is where the bindings come from when there is one.
    assert_eq!(
        prop(&out, "k::Reader::count", "local_bindings").as_deref(),
        Some("n")
    );
    assert!(
        out.edges
            .iter()
            .any(|e| e.ty == "HAS_METHOD" && e.src == "k::Reader" && e.dst == "k::Reader::get"),
        "the trait should own its items"
    );
    every_edge_has_endpoints(&out);
}

/// A union is a struct whose fields overlap in memory, and is described the
/// same way.
#[test]
fn a_union_records_its_fields() {
    let t = Tree::new("union");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        "pub union Word { pub bits: u64, pub halves: [u32; 2] }",
    );

    let out = run(&t);
    let node = out.nodes.iter().find(|n| n.key == "k::Word").unwrap();
    assert_eq!(node.label, "Union");
    let fields = node
        .props
        .get("fields")
        .map(|v| v.get("$value").unwrap_or(v));
    assert!(matches!(fields, Some(Value::Array(items)) if items.len() == 2));
}

/// `extern crate`, `use x as y` and `use a::{self as b}` all bring a name into
/// scope under a name of the caller's choosing.
#[test]
fn renamed_and_extern_imports_are_recorded() {
    let t = Tree::new("renames");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", "pub mod inner;")
        .write("src/inner.rs", "pub struct Thing;")
        .write(
            "src/user.rs",
            "extern crate alloc;\nuse crate::inner::Thing as Renamed;\nuse crate::{inner as nested};",
        );

    let out = run(&t);
    let list = prop(&out, "k::user", "imports").unwrap();

    // The rename names the *original*, which is what the node is keyed by.
    assert!(list.contains("k::inner::Thing"), "{list}");
    // `{inner as nested}` is the module itself, not a child called `self`.
    assert!(list.contains("k::inner"), "{list}");
    assert!(list.contains("alloc"), "{list}");
    every_edge_has_endpoints(&out);
}

/// A `let` can bind several names at once, and every one of them is a name the
/// function introduced.
#[test]
fn destructuring_bindings_are_all_listed() {
    let t = Tree::new("patterns");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        r#"
pub struct P { pub x: i64, pub y: i64 }
pub fn go(p: P) {
    let (first, second) = (1, 2);
    let P { x, y } = p;
    let [head, tail] = [3, 4];
    let &borrowed = &5;
    let typed: i64 = 6;
    let Some(inner) = None::<i64> else { return };
}
"#,
    );

    let out = run(&t);
    let bound = prop(&out, "k::go", "local_bindings").unwrap();
    for name in [
        "first", "second", "x", "y", "head", "tail", "borrowed", "typed", "inner",
    ] {
        assert!(bound.contains(name), "{name} missing from {bound}");
    }
}

/// Off by default: every body stored is roughly a copy of the codebase.
#[test]
fn source_is_stored_only_when_asked_for() {
    let t = Tree::new("source");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", LIB);

    let off = run(&t);
    assert!(prop(&off, "k::add", "_code").is_none());

    let on = run_with_source(&t);
    assert!(
        prop(&on, "k::add", "_code").is_some_and(|c| c.contains("sum")),
        "the body should be retrievable when asked for"
    );
    // Underscore-prefixed, so it stays out of embeddings and the schema summary.
    assert!(prop(&on, "k::add", "source_code_raw").is_none());
}

/// Determinism is the reason the walk is sorted and the parallel results are
/// collected in order rather than as they finish.
#[test]
fn the_same_tree_twice_gives_the_same_result() {
    let t = Tree::new("determinism");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", LIB)
        .write("src/a.rs", "pub fn a() {}")
        .write("src/b.rs", "pub fn b() {}")
        .write("docs/one.md", "# One")
        .write("docs/two.md", "# Two");

    let first = run(&t);
    let second = run(&t);

    assert_eq!(keys(&first), keys(&second), "node order must be stable");
    let e = |p: &Assembled| {
        p.edges
            .iter()
            .map(|e| format!("{}-{}->{}", e.src, e.ty, e.dst))
            .collect::<Vec<_>>()
    };
    assert_eq!(e(&first), e(&second), "edge order must be stable");
}

/// A file that will not parse is reported, not fatal: one broken fixture should
/// not sink an ingest.
#[test]
fn an_unparsable_file_is_reported_rather_than_fatal() {
    let t = Tree::new("broken");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", "pub fn fine() {}")
        .write("src/broken.rs", "pub fn ( ) ) {{{");

    let out = run(&t);
    assert!(
        keys(&out).contains(&"k::fine"),
        "the good file still parses"
    );
    assert!(
        out.notes.iter().any(|n| n.contains("did not parse")),
        "{:?}",
        out.notes
    );
}

/// Two packages both have a `benches/graph.rs`, and neither one is reachable
/// from its library's module tree — a bench is its own crate root.
#[test]
fn non_library_targets_are_keyed_under_their_package() {
    let t = Tree::new("targets");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("benches/graph.rs", "fn timed() {}")
        .write("tests/api.rs", "fn checks() {}")
        // A module *called* tests, inside the library — not an integration test.
        .write("src/tests/mod.rs", "pub fn inner() {}");

    let out = run(&t);
    let k = keys(&out);
    assert!(k.contains(&"k::benches::graph::timed"), "{k:?}");
    assert!(k.contains(&"k::tests::api::checks"), "{k:?}");
    assert!(k.contains(&"k::tests::inner"), "{k:?}");
}

/// Six `impl From<…> for PropValue` blocks are six different functions. Keyed
/// by the type alone they would be one node, and five would be dropped as
/// collisions.
#[test]
fn trait_impl_methods_are_keyed_by_qualified_path() {
    let t = Tree::new("qualified");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        r#"
pub enum V { I(i64), F(f64) }
impl From<i64> for V { fn from(v: i64) -> Self { V::I(v) } }
impl From<f64> for V { fn from(v: f64) -> Self { V::F(v) } }
impl V { pub fn new() -> Self { V::I(0) } }
"#,
    );

    let out = run(&t);
    let k = keys(&out);
    assert!(k.contains(&"<k::V as From<i64>>::from"), "{k:?}");
    assert!(k.contains(&"<k::V as From<f64>>::from"), "{k:?}");
    // An inherent impl needs no qualifying: there is only one.
    assert!(k.contains(&"k::V::new"), "{k:?}");
    {
        // The router counted cross-handler collisions; the parser-level claim
        // is stronger and direct — no key is produced twice.
        let mut seen = std::collections::BTreeSet::new();
        for k in keys(&out) {
            assert!(seen.insert(k.to_string()), "duplicate key: {k}");
        }
    }

    // One `From` node, with the arguments on the edges instead.
    assert_eq!(out.nodes.iter().filter(|n| n.key == "From").count(), 1);
    let args: Vec<_> = out
        .edges
        .iter()
        .filter(|e| e.ty == "IMPLEMENTS")
        .filter_map(|e| e.props.get("impl"))
        .map(|d| format!("{d:?}"))
        .collect();
    assert_eq!(args.len(), 2, "{args:?}");
}

/// Hundreds of `new`/`from`/`len` share a name across a workspace. Resolving
/// them by the caller's own module first is what turns most of those calls from
/// ambiguous into edges.
/// An impl that spells its trait out in full must land on the real node. Going
/// by simple name alone, two traits sharing a name make the lookup ambiguous,
/// and the "external" stand-in it falls back to would carry a key one of them
/// already owns — replacing a real trait with a placeholder.
#[test]
fn a_fully_qualified_impl_finds_the_trait_it_names() {
    let t = Tree::new("qualified-impl");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", "pub mod one;\npub mod two;\npub struct T;")
        .write("src/one.rs", "pub trait Shared {}")
        // A second trait of the same name, which is what defeats a by-name
        // lookup — and the impl below names its target in full.
        .write("src/two.rs", "pub trait Shared {}")
        .write(
            "src/uses.rs",
            "impl crate::one::Shared for crate::T {}\npub struct Other;",
        );

    let out = run(&t);
    {
        // The router counted cross-handler collisions; the parser-level claim
        // is stronger and direct — no key is produced twice.
        let mut seen = std::collections::BTreeSet::new();
        for k in keys(&out) {
            assert!(seen.insert(k.to_string()), "duplicate key: {k}");
        }
    }
    // The real trait, not a stand-in wearing its key.
    let node = out
        .nodes
        .iter()
        .find(|n| n.key == "k::one::Shared")
        .unwrap();
    assert!(
        !node.props.contains_key("external"),
        "a trait this crate defines is not external"
    );
}

/// `impl Database` sits in `api/snapshot.rs` while `Database` is declared in
/// `api/mod.rs`. Keying the block by the module holding it invents
/// `k::api::snapshot::Database`, and every method and `IMPLEMENTS` edge then
/// hangs off a node that does not exist — which a bulk write refuses outright.
#[test]
fn an_impl_finds_a_type_declared_in_another_file() {
    let t = Tree::new("impl-elsewhere");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", "pub mod api;")
        .write("src/api/mod.rs", "pub mod snapshot;\npub struct Database;")
        .write(
            "src/api/snapshot.rs",
            "use super::Database;\nimpl Database { pub fn snapshot(&self) {} }",
        );

    let out = run(&t);
    let k = keys(&out);
    assert!(k.contains(&"k::api::Database::snapshot"), "{k:?}");
    assert!(
        !k.iter().any(|k| k.contains("snapshot::Database")),
        "the impl must not invent a type in its own module: {k:?}"
    );
    every_edge_has_endpoints(&out);
}

/// A method's calls have to resolve too, and they cannot until the block its
/// method belongs to has been resolved — which is why it happens in phases.
#[test]
fn a_method_body_resolves_its_calls() {
    let t = Tree::new("method-calls");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", "pub mod thing;\npub fn helper() {}")
        .write(
            "src/thing.rs",
            "pub struct T;\nimpl T { pub fn go(&self) { crate::helper(); self.inner(); }\n pub fn inner(&self) {} }",
        );

    let out = run(&t);
    let calls: Vec<(&str, &str)> = out
        .edges
        .iter()
        .filter(|e| e.ty == "CALLS")
        .map(|e| (e.src.as_str(), e.dst.as_str()))
        .collect();
    assert!(
        calls.contains(&("k::thing::T::go", "k::helper")),
        "{calls:?}"
    );
    // A method calling a sibling method resolves to the qualified key.
    assert!(
        calls.contains(&("k::thing::T::go", "k::thing::T::inner")),
        "{calls:?}"
    );
    every_edge_has_endpoints(&out);
}

/// An `impl` block's Self type is keyed by the path the source wrote, expanded
/// through the file's imports — the same key a call or a `use` would produce.
/// Keeping only `HashSet` would merge every crate's `HashSet` into one node and
/// would not match `std::collections::HashSet` written out elsewhere.
#[test]
fn an_external_impl_target_keeps_its_whole_path() {
    let t = Tree::new("impl-target-path");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        r#"
use std::collections::BTreeMap;
pub trait Filter {}
impl Filter for std::collections::HashSet<u64> {}
impl Filter for BTreeMap<u64, u64> {}
"#,
    );

    let out = run(&t);
    let k = keys(&out);

    // Written out in full at the impl site.
    assert!(k.contains(&"std::collections::HashSet"), "{k:?}");
    // Written short, but the file said where it came from.
    assert!(k.contains(&"std::collections::BTreeMap"), "{k:?}");
    assert!(!k.contains(&"HashSet"), "the bare tail is not a key: {k:?}");

    // A use site proves it is a type; it does not prove which kind, so `Type`
    // rather than a guess at `Struct` — `impl … for Option<T>` would be an enum.
    let hs = out
        .nodes
        .iter()
        .find(|n| n.key == "std::collections::HashSet")
        .unwrap();
    assert_eq!(hs.label, "Type");
    assert_eq!(hs.extra_labels, vec!["External".to_string()]);
    every_edge_has_endpoints(&out);
}

/// The same path is reached from several places that know different amounts: a
/// `use` says only that a name exists, while an `impl` says it is a type. The
/// import pass runs first, so plain insertion would let the site that knows
/// least win and leave the node labelled `External` alone.
#[test]
fn a_later_site_strengthens_an_external_node() {
    let t = Tree::new("strengthen");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        r#"
use std::collections::HashSet;
use std::fs;
pub trait Filter {}
impl Filter for HashSet<u64> {}
pub fn go() { fs::read(); }
"#,
    );

    let out = run(&t);
    let node = |key: &str| out.nodes.iter().find(|n| n.key == key).unwrap();

    // Imported *and* used as an impl target: the impl knows more, and wins.
    let hs = node("std::collections::HashSet");
    assert_eq!(hs.label, "Type");
    assert_eq!(hs.extra_labels, vec!["External".to_string()]);

    // Imported *and* called through: the call proves it is callable.
    let read = node("std::fs::read");
    assert_eq!(read.label, "Function");
    assert_eq!(read.extra_labels, vec!["External".to_string()]);
    every_edge_has_endpoints(&out);
}

/// Nothing this repository's own tree produces may be unwritable.
#[test]
fn a_foreign_type_still_gets_an_endpoint() {
    let t = Tree::new("foreign");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        "use std::fmt;\npub trait Mine {}\nimpl Mine for Vec<u8> {}\nimpl fmt::Display for String { fn fmt(&self) {} }",
    );

    let out = run(&t);
    every_edge_has_endpoints(&out);
    // `Vec` is nobody's here, so it exists as an external stand-in rather than
    // as a key nothing owns.
    let v = out.nodes.iter().find(|n| n.key == "Vec").expect("Vec node");
    assert_eq!(v.extra_labels, vec!["External".to_string()]);
}

#[test]
fn calls_resolve_to_the_nearest_definition() {
    let t = Tree::new("locality");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", "pub mod a;\npub mod b;")
        .write("src/a.rs", "pub fn helper() {}\npub fn go() { helper(); }")
        .write("src/b.rs", "pub fn helper() {}\npub fn go() { helper(); }");

    let out = run(&t);
    let call = |src: &str| {
        out.edges
            .iter()
            .find(|e| e.ty == "CALLS" && e.src == src)
            .map(|e| e.dst.clone())
    };
    assert_eq!(call("k::a::go").as_deref(), Some("k::a::helper"));
    assert_eq!(call("k::b::go").as_deref(), Some("k::b::helper"));
}

/// Every definition knows its file and line, and every written relation
/// knows the line it is written on: caller —CALLS(line 4)→ callee(line 7).
#[test]
fn lines_and_files_are_recorded() {
    let t = Tree::new("lines");
    t.write("Cargo.toml", "[package]\nname = \"lines\"\n");
    t.write(
        "src/lib.rs",
        "use std::fs;\n\
         \n\
         pub fn caller() {\n\
         \x20   helper();\n\
         \x20   fs::read(\"x\").ok();\n\
         }\n\
         \n\
         fn helper() {}\n\
         \n\
         pub const LIMIT: usize = 4;\n",
    );
    let out = run(&t);

    let node = |key: &str| {
        out.nodes
            .iter()
            .find(|n| n.key == format!("lines::{key}"))
            .unwrap_or_else(|| panic!("no node lines::{key}"))
    };
    assert_eq!(node("caller").props["line"], Value::from(3u64));
    assert_eq!(
        node("caller").props["file"],
        Value::String("src/lib.rs".into())
    );
    assert_eq!(node("helper").props["line"], Value::from(8u64));
    assert_eq!(node("LIMIT").props["line"], Value::from(10u64));

    let edge = |ty: &str, dst: &str| {
        out.edges
            .iter()
            .find(|e| e.ty == ty && e.dst == dst)
            .unwrap_or_else(|| panic!("no {ty} edge to {dst}"))
    };
    assert_eq!(
        edge("CALLS", "lines::helper").props["line"],
        Value::from(4u64),
        "the call site, not the definition"
    );
    assert_eq!(
        edge("CALLS", "std::fs::read").props["line"],
        Value::from(5u64)
    );
    assert_eq!(edge("IMPORTS", "std::fs").props["line"], Value::from(1u64));
    assert_eq!(
        edge("CONTAINS", "lines::helper").props["line"],
        Value::from(8u64)
    );

    // The file-level module spans the file: `path` says which one, and a
    // single line would be a pick.
    let module = out.nodes.iter().find(|n| n.key == "lines").unwrap();
    assert!(module.props.contains_key("path"));
    assert!(!module.props.contains_key("line"));
}

/// A digest rooted at a crate's `src/` still records paths an editor at the
/// crate root can open: `compute/cache.rs` is written `src/compute/cache.rs`,
/// the same convention module resolution already assumed.
#[test]
fn file_paths_are_crate_root_relative() {
    let t = Tree::new("srcroot");
    t.write("src/lib.rs", "pub mod compute;\n")
        .write("src/compute/mod.rs", "pub fn go() {}\n");

    // Rooted at src/ — the host hands `compute/mod.rs`.
    let out = run_files(&TestFiles::rooted(t.0.join("src")));
    let f = out
        .nodes
        .iter()
        .find(|n| n.key.ends_with("::compute::go"))
        .expect("the function");
    assert_eq!(f.props["file"], Value::String("src/compute/mod.rs".into()));
    let module = out
        .nodes
        .iter()
        .find(|n| n.props.contains_key("path") && n.key.ends_with("::compute"))
        .expect("the module");
    assert_eq!(
        module.props["path"],
        Value::String("src/compute/mod.rs".into())
    );

    // Rooted at the crate — the paths already carry `src/`, and stay as written.
    let whole = run(&t);
    let f = whole
        .nodes
        .iter()
        .find(|n| n.key.ends_with("::compute::go"))
        .expect("the function");
    assert_eq!(f.props["file"], Value::String("src/compute/mod.rs".into()));
}

/// Impl methods become nodes at assemble, where the file is no longer in
/// hand — the live serve-watch drill caught them as the one node kind
/// without file attribution, invisible to an incremental sync.
#[test]
fn impl_methods_carry_their_file_like_everything_else() {
    let t = Tree::new("method-file");
    t.write(
        "Cargo.toml",
        "[package]\nname = \"k\"\nversion = \"0.0.0\"\n",
    );
    t.write(
        "src/lib.rs",
        "pub struct S;\nimpl S {\n    pub fn m(&self) {}\n    pub fn assoc() {}\n}\n",
    );
    let out = run(&t);
    for suffix in ["S::m", "S::assoc"] {
        let n = out
            .nodes
            .iter()
            .find(|n| n.key.ends_with(suffix))
            .unwrap_or_else(|| panic!("missing {suffix}"));
        assert_eq!(
            n.props.get("file"),
            Some(&Value::String("src/lib.rs".into())),
            "{suffix} lost its file"
        );
    }
}

fn has_edge(a: &Assembled, src: &str, ty: &str, dst: &str) -> bool {
    a.edges
        .iter()
        .any(|e| e.src == src && e.ty == ty && e.dst == dst)
}

/// Every variant a body builds, as `(type key, variant)`.
fn built(a: &Assembled, src: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = a
        .edges
        .iter()
        .filter(|e| e.src == src && e.ty == "INSTANTIATES")
        .map(|e| {
            let variant = e.props.get("variant").and_then(text_of).unwrap_or_default();
            (e.dst.clone(), variant)
        })
        .collect();
    out.sort();
    out
}

/// Building a value is not calling a function.
///
/// `Ok(v)`, `Mine::A(v)` and `Meters(1.0)` are all spelled like calls and
/// none of them is one; read as calls they put `Function` nodes named `Ok`
/// and `Mine::A` in the graph, while the enum those names belong to sat in it
/// unlinked. Each is an `INSTANTIATES` edge to the type built, with the
/// variant on the edge — so "who builds a `Mine`?" is one hop, and a
/// constructor that is not an item gets no node of its own.
#[test]
fn constructing_a_value_is_instantiation_not_a_call() {
    let t = Tree::new("ctor-instantiate");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        r#"
pub enum Mine { A(i64), B { x: i64 }, C }
pub struct Meters(pub f64);
pub struct Named { pub x: i64 }

pub fn build(n: i64) -> Result<Option<Mine>, String> {
    let _tuple_variant = Mine::A(n);
    let _struct_variant = Mine::B { x: n };
    let _tuple_struct = Meters(1.0);
    let _struct_literal = Named { x: n };
    let _external = std::num::Wrapping(n);
    let _still_a_call = String::from("x");
    Ok(Some(Mine::A(n)))
}

impl Meters {
    pub fn zero() -> Self { Self(0.0) }
}
"#,
    );
    let a = run(&t);

    assert_eq!(
        built(&a, "k::build"),
        vec![
            ("Option".into(), "Some".into()),
            ("Result".into(), "Ok".into()),
            ("k::Meters".into(), String::new()),
            ("k::Mine".into(), "A".into()),
            ("k::Mine".into(), "B".into()),
            ("k::Named".into(), String::new()),
            ("std::num::Wrapping".into(), String::new()),
        ],
    );

    // No node is invented for a constructor, and the prelude enums the
    // constructions name become reachable instead.
    let keys = keys(&a);
    for phantom in ["Ok", "Some", "Mine::A", "Mine::B", "Meters"] {
        assert!(!keys.contains(&phantom), "invented a node for `{phantom}`");
    }
    assert!(
        keys.contains(&"Result") && keys.contains(&"Option"),
        "{keys:?}"
    );

    // `Self` names the impl's type. Left as written it would put one node
    // called `Self` in the graph for every impl in the tree.
    assert_eq!(
        built(&a, "k::Meters::zero"),
        vec![("k::Meters".to_string(), String::new())],
    );
    assert!(!keys.contains(&"Self"), "minted a node for `Self`");

    // A function that merely looks like a constructor is still a call.
    assert!(has_edge(&a, "k::build", "CALLS", "String::from"));
    every_edge_has_endpoints(&a);
}

/// A std method is keyed by the type that declares it, not by the receiver.
///
/// `Range` declares no `collect` — it *is* an `Iterator`, and `Iterator`
/// declares it. Keyed by the receiver, one method scattered into a node per
/// concrete iterator the tree happened to build, and "who calls
/// `Iterator::map`?" answered with a fraction of them.
#[test]
fn std_iterator_methods_are_keyed_by_the_trait_that_declares_them() {
    let t = Tree::new("iter-owner");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        r#"
pub fn over_a_range() -> usize {
    let r = 0..10;
    let doubled: Vec<usize> = r.map(|n| n * 2).collect();
    doubled.len()
}

pub fn over_chars(s: String) -> usize {
    let c = s.chars();
    c.count()
}
"#,
    );
    let a = run(&t);
    let called: Vec<&str> = a
        .edges
        .iter()
        .filter(|e| e.ty == "CALLS" && e.src.starts_with("k::over_"))
        .map(|e| e.dst.as_str())
        .collect();
    for one in ["Iterator::map", "Iterator::collect", "Iterator::count"] {
        assert!(called.contains(&one), "{called:?}");
    }
    for scattered in ["Range::map", "Range::collect", "Chars::count"] {
        assert!(!called.contains(&scattered), "{called:?}");
        assert!(!keys(&a).contains(&scattered), "minted `{scattered}`");
    }
}

/// A channel's two halves are typed by the pair its constructor returns.
///
/// `let (tx, rx) = mpsc::channel();` is how every Rust channel starts, and
/// `channel()` is external — no declared return, so the tuple had no element
/// types, so both halves were untyped and every `tx.send(v)` and `rx.recv()`
/// in a tree fell into the unresolved ledger. A whole repository's message
/// passing was invisible for want of one return type.
#[test]
fn channel_halves_type_their_sends_and_receives() {
    let t = Tree::new("chan-pair");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        r#"
use std::sync::mpsc;

pub struct Job;

pub fn std_pair() {
    let (tx, rx) = mpsc::channel();
    tx.send(Job);
    rx.recv();
}

pub fn tokio_pair() {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    tx.send(Job);
    rx.recv();
}

pub fn crossbeam_pair() {
    let (s, r) = crossbeam::channel::unbounded::<Job>();
    s.send(Job);
    r.recv();
}
"#,
    );
    let a = run(&t);
    for (caller, sender, receiver) in [
        (
            "k::std_pair",
            "std::sync::mpsc::Sender",
            "std::sync::mpsc::Receiver",
        ),
        (
            "k::tokio_pair",
            "tokio::sync::mpsc::UnboundedSender",
            "tokio::sync::mpsc::UnboundedReceiver",
        ),
        (
            "k::crossbeam_pair",
            "crossbeam::channel::Sender",
            "crossbeam::channel::Receiver",
        ),
    ] {
        assert!(
            has_edge(&a, caller, "CALLS", &format!("{sender}::send")),
            "{caller}: no send — {:?}",
            a.edges
                .iter()
                .filter(|e| e.src == caller)
                .map(|e| e.dst.as_str())
                .collect::<Vec<_>>()
        );
        assert!(
            has_edge(&a, caller, "CALLS", &format!("{receiver}::recv")),
            "{caller}: no recv"
        );
    }
    // The pair is keyed by the constructor's own module, so it is the same
    // node an annotated `let rx: mpsc::UnboundedReceiver<T>` resolves to
    // rather than a second spelling of one type.
    assert!(!keys(&a).contains(&"Sender::send"), "{:?}", keys(&a));
    every_edge_has_endpoints(&a);
}

/// A turbofish is the call's type argument, not part of its name.
#[test]
fn a_turbofish_splits_off_the_path_it_qualifies() {
    assert_eq!(split_turbofish("mpsc::channel"), ("mpsc::channel", None));
    assert_eq!(
        split_turbofish("mpsc::channel::<Job>"),
        ("mpsc::channel", Some("Job"))
    );
    // Only a top-level comma splits: the first argument may carry its own.
    assert_eq!(
        split_turbofish("f::<HashMap<K, V>>"),
        ("f", Some("HashMap<K, V>"))
    );
    assert_eq!(split_turbofish("f::<A, B>"), ("f", Some("A")));
}

/// A renamed import is in scope under the name it introduced, and only that.
///
/// The index was keyed by the last segment of the path instead, so
/// `use tokio::sync::mpsc as tmpsc;` put `mpsc` in scope and left `tmpsc`
/// out — the one name the file can actually write. Every `tmpsc::f()` in the
/// tree resolved to nothing, and a `mpsc::f()` nobody wrote would have
/// resolved to something.
#[test]
fn a_renamed_import_is_in_scope_under_the_name_it_introduced() {
    let t = Tree::new("import-rename");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        r#"
use std::sync::mpsc as chan;
use std::collections::HashMap as Map;

pub struct Job;

pub fn renamed() {
    let (tx, rx) = chan::channel();
    tx.send(Job);
    rx.recv();
    let m: Map<String, i64> = Map::new();
    m.len();
}
"#,
    );
    let a = run(&t);
    let called: Vec<&str> = a
        .edges
        .iter()
        .filter(|e| e.src == "k::renamed" && e.ty == "CALLS")
        .map(|e| e.dst.as_str())
        .collect();
    for one in [
        "std::sync::mpsc::channel",
        "std::sync::mpsc::Sender::send",
        "std::sync::mpsc::Receiver::recv",
        "std::collections::HashMap::len",
    ] {
        assert!(called.contains(&one), "expected {one} in {called:?}");
    }
    // The old spelling was never in scope and must not be invented.
    assert!(!keys(&a).contains(&"chan::channel"), "{:?}", keys(&a));
}

// ---- P0 eval harness: known resolution gaps, un-ignored as their phase
// lands. `just eval` runs these; CI's normal `cargo test` skips them.

/// The benchmark's false self-edge, as a fixture: a path-qualified call
/// (`node::remove(...)`) to a free function behind a `pub use` re-export,
/// while a same-module METHOD shares the simple name. The call must bind to
/// the free function — or to nothing — never to the method.
#[test]
fn qualified_call_through_reexport_never_binds_to_local_method() {
    let t = Tree::new("p0-qualified");
    t.write(
        "Cargo.toml",
        "[package]\nname = \"k\"\nversion = \"0.0.0\"\n",
    );
    t.write("src/lib.rs", "pub mod store;\npub mod api;\n");
    t.write(
        "src/store.rs",
        "mod inner { pub fn remove(_: u64) {} }\npub use inner::remove;\n",
    );
    t.write(
        "src/api.rs",
        "use crate::store;\npub struct Txn;\nimpl Txn {\n    pub fn remove(&mut self, id: u64) {\n        store::remove(id)\n    }\n}\n",
    );
    let a = run(&t);
    assert!(
        !has_edge(&a, "k::api::Txn::remove", "CALLS", "k::api::Txn::remove"),
        "a path-qualified call must never produce a self-edge to a same-named method"
    );
    assert!(
        has_edge(
            &a,
            "k::api::Txn::remove",
            "CALLS",
            "k::store::inner::remove"
        ),
        "the qualifier names the re-exported free function"
    );
}

/// The benchmark's missing integration-test callers, as a fixture: a
/// receiver whose type comes from an initializer's declared return type
/// (`let mut txn = open();` → `Txn`), calling a method from a module where
/// the simple name is NOT unique in scope. Requires receiver typing.
#[test]
fn receiver_type_from_initializer_return_resolves_method_calls() {
    let t = Tree::new("p0-receiver");
    t.write(
        "Cargo.toml",
        "[package]\nname = \"k\"\nversion = \"0.0.0\"\n",
    );
    t.write(
        "src/lib.rs",
        "pub mod db;\npub mod other;\npub mod caller;\n",
    );
    t.write(
        "src/db.rs",
        "pub struct Txn;\npub fn open() -> Txn { Txn }\nimpl Txn {\n    pub fn remove(&mut self, _: u64) {}\n}\n",
    );
    t.write(
        "src/other.rs",
        "pub struct Store;\nimpl Store {\n    pub fn remove(&mut self, _: u64) {}\n}\n",
    );
    t.write(
        "src/caller.rs",
        "use crate::db::open;\npub fn go() {\n    let mut txn = open();\n    txn.remove(7);\n}\n",
    );
    let a = run(&t);
    assert!(
        has_edge(&a, "k::caller::go", "CALLS", "k::db::Txn::remove"),
        "the initializer's return type names the receiver: open() -> Txn"
    );
    assert!(
        !has_edge(&a, "k::caller::go", "CALLS", "k::other::Store::remove"),
        "and never the same-named method on an unrelated type"
    );
}

/// The shape integration tests actually write: a chain of locals, each typed
/// by the previous one's declared return, with `?` and `.unwrap()` peeling
/// `Result` along the way — `open()?` → `db.plane()` → `.write().unwrap()`.
#[test]
fn receiver_chain_through_wrapped_returns_resolves_method_calls() {
    let t = Tree::new("p3-chain");
    t.write(
        "Cargo.toml",
        "[package]\nname = \"k\"\nversion = \"0.0.0\"\n",
    );
    t.write("src/lib.rs", "pub mod db;\npub mod caller;\n");
    t.write(
        "src/db.rs",
        concat!(
            "pub struct Db;\npub struct Plane;\npub struct Txn;\n",
            "pub fn open() -> Result<Db, ()> { Ok(Db) }\n",
            "impl Db {\n    pub fn plane(&self) -> Plane { Plane }\n}\n",
            "impl Plane {\n    pub fn write(&self) -> Result<Txn, ()> { Ok(Txn) }\n}\n",
            "impl Txn {\n    pub fn remove(&mut self, _: u64) {}\n}\n",
        ),
    );
    t.write(
        "src/caller.rs",
        concat!(
            "use crate::db::open;\n",
            "pub fn go() -> Result<(), ()> {\n",
            "    let db = open()?;\n",
            "    let plane = db.plane();\n",
            "    let mut txn = plane.write().unwrap();\n",
            "    txn.remove(7);\n    Ok(())\n}\n",
        ),
    );
    let a = run(&t);
    assert!(
        has_edge(&a, "k::caller::go", "CALLS", "k::db::Txn::remove"),
        "each hop of the chain is a declared fact: {:?}",
        a.edges
            .iter()
            .filter(|e| e.ty == "CALLS")
            .map(|e| (e.src.as_str(), e.dst.as_str()))
            .collect::<Vec<_>>()
    );
}

// ---- baseline ports: cases mined from codegraph + codebase-memory-mcp ----
// (scratchpad/baseline/*.md holds the full inventories and the gap matrix.)

/// codegraph: `users::router()` in a parent module resolves SELF-relative to
/// the child module's fn — and a colliding leaf name never crosses files.
#[test]
fn self_relative_submodule_calls_disambiguate_colliding_leaves() {
    let t = Tree::new("bl-selfrel");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", "pub mod http;\n")
        .write(
            "src/http/mod.rs",
            "pub mod users;\npub mod profiles;\npub fn api_router() { users::router(); profiles::router(); }\n",
        )
        .write("src/http/users.rs", "pub fn router() -> i32 { 1 }\n")
        .write("src/http/profiles.rs", "pub fn router() -> i32 { 2 }\n");
    let a = run(&t);
    assert!(has_edge(
        &a,
        "k::http::api_router",
        "CALLS",
        "k::http::users::router"
    ));
    assert!(has_edge(
        &a,
        "k::http::api_router",
        "CALLS",
        "k::http::profiles::router"
    ));
}

/// codegraph: a 3-segment path through an imported module reaches the leaf.
#[test]
fn three_segment_path_through_imported_module_resolves() {
    let t = Tree::new("bl-3seg");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", "pub mod routes;\npub mod database;\n")
        .write("src/database/mod.rs", "pub mod profiles;\n")
        .write("src/database/profiles.rs", "pub fn find(id: i32) -> i32 { id }\n")
        .write(
            "src/routes/mod.rs",
            "use crate::database;\npub fn get_profile(id: i32) -> i32 { database::profiles::find(id) }\n",
        );
    let a = run(&t);
    assert!(has_edge(
        &a,
        "k::routes::get_profile",
        "CALLS",
        "k::database::profiles::find"
    ));
}

/// codegraph: literal receivers (`"lit".len()`, `5.0f64.floor()`) must never
/// bind to a same-named local method — they go to the ledger, not to a guess.
#[test]
fn literal_receivers_never_bind_to_local_methods() {
    let t = Tree::new("bl-litrecv");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        "pub struct S;\nimpl S { pub fn len(&self) -> usize { 0 } }\npub fn run() { let _ = \"lit\".len(); let _ = 5.0f64.floor(); }\n",
    );
    let a = run(&t);
    assert!(
        !has_edge(&a, "k::run", "CALLS", "k::S::len"),
        "a literal's method is not the local type's"
    );
}

/// codegraph: a glob use emits no binding edges — it stays in the imports
/// property as written, and never invents a target.
#[test]
fn glob_use_emits_no_import_edge() {
    let t = Tree::new("bl-glob");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", "pub mod a;\npub mod b;\n")
        .write("src/a.rs", "pub fn thing() {}\n")
        .write("src/b.rs", "use crate::a::*;\npub fn go() {}\n");
    let a = run(&t);
    assert!(
        !a.edges.iter().any(|e| e.src == "k::b" && e.ty == "IMPORTS"),
        "a glob names no single target"
    );
}

/// cbm: qualified method call in UFCS position (`S::hi(&s)`) resolves to the
/// declared method like any path call.
#[test]
fn ufcs_qualified_method_call_resolves() {
    let t = Tree::new("bl-ufcs");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        "pub struct S;\nimpl S { pub fn hi(&self) -> i32 { 1 } }\npub fn run(s: &S) -> i32 { S::hi(s) }\n",
    );
    let a = run(&t);
    assert!(has_edge(&a, "k::run", "CALLS", "k::S::hi"));
}

/// cbm/codegraph decoy discipline: two same-named methods, each caller typed
/// by its parameter — exactly its own, never the other (Apple/Orange shape).
#[test]
fn typed_params_never_cross_attribute_same_named_methods() {
    let t = Tree::new("bl-appleorange");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        concat!(
            "pub struct Apple;\nimpl Apple { pub fn name(&self) -> i32 { 1 } }\n",
            "pub struct Orange;\nimpl Orange { pub fn name(&self) -> i32 { 2 } }\n",
            "pub fn pick_apple(a: &Apple) -> i32 { a.name() }\n",
            "pub fn pick_orange(o: &Orange) -> i32 { o.name() }\n",
        ),
    );
    let a = run(&t);
    assert!(has_edge(&a, "k::pick_apple", "CALLS", "k::Apple::name"));
    assert!(has_edge(&a, "k::pick_orange", "CALLS", "k::Orange::name"));
    assert!(!has_edge(&a, "k::pick_apple", "CALLS", "k::Orange::name"));
    assert!(!has_edge(&a, "k::pick_orange", "CALLS", "k::Apple::name"));
}

// ---- baseline Tier B: conformance, fields, instantiation, supertraits ----

/// codegraph: a chained method found only on an implemented trait's default
/// resolves through conformance — and a same-named decoy method never wins.
#[test]
fn trait_default_methods_resolve_through_conformance() {
    let t = Tree::new("bl-traitdefault");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        concat!(
            "pub struct Foo;\nimpl Foo { pub fn new() -> Foo { Foo } }\n",
            "pub struct Decoy;\nimpl Decoy { pub fn draw(&self) {} }\n",
            "pub trait Drawable { fn draw(&self) {} }\nimpl Drawable for Foo {}\n",
            "pub fn caller() { let f = Foo::new(); f.draw(); }\n",
        ),
    );
    let a = run(&t);
    assert!(
        has_edge(&a, "k::caller", "CALLS", "k::Drawable::draw"),
        "conformance reaches the trait's default method: {:?}",
        a.edges
            .iter()
            .filter(|e| e.ty == "CALLS")
            .map(|e| (e.src.as_str(), e.dst.as_str()))
            .collect::<Vec<_>>()
    );
    assert!(!has_edge(&a, "k::caller", "CALLS", "k::Decoy::draw"));
}

/// A trait method the impl overrides resolves to the impl's own
/// `<T as Trait>::m` — the key no call site can spell.
#[test]
fn overridden_trait_methods_resolve_to_the_impl() {
    let t = Tree::new("bl-traitimpl");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        concat!(
            "pub struct Foo;\n",
            "pub trait Render { fn render(&self) -> i32; }\n",
            "impl Render for Foo { fn render(&self) -> i32 { 1 } }\n",
            "pub fn caller(f: &Foo) -> i32 { f.render() }\n",
        ),
    );
    let a = run(&t);
    assert!(
        has_edge(&a, "k::caller", "CALLS", "<k::Foo as Render>::render"),
        "{:?}",
        a.edges
            .iter()
            .filter(|e| e.ty == "CALLS")
            .map(|e| e.dst.as_str())
            .collect::<Vec<_>>()
    );
}

/// codegraph #1276-shape: a dotted receiver walks declared field types —
/// `o.inner.ping()` and `self.inner.ping()` both resolve.
#[test]
fn field_chains_type_receivers() {
    let t = Tree::new("bl-fieldchain");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        concat!(
            "pub struct Inner;\nimpl Inner { pub fn ping(&self) -> i32 { 1 } }\n",
            "pub struct Outer { pub inner: Inner }\n",
            "impl Outer { pub fn go(&self) -> i32 { self.inner.ping() } }\n",
            "pub fn run(o: &Outer) -> i32 { o.inner.ping() }\n",
        ),
    );
    let a = run(&t);
    assert!(
        has_edge(&a, "k::run", "CALLS", "k::Inner::ping"),
        "{:?}",
        a.edges
    );
    assert!(has_edge(&a, "k::Outer::go", "CALLS", "k::Inner::ping"));
}

/// codegraph: a struct literal is a use of the type — cross-module, through
/// the import, as an INSTANTIATES fact.
#[test]
fn struct_literals_are_instantiation_facts() {
    let t = Tree::new("bl-instantiate");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", "pub mod types;\npub mod consumer;\n")
        .write("src/types.rs", "pub struct Widget { pub n: i32 }\n")
        .write(
            "src/consumer.rs",
            "use crate::types::Widget;\npub fn build() -> Widget { Widget { n: 1 } }\n",
        );
    let a = run(&t);
    assert!(
        has_edge(&a, "k::consumer::build", "INSTANTIATES", "k::types::Widget"),
        "{:?}",
        a.edges
            .iter()
            .filter(|e| e.ty == "INSTANTIATES")
            .map(|e| (e.src.as_str(), e.dst.as_str()))
            .collect::<Vec<_>>()
    );
}

/// codegraph: `pub union` is a first-class type — extracted, implementable,
/// its methods resolvable like any other.
#[test]
fn unions_are_types_with_methods() {
    let t = Tree::new("bl-union");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        concat!(
            "pub union Reg { pub raw: u32 }\n",
            "impl Reg { pub fn describe(&self) -> u32 { unsafe { self.raw } } }\n",
            "pub fn run(r: &Reg) -> u32 { r.describe() }\n",
        ),
    );
    let a = run(&t);
    let union = a
        .nodes
        .iter()
        .find(|n| n.key == "k::Reg")
        .expect("union node");
    assert_eq!(union.label, "Union");
    assert!(has_edge(&a, "k::run", "CALLS", "k::Reg::describe"));
}

/// codegraph: `trait Error: Display` — the supertrait is an EXTENDS fact.
#[test]
fn supertraits_become_extends_edges() {
    let t = Tree::new("bl-supertrait");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        "pub trait Display2 {}\npub trait Error2: Display2 { fn describe(&self) -> i32 { 0 } }\n",
    );
    let a = run(&t);
    assert!(
        has_edge(&a, "k::Error2", "EXTENDS", "k::Display2"),
        "{:?}",
        a.edges
            .iter()
            .filter(|e| e.ty == "EXTENDS")
            .map(|e| (e.src.as_str(), e.dst.as_str()))
            .collect::<Vec<_>>()
    );
}

/// codegraph kernel-parity shape: an impl written above its struct resolves
/// the same as one below — assemble sees the whole batch.
#[test]
fn impl_before_struct_is_order_independent() {
    let t = Tree::new("bl-implfirst");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        "impl Later { pub fn go(&self) -> i32 { 1 } }\npub struct Later;\npub fn run(l: &Later) -> i32 { l.go() }\n",
    );
    let a = run(&t);
    assert!(has_edge(&a, "k::Later", "HAS_METHOD", "k::Later::go"));
    assert!(has_edge(&a, "k::run", "CALLS", "k::Later::go"));
}

/// C4 (narrow): a bare in-tree function passed as an argument is a
/// REFERENCES fact — argument position only, never a self-loop.
#[test]
fn functions_passed_as_values_become_references() {
    let t = Tree::new("bl-fnref");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        concat!(
            "pub fn handler() {}\n",
            "pub fn register(_cb: fn()) {}\n",
            "pub fn wire() { register(handler); }\n",
            "pub fn retry() { register(retry); }\n",
        ),
    );
    let a = run(&t);
    assert!(
        a.edges
            .iter()
            .any(|e| e.ty == "REFERENCES" && e.src == "k::wire" && e.dst == "k::handler"),
        "{:?}",
        a.edges
            .iter()
            .filter(|e| e.ty == "REFERENCES")
            .collect::<Vec<_>>()
    );
    assert!(
        !a.edges
            .iter()
            .any(|e| e.ty == "REFERENCES" && e.src == "k::retry" && e.dst == "k::retry"),
        "no self-loop"
    );
}

/// The model_pbt gap, as a fixture: a closure's parameter typed by the
/// callee's declared `Fn(...)` bound — impl-trait, generic-with-bound, and
/// where-clause forms all state the type.
#[test]
fn closure_params_type_through_the_callees_declared_bound() {
    let t = Tree::new("bl-closure");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        concat!(
            "pub struct Plane;\nimpl Plane { pub fn wipe(&self) {} }\n",
            "pub struct Harness;\n",
            "impl Harness {\n",
            "    pub fn on_all(&self, f: impl Fn(&Plane)) { let _ = f; }\n",
            "    pub fn apply(&self) { self.on_all(|p| p.wipe()); }\n",
            "}\n",
            "pub fn each<F: Fn(&Plane)>(f: F) { let _ = f; }\n",
            "pub fn run() { each(|p| p.wipe()); }\n",
            "pub fn each_where<F>(f: F) where F: Fn(&Plane) { let _ = f; }\n",
            "pub fn run_where() { each_where(|p| p.wipe()); }\n",
        ),
    );
    let a = run(&t);
    assert!(
        has_edge(&a, "k::Harness::apply", "CALLS", "k::Plane::wipe"),
        "impl-Fn bound types the closure param: {:?}",
        a.edges
            .iter()
            .filter(|e| e.ty == "CALLS")
            .map(|e| (e.src.as_str(), e.dst.as_str()))
            .collect::<Vec<_>>()
    );
    assert!(has_edge(&a, "k::run", "CALLS", "k::Plane::wipe"));
    assert!(has_edge(&a, "k::run_where", "CALLS", "k::Plane::wipe"));
}

/// A receiver whose type the body states — a parameter, an annotation, a
/// constructor path — resolves into std and other crates too: the target is
/// an external stand-in keyed `Type::method`, labelled as the method a call
/// site proved it to be, and the edge says the receiver's type is what
/// resolved it.
#[test]
fn a_typed_receiver_resolves_into_std() {
    let t = Tree::new("ext-recv");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        concat!(
            "use std::collections::HashMap;\n",
            "pub fn go(s: &str, v: Vec<u8>) {\n",
            "    s.trim();\n",
            "    v.len();\n",
            "    let mut w = Vec::new();\n",
            "    w.push(1);\n",
            "    let n = String::from(\"x\");\n",
            "    n.as_str();\n",
            "    let m: HashMap<u8, u8> = HashMap::new();\n",
            "    m.get(&1);\n",
            "    \"lit\".to_string();\n",
            "}\n",
        ),
    );
    let out = run(&t);
    let calls: Vec<&str> = out
        .edges
        .iter()
        .filter(|e| e.ty == "CALLS" && e.src == "k::go")
        .map(|e| e.dst.as_str())
        .collect();
    for target in [
        "str::trim",
        "Vec::len",
        "Vec::push",
        "String::as_str",
        // Expanded through the file's `use`, as a path call would be.
        "std::collections::HashMap::get",
        // A blanket trait's method is the trait's, wherever it is called.
        "ToString::to_string",
    ] {
        assert!(calls.contains(&target), "{target} missing from {calls:?}");
        let node = out.nodes.iter().find(|n| n.key == target).unwrap();
        assert_eq!(node.label, "Method", "{target}");
        assert_eq!(node.extra_labels, vec!["External".to_string()], "{target}");
    }
    let edge = out
        .edges
        .iter()
        .find(|e| e.src == "k::go" && e.dst == "Vec::push")
        .unwrap();
    assert_eq!(
        edge.props.get("_resolved_by"),
        Some(&Value::String("external-receiver".into()))
    );
    // The `String::from` constructor is itself a path call, recorded as before.
    assert!(calls.contains(&"String::from"), "{calls:?}");
    // Nothing above went to the ledger.
    assert!(
        !calls.iter().any(|c| c.starts_with("?::")),
        "every receiver was typed: {calls:?}"
    );
    every_edge_has_endpoints(&out);
}

/// A chain is typed hop by hop — `v.iter().map(…).collect()` — with each
/// hop into std resolved to where std declares it: `Vec::iter`, then
/// `Iterator::map` and `Iterator::collect`, since the adapter types are
/// std's business and never spelled by the source.
#[test]
fn a_method_chain_is_typed_hop_by_hop() {
    let t = Tree::new("ext-chain");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        concat!(
            "pub struct Bag { pub items: Vec<u8> }\n",
            "impl Bag {\n",
            "    pub fn total(&self) -> usize { self.items.iter().map(|x| *x as usize).sum() }\n",
            "    pub fn names(&self) -> Vec<String> { self.items.iter().map(|x| x.to_string()).collect() }\n",
            "}\n",
            "pub fn count(b: &Bag) -> usize { b.names().len() }\n",
        ),
    );
    let out = run(&t);
    let calls = |src: &str| -> Vec<String> {
        out.edges
            .iter()
            .filter(|e| e.ty == "CALLS" && e.src == src)
            .map(|e| e.dst.clone())
            .collect()
    };
    let total = calls("k::Bag::total");
    for target in ["Vec::iter", "Iterator::map", "Iterator::sum"] {
        assert!(
            total.contains(&target.to_string()),
            "{target} missing from {total:?}"
        );
    }
    let names = calls("k::Bag::names");
    assert!(
        names.contains(&"Iterator::collect".to_string()),
        "{names:?}"
    );
    // A declared method's stated return types the next hop: `names()` is a
    // `Vec<String>`, so `.len()` on it is `Vec::len`.
    let count = calls("k::count");
    assert!(count.contains(&"k::Bag::names".to_string()), "{count:?}");
    assert!(count.contains(&"Vec::len".to_string()), "{count:?}");
    every_edge_has_endpoints(&out);
}

/// A bounded generic parameter is its bound: `x: T` with `T: Tr`, `y: &impl
/// Tr` and `b: Box<dyn Tr>` all answer `.m()` with `Tr::m`, since every
/// method called on such a value is the trait's. An unbounded parameter
/// states no type, so `u.m()` stays in the ledger rather than becoming a
/// `U::m` that exists nowhere; likewise a receiver nothing states the type
/// of.
#[test]
fn bounded_generics_are_their_trait_and_unbounded_ones_stay_in_the_ledger() {
    let t = Tree::new("ext-generic");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        concat!(
            "pub trait Tr { fn m(&self); }\n",
            "pub fn go<T: Tr>(x: T, y: &impl Tr, b: Box<dyn Tr>) { x.m(); y.m(); b.m(); }\n",
            "pub fn wc<T>(x: T) where T: Tr { x.m(); }\n",
            "pub struct W<U>(U);\n",
            "impl<U> W<U> { pub fn inner(&self, u: U) { u.m(); } }\n",
            "pub fn other() { let z = unknown(); z.m(); }\n",
        ),
    );
    let out = run(&t);
    for src in ["k::go", "k::wc"] {
        assert!(
            has_edge(&out, src, "CALLS", "k::Tr::m"),
            "{src}: {:?}",
            out.edges
        );
    }
    let mut ledger: Vec<&str> = out
        .edges
        .iter()
        .filter(|e| e.ty == "CALLS" && e.dst.starts_with("?::"))
        .map(|e| e.src.as_str())
        .collect();
    ledger.sort();
    assert_eq!(ledger, vec!["k::W::inner", "k::other"], "{ledger:?}");
    assert!(
        !out.nodes.iter().any(|n| n.key == "T::m" || n.key == "U::m"),
        "no method node for a type parameter"
    );
    every_edge_has_endpoints(&out);
}

/// A declared type still wins over an external one with the same short
/// name, and a declared method still beats std's table: `.clone()` on a type
/// that declares one lands on the declaration.
#[test]
fn declared_types_and_methods_beat_the_std_table() {
    let t = Tree::new("ext-declared");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        concat!(
            "pub struct Vec;\nimpl Vec { pub fn len(&self) -> usize { 0 } }\n",
            "pub struct Txn;\nimpl Txn { pub fn clone(&self) -> Txn { Txn } pub fn commit(&self) {} }\n",
            "pub fn go(v: &Vec, t: &Txn) { v.len(); t.clone().commit(); }\n",
        ),
    );
    let out = run(&t);
    assert!(has_edge(&out, "k::go", "CALLS", "k::Vec::len"));
    assert!(has_edge(&out, "k::go", "CALLS", "k::Txn::clone"));
    assert!(has_edge(&out, "k::go", "CALLS", "k::Txn::commit"));
    assert!(
        !out.nodes
            .iter()
            .any(|n| n.key == "Vec::len" || n.key == "Txn::clone"),
        "nothing external was minted for a declared type"
    );
}

/// `Box`, `Arc` and `Rc` are looked through to what they point at, as method
/// resolution does by deref — `Arc<Mutex<T>>` answers `.lock()` as `Mutex`,
/// and `Box<dyn Tr>` as `Tr`.
#[test]
fn smart_pointers_type_as_their_pointee() {
    let t = Tree::new("ext-deref");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        concat!(
            "use std::sync::{Arc, Mutex};\n",
            "pub trait Tr { fn m(&self); }\n",
            "pub fn go(a: Arc<Mutex<u8>>, b: Box<dyn Tr>) { a.lock(); b.m(); }\n",
        ),
    );
    let out = run(&t);
    assert!(
        has_edge(&out, "k::go", "CALLS", "std::sync::Mutex::lock"),
        "{:?}",
        out.edges
    );
    assert!(
        has_edge(&out, "k::go", "CALLS", "k::Tr::m"),
        "a `Box<dyn Tr>` receiver answers with the trait's method"
    );
}

/// Resolution over a real tree, for eyeballing a change to receiver typing
/// against a codebase rather than a fixture: `DRSG_EVAL_ROOT=<dir> cargo test
/// -- --ignored eval_resolution --nocapture`. Prints how calls resolved and
/// what still leads the ledger; asserts nothing beyond the graph being
/// well-formed.
#[test]
#[ignore]
fn eval_resolution_over_a_real_tree() {
    let Ok(root) = std::env::var("DRSG_EVAL_ROOT") else {
        eprintln!("DRSG_EVAL_ROOT not set — nothing to evaluate");
        return;
    };
    let out = run_files(&TestFiles::rooted(root));
    let mut by_strategy: std::collections::BTreeMap<String, usize> = Default::default();
    let mut ledger: std::collections::BTreeMap<String, usize> = Default::default();
    for e in out.edges.iter().filter(|e| e.ty == "CALLS") {
        let how = match e.props.get("_resolved_by") {
            Some(Value::String(s)) => s.clone(),
            _ => "unstamped".into(),
        };
        *by_strategy.entry(how).or_default() += 1;
        if let Some(name) = e
            .dst
            .strip_prefix("?::")
            .and_then(|k| k.rsplit("::").next())
        {
            *ledger.entry(name.to_string()).or_default() += 1;
        }
    }
    let mut top: Vec<_> = ledger.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    // Why each ledger entry could not be typed, by the shape of its receiver.
    let mut why: std::collections::BTreeMap<String, usize> = Default::default();
    let mut heads: std::collections::BTreeMap<String, usize> = Default::default();
    let mut breaks: std::collections::BTreeMap<String, usize> = Default::default();
    for e in out.edges.iter().filter(|e| e.dst.starts_with("?::")) {
        let recv = match e.props.get("_recv") {
            Some(Value::String(r)) => r.as_str(),
            _ => {
                *why.entry("no chain (index/await/closure/block/number)".into())
                    .or_default() += 1;
                continue;
            }
        };
        let hops: Vec<&str> = recv.split('.').collect();
        let head = hops[0];
        let kind = if head == "self" {
            "self.<field…> — a field's type unknown or external"
        } else if head.starts_with('#') {
            "literal head"
        } else if head.ends_with("()") && head.contains("::") {
            "path call — no return fact"
        } else if head.ends_with("()") {
            "bare call — no return fact"
        } else if hops.len() == 1 {
            "bare local — untyped binding"
        } else {
            "local chain — untyped binding or a hop with no return fact"
        };
        *why.entry(kind.into()).or_default() += 1;
        if hops.len() == 1 && !head.ends_with("()") {
            *heads.entry(head.to_string()).or_default() += 1;
        }
        if let Some(last) = hops.iter().rev().find(|h| h.ends_with("()")) {
            *breaks.entry((*last).to_string()).or_default() += 1;
        }
    }
    if let Ok(needle) = std::env::var("DRSG_EVAL_SHOW") {
        eprintln!("ledger edges whose receiver or name contains `{needle}`:");
        for e in out
            .edges
            .iter()
            .filter(|e| e.dst.starts_with("?::"))
            .filter(|e| {
                e.dst.contains(&needle)
                    || matches!(e.props.get("_recv"), Some(Value::String(r)) if r.contains(&needle))
                    || matches!(e.props.get("_reason"), Some(Value::String(r)) if r.contains(&needle))
            })
            .take(40)
        {
            eprintln!(
                "  {}  .{}  recv={:?}\n      {:?}",
                e.src,
                e.dst.rsplit("::").next().unwrap_or(""),
                e.props.get("_recv"),
                e.props.get("_reason")
            );
        }
    }
    eprintln!("why unresolved:");
    for (k, n) in &why {
        eprintln!("  {n:5}  {k}");
    }
    // The parser's own account, with names blanked so the shapes group.
    let mut reasons: std::collections::BTreeMap<String, usize> = Default::default();
    for e in out.edges.iter().filter(|e| e.dst.starts_with("?::")) {
        if let Some(Value::String(r)) = e.props.get("_reason") {
            let shape: String = r
                .split('`')
                .enumerate()
                .map(|(i, part)| if i % 2 == 1 { "_" } else { part })
                .collect();
            *reasons.entry(shape).or_default() += 1;
        }
    }
    let mut reasons: Vec<_> = reasons.into_iter().collect();
    reasons.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    eprintln!("reasons:");
    for (r, n) in reasons.iter().take(12) {
        eprintln!("  {n:5}  {r}");
    }
    let mut heads: Vec<_> = heads.into_iter().collect();
    heads.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    eprintln!("untyped bare locals:");
    for (name, n) in heads.iter().take(25) {
        eprintln!("  {n:5}  {name}");
    }
    let mut breaks: Vec<_> = breaks.into_iter().collect();
    breaks.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    eprintln!("last call hop in an untyped chain:");
    for (name, n) in breaks.iter().take(25) {
        eprintln!("  {n:5}  {name}");
    }
    eprintln!("nodes {} edges {}", out.nodes.len(), out.edges.len());
    eprintln!(
        "UnresolvedRef nodes {}",
        out.nodes
            .iter()
            .filter(|n| n.label == "UnresolvedRef")
            .count()
    );
    for (how, n) in &by_strategy {
        eprintln!("  CALLS by {how}: {n}");
    }
    eprintln!("ledger leaders:");
    for (name, n) in top.iter().take(25) {
        eprintln!("  {n:5}  {name}");
    }
    for note in &out.notes {
        eprintln!("note: {note}");
    }
    every_edge_has_endpoints(&out);
}

/// A hop into std that returns an `Option`, a `Result` or an iterator is a
/// fact the parser carries, so the chain goes on: `map.get(k)` is an
/// `Option`, and `.map(…)` on it is `Option::map`, `.unwrap()` is
/// `Option::unwrap`. A method no declaration and no table knows stops the
/// chain — `Type::method` is still recorded, since the type is known, but
/// nothing after it is.
#[test]
fn std_returns_carry_a_chain_through_option_and_result() {
    let t = Tree::new("ext-std-returns");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        concat!(
            "use std::collections::HashMap;\n",
            "#[derive(Clone)]\npub struct Txn;\n",
            "pub fn open() -> Result<Txn, ()> { Ok(Txn) }\n",
            "pub fn go(m: &HashMap<u8, u8>, s: &str, t: &Txn) {\n",
            "    m.get(&1).map(|x| *x).unwrap();\n",
            "    s.parse::<u8>().map_err(|e| e).unwrap_or_default();\n",
            "    let r = open();\n",
            "    r.is_ok();\n",
            "    let c = t.clone();\n",
            "    c.clone();\n",
            "    s.chars().rev().count();\n",
            "    s.find('x').unwrap_or(0).max(1);\n",
            "}\n",
        ),
    );
    let out = run(&t);
    let calls: Vec<&str> = out
        .edges
        .iter()
        .filter(|e| e.ty == "CALLS" && e.src == "k::go")
        .map(|e| e.dst.as_str())
        .collect();
    for target in [
        "std::collections::HashMap::get",
        "Option::map",
        "Option::unwrap",
        "str::parse",
        "Result::map_err",
        "Result::unwrap_or_default",
        // An un-peeled declared `Result<Txn>` return is a `Result`.
        "Result::is_ok",
        // `#[derive(Clone)]` writes no `fn clone`: the call is the trait's,
        // and its result is the type again, so the second `.clone()` is too.
        "Clone::clone",
        "str::chars",
        "Iterator::rev",
        "Iterator::count",
        "str::find",
        "Option::unwrap_or",
    ] {
        assert!(calls.contains(&target), "{target} missing from {calls:?}");
    }
    let clone = out
        .edges
        .iter()
        .find(|e| e.src == "k::go" && e.dst == "Clone::clone")
        .unwrap();
    assert_eq!(
        clone.props.get("_resolved_by"),
        Some(&Value::String("std-trait".into()))
    );
    assert_eq!(
        clone.props.get("_confidence"),
        Some(&Value::String("medium".into()))
    );
    // `find` is an `Option<usize>`, `unwrap_or(0)` its payload, and `max`
    // on a `usize` is `Ord`'s.
    assert!(calls.contains(&"Ord::max"), "{calls:?}");
    every_edge_has_endpoints(&out);
}

/// Type arguments carry through every binding a body writes: `for n in
/// &v` is a `Node` when `v: Vec<Node>`, so is `|n| …` handed to
/// `v.iter().map(…)`, `Some(n)` of a `Option<Node>`, `(i, n)` of an
/// `enumerate()`, `Node { key, .. }` of a match arm; a `type` alias is its
/// target with the parameters filled in; a turbofish says what `collect`
/// makes; and a closure whose body is a chain says what `map` yields.
#[test]
fn bindings_and_closures_type_through_generics() {
    let t = Tree::new("ext-generics");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        concat!(
            "use std::collections::HashMap;\n",
            "pub struct Node { pub key: String, pub line: u64 }\n",
            "impl Node { pub fn name(&self) -> String { self.key.clone() } }\n",
            "pub struct Error;\n",
            "pub type Result<T> = std::result::Result<T, Error>;\n",
            "pub type Props = HashMap<String, Node>;\n",
            "pub fn open() -> Result<Node> { Err(Error) }\n",
            "pub fn go(v: Vec<Node>, m: &Props, o: Option<Node>) -> Result<()> {\n",
            "    for n in &v { n.name(); }\n",
            "    for (i, n) in v.iter().enumerate() { n.line; i.checked_add(1); }\n",
            "    for (k, n) in m { k.len(); n.name(); }\n",
            "    if let Some(n) = o { n.name(); }\n",
            "    match m.get(\"x\") { Some(Node { key, .. }) => { key.trim(); } None => {} }\n",
            "    let names: Vec<String> = v.iter().map(|n| n.name()).collect();\n",
            "    names.first();\n",
            "    let lens = v.iter().map(|n| n.key.len()).collect::<Vec<_>>();\n",
            "    lens.iter().sum::<usize>().checked_add(1);\n",
            "    let first = v.iter().map(|n| n.name()).next();\n",
            "    first.unwrap().trim();\n",
            "    let n = open()?;\n",
            "    n.name();\n",
            "    let props = Props::new();\n",
            "    props.insert(String::new(), Node { key: String::new(), line: 0 });\n",
            "    v.iter().filter(|n| n.line > 0).for_each(|n| { n.name(); });\n",
            "    Ok(())\n",
            "}\n",
        ),
    );
    let out = run(&t);
    let calls: Vec<&str> = out
        .edges
        .iter()
        .filter(|e| e.ty == "CALLS" && e.src == "k::go")
        .map(|e| e.dst.as_str())
        .collect();
    let ledger: Vec<String> = out
        .edges
        .iter()
        .filter(|e| e.src == "k::go" && e.dst.starts_with("?::"))
        .map(|e| format!("{} {:?}", e.dst, e.props.get("_reason")))
        .collect();
    for target in [
        "k::Node::name",
        "usize::checked_add",
        "String::len",
        "String::trim",
        "Vec::first",
        "Vec::iter",
        "Iterator::map",
        "Iterator::collect",
        "Iterator::sum",
        "Iterator::next",
        "Option::unwrap",
        "std::collections::HashMap::insert",
        "Iterator::filter",
        "Iterator::for_each",
    ] {
        assert!(
            calls.contains(&target),
            "{target} missing from {calls:?}\n{ledger:#?}"
        );
    }
    assert!(ledger.is_empty(), "every receiver was typed: {ledger:#?}");
    every_edge_has_endpoints(&out);
}

/// What a body names beyond its locals types too: a constant by its
/// declaration; a `match` arm's `Variant(x)` by the enum's own fields,
/// whether the pattern spells the enum or imports the variant; `let (a, b)
/// = (x, y)` each side by side; and `let e = e?;` re-binds `e` from what it
/// was, so both the old and the new `e` type.
#[test]
fn constants_variants_and_rebindings_type() {
    let t = Tree::new("ext-consts");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        concat!(
            "pub const NAMES: &[&str] = &[\"a\"];\n",
            "pub static COUNT: usize = 1;\n",
            "pub struct Txn;\nimpl Txn { pub fn commit(self) {} }\n",
            "pub struct Plan;\nimpl Plan { pub fn run(&self) {} }\n",
            "pub enum Stmt { Write(Txn), Read { plan: Plan }, Nothing }\n",
            "use Stmt::Read;\n",
            "pub fn go(s: Stmt, r: Result<Txn, ()>) {\n",
            "    NAMES.iter().count();\n",
            "    COUNT.checked_add(1);\n",
            "    match s { Stmt::Write(t) => t.commit(), Read { plan } => plan.run(), Stmt::Nothing => {} }\n",
            "    let (a, b) = (Vec::<u8>::new(), String::new());\n",
            "    a.len(); b.trim();\n",
            "    let r = r.unwrap();\n",
            "    r.commit();\n",
            "}\n",
        ),
    );
    let out = run(&t);
    let calls: Vec<&str> = out
        .edges
        .iter()
        .filter(|e| e.ty == "CALLS" && e.src == "k::go")
        .map(|e| e.dst.as_str())
        .collect();
    let ledger: Vec<String> = out
        .edges
        .iter()
        .filter(|e| e.src == "k::go" && e.dst.starts_with("?::"))
        .map(|e| format!("{} {:?}", e.dst, e.props.get("_reason")))
        .collect();
    for target in [
        "slice::iter",
        "Iterator::count",
        "usize::checked_add",
        "k::Txn::commit",
        "k::Plan::run",
        "Vec::len",
        "String::trim",
        "Result::unwrap",
    ] {
        assert!(
            calls.contains(&target),
            "{target} missing from {calls:?}\n{ledger:#?}"
        );
    }
    assert!(ledger.is_empty(), "every receiver was typed: {ledger:#?}");
    every_edge_has_endpoints(&out);
}

/// Three rules, all of them cargo's or the language's own: the `#[test]`
/// family, a `#[cfg(test)]` module (a scope, so it reaches the `impl` blocks
/// written inside it), and a file under `tests/`. Production code beside them
/// stays unflagged, and a `#[cfg(not(test))]` item is the opposite of a test.
#[test]
fn test_code_is_flagged_by_attribute_scope_and_target() {
    let t = Tree::new("testflag");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n");
    t.write(
        "src/lib.rs",
        r#"
pub struct Engine;

impl Engine {
    pub fn start(&self) {}
}

#[cfg(not(test))]
pub fn production_only() {}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture { n: u32 }

    impl Fixture {
        fn make() -> Self { Fixture { n: 1 } }
    }

    #[test]
    fn starts() { Engine.start(); }

    fn helper() {}
}

#[tokio::test]
async fn also_a_test() {}
"#,
    );
    t.write(
        "tests/integration.rs",
        "pub fn drives_it() {}\n\npub struct Harness;\n\nimpl Harness {\n    pub fn run(&self) {}\n}\n",
    );
    let a = run(&t);

    let flagged = |key: &str| -> (String, String) {
        let n = a
            .nodes
            .iter()
            .find(|n| n.key == key)
            .unwrap_or_else(|| panic!("no node {key} in {:?}", keys(&a)));
        (
            text_of(
                n.props
                    .get("test_flag")
                    .unwrap_or_else(|| panic!("{key} carries no test_flag: {:?}", n.props)),
            )
            .unwrap(),
            text_of(n.props.get("_test_flag_confidence").unwrap()).unwrap(),
        )
    };

    // The attribute is the narrowest evidence and survives the scope pass.
    assert_eq!(
        flagged("k::tests::starts"),
        ("attribute".into(), "definitive".into())
    );
    assert_eq!(
        flagged("k::also_a_test"),
        ("attribute".into(), "definitive".into()),
        "a runtime's own test attribute is still a test attribute"
    );

    // The `#[cfg(test)]` module is a scope: everything under it, impl methods
    // included, is compiled out of the library.
    for key in [
        "k::tests",
        "k::tests::helper",
        "k::tests::Fixture",
        "k::tests::Fixture::make",
    ] {
        assert_eq!(
            flagged(key),
            ("build-rule".into(), "definitive".into()),
            "{key}"
        );
    }

    // An integration target is a scope too.
    for key in [
        "k::tests::integration::drives_it",
        "k::tests::integration::Harness",
        "k::tests::integration::Harness::run",
    ] {
        assert_eq!(
            flagged(key),
            ("build-rule".into(), "definitive".into()),
            "{key}"
        );
    }

    for key in ["k::Engine", "k::Engine::start", "k::production_only", "k"] {
        assert!(
            !a.nodes
                .iter()
                .find(|n| n.key == key)
                .unwrap()
                .props
                .contains_key("test_flag"),
            "{key} must not be flagged"
        );
    }
}

/// A declared type is a dependency as surely as a call is. Fields,
/// parameters, returns, enum payloads and alias targets all say so — folded
/// to one edge per pair carrying the union of the positions, generic
/// arguments walked, foreign types left alone, and never a self-loop.
#[test]
fn declared_types_become_uses_type_edges() {
    let t = Tree::new("usestype");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n");
    t.write(
        "src/lib.rs",
        r#"
use std::collections::HashMap;
use std::sync::mpsc;

pub struct Job;

pub struct Cfg {
    pub n: u32,
}

pub type Jobs = Vec<Job>;

pub enum Event {
    Started(Job),
    Nothing,
}

pub struct Pool {
    pub jobs: mpsc::Sender<Job>,
    pub index: HashMap<String, Cfg>,
}

pub struct Node {
    pub next: Option<Box<Node>>,
}

pub fn work(c: &Cfg) -> Job {
    let _ = c;
    Job
}

pub fn foreign(s: String) -> u32 {
    let _ = s;
    0
}

pub fn round_trip(c: Cfg) -> Cfg {
    c
}
"#,
    );
    let a = run(&t);

    let uses = |src: &str, dst: &str| -> Option<String> {
        a.edges
            .iter()
            .find(|e| e.ty == "USES_TYPE" && e.src == src && e.dst == dst)
            .map(|e| text_of(&e.props["role"]).unwrap())
    };

    // A field, through the generic argument of a foreign wrapper: this is how
    // most code depends on a type, and stopping at `Sender` would miss it.
    assert_eq!(uses("k::Pool", "k::Job").as_deref(), Some("field"));
    assert_eq!(uses("k::Pool", "k::Cfg").as_deref(), Some("field"));
    // Parameter and return fold to one edge carrying both positions.
    assert_eq!(uses("k::work", "k::Cfg").as_deref(), Some("param"));
    assert_eq!(uses("k::work", "k::Job").as_deref(), Some("return"));
    assert_eq!(
        uses("k::round_trip", "k::Cfg").as_deref(),
        Some("param, return"),
        "one dependency, two positions, one edge"
    );
    // An enum payload and an alias target.
    assert_eq!(uses("k::Event", "k::Job").as_deref(), Some("variant"));
    assert_eq!(uses("k::Jobs", "k::Job").as_deref(), Some("alias"));

    // Foreign types stay text: no edge to String, u32, Vec, HashMap, Sender.
    for dst in ["String", "u32", "Vec", "HashMap", "Sender", "Option", "Box"] {
        assert!(
            !a.edges
                .iter()
                .any(|e| e.ty == "USES_TYPE" && e.dst.ends_with(dst)),
            "{dst} is not declared here and must not get an edge"
        );
    }
    // A type that mentions itself is a real shape and a useless edge.
    assert!(
        !a.edges
            .iter()
            .any(|e| e.ty == "USES_TYPE" && e.src == e.dst),
        "never a self-loop"
    );
    // The ledger says what it did and what it left.
    assert!(
        a.notes.iter().any(|n| n.contains("USES_TYPE")),
        "{:?}",
        a.notes
    );
}

/// Concurrency is a fact about the call site. What a spawner is handed runs
/// somewhere else, and a reader who cannot tell that call from a synchronous
/// one is reading a different program. Only the departures are marked: inside
/// an ordinary body, waiting is the expectation.
#[test]
fn a_call_that_runs_beside_its_caller_says_so() {
    let t = Tree::new("spawned");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n");
    t.write(
        "src/lib.rs",
        r#"
pub struct Cfg;

pub async fn work(c: &Cfg) -> u32 { let _ = c; 0 }
pub fn blocking() {}
pub fn plain() {}

pub async fn run(handle: Handle) {
    let c = Cfg;
    tokio::spawn(async move { work(&c).await });
    std::thread::spawn(|| blocking());
    tokio::task::spawn_blocking(|| heavy());
    handle.spawn(async { nested() });
    plain();
}

pub fn heavy() {}
pub fn nested() {}
pub struct Handle;
"#,
    );
    let a = run(&t);

    let shape = |dst: &str| -> Option<String> {
        a.edges
            .iter()
            .find(|e| e.ty == "CALLS" && e.src == "k::run" && e.dst.ends_with(dst))
            .map(|e| match e.props.get("concurrent") {
                Some(v) => text_of(v).unwrap_or_default(),
                None => String::new(),
            })
    };

    assert_eq!(shape("k::work").as_deref(), Some("spawned"));
    assert_eq!(shape("k::blocking").as_deref(), Some("spawned"));
    assert_eq!(
        shape("k::heavy").as_deref(),
        Some("blocking"),
        "`spawn_blocking` is its own departure"
    );
    assert_eq!(
        shape("k::nested").as_deref(),
        Some("spawned"),
        "a method-form spawner is the same rule"
    );
    assert_eq!(
        shape("k::plain").as_deref(),
        Some(""),
        "an ordinary call says nothing — waiting is the expectation"
    );
    // The spawner itself is called synchronously; marking an edge to
    // something named `spawn` would only repeat its own name.
    let spawner = a
        .edges
        .iter()
        .find(|e| e.ty == "CALLS" && e.dst.ends_with("tokio::spawn"))
        .expect("the spawn call is still an edge");
    assert!(!spawner.props.contains_key("concurrent"));
}

/// One caller reaching one callee both ways folds to one edge carrying the
/// union: an awaited site must not swallow a spawned one by being written
/// first, which is the rule ts and py already apply.
#[test]
fn a_spawned_site_survives_an_awaited_one() {
    let t = Tree::new("union");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n");
    t.write(
        "src/lib.rs",
        concat!(
            "pub async fn work() {}\n",
            "pub async fn first_awaited() {\n",
            "    work().await;\n",
            "    tokio::spawn(async { work().await });\n",
            "}\n",
            "pub async fn first_spawned() {\n",
            "    tokio::spawn(async { work().await });\n",
            "    work().await;\n",
            "}\n",
        ),
    );
    let a = run(&t);
    for caller in ["k::first_awaited", "k::first_spawned"] {
        let edges: Vec<_> = a
            .edges
            .iter()
            .filter(|e| e.ty == "CALLS" && e.src == caller && e.dst == "k::work")
            .collect();
        assert_eq!(edges.len(), 1, "one callee, one edge: {edges:?}");
        assert_eq!(
            text_of(&edges[0].props["concurrent"]).as_deref(),
            Some("spawned"),
            "{caller}: the spawned half is what a reader needs, whichever was written first"
        );
    }
}

/// A derive and a hand-written impl state the same fact, and the graph used
/// to record one and not the other. Which spelling was used rides on the
/// edge, because that is a fact about the source, not a different relation.
#[test]
fn a_derive_is_an_implements_edge() {
    let t = Tree::new("derives");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n");
    t.write(
        "src/lib.rs",
        concat!(
            "pub trait Local {}\n",
            "#[derive(Clone, Debug, serde::Serialize)]\n",
            "pub struct Cfg;\n",
            "#[derive(Local)]\n",
            "pub struct Both;\n",
            "pub struct Written;\n",
            "impl Local for Written {}\n",
            "#[derive(Clone)]\n",
            "pub enum Shape { A }\n",
        ),
    );
    let a = run(&t);

    let derived = |src: &str, dst: &str| -> Option<bool> {
        a.edges
            .iter()
            .find(|e| e.ty == "IMPLEMENTS" && e.src == src && e.dst.ends_with(dst))
            .map(|e| e.props.contains_key("derived"))
    };

    assert_eq!(derived("k::Cfg", "Clone"), Some(true));
    assert_eq!(derived("k::Cfg", "Debug"), Some(true));
    assert_eq!(
        derived("k::Cfg", "serde::Serialize"),
        Some(true),
        "an external trait, exactly where a hand-written impl would land"
    );
    assert_eq!(derived("k::Shape", "Clone"), Some(true), "enums derive too");
    assert_eq!(
        derived("k::Both", "k::Local"),
        Some(true),
        "a derive naming a trait this tree declares resolves to it"
    );
    assert_eq!(
        derived("k::Written", "k::Local"),
        Some(false),
        "a hand-written impl is the same edge, unmarked"
    );
}

/// On a service the attributes are the architecture. The path is the node —
/// so every GET handler is one hop from `get` — and the whole attribute rides
/// on the edge, because the route is what a reader actually wants.
#[test]
fn attributes_become_annotated_by_with_their_whole_text() {
    let t = Tree::new("attrs");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n");
    t.write(
        "src/lib.rs",
        concat!(
            "#[get(\"/health\")]\n",
            "pub fn health() {}\n",
            "#[get(\"/users\")]\n",
            "pub fn users() {}\n",
            "#[tokio::main]\n",
            "pub async fn main() {}\n",
            "#[derive(Clone)]\n",
            "#[serde(rename_all = \"snake_case\")]\n",
            "pub struct Cfg;\n",
            "#[allow(dead_code)]\n",
            "#[inline]\n",
            "pub fn quiet() {}\n",
        ),
    );
    let a = run(&t);

    let ann = |src: &str| -> Vec<(String, String)> {
        a.edges
            .iter()
            .filter(|e| e.ty == "ANNOTATED_BY" && e.src == src)
            .map(|e| {
                (
                    e.dst.clone(),
                    e.props
                        .get("arguments")
                        .and_then(text_of)
                        .unwrap_or_default(),
                )
            })
            .collect()
    };

    // Two routes, one `get` node — the vocabulary stays finite.
    let health = ann("k::health");
    assert_eq!(health.len(), 1, "{health:?}");
    assert!(health[0].0.ends_with("get"), "{health:?}");
    assert!(health[0].1.contains("/health"), "{health:?}");
    assert!(ann("k::users")[0].1.contains("/users"));
    assert_eq!(ann("k::users")[0].0, health[0].0, "one `get`, two edges");

    assert!(ann("k::main")[0].0.ends_with("tokio::main"));
    assert!(
        ann("k::Cfg")
            .iter()
            .any(|(dst, args)| dst.ends_with("serde") && args.contains("snake_case")),
        "{:?}",
        ann("k::Cfg")
    );
    // A derive left by the other door, and the lint attributes are not facts
    // about the program's shape.
    assert!(
        !ann("k::Cfg").iter().any(|(dst, _)| dst.ends_with("derive")),
        "a derive is an IMPLEMENTS edge, not an annotation"
    );
    assert!(ann("k::quiet").is_empty(), "{:?}", ann("k::quiet"));
}

/// A declaration's extent, and the trap beside it: `line` points at the
/// *name* so documentation and attributes above it cannot move where the
/// graph says a thing starts, while the end comes from the whole item. Get
/// that backwards and every recorded `file:line` a reader follows shifts up
/// onto a doc comment.
#[test]
fn a_declaration_records_where_it_ends_without_moving_where_it_starts() {
    let t = Tree::new("extent");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n");
    t.write(
        "src/lib.rs",
        concat!(
            "/// One.\n",               // 1
            "/// Two.\n",               // 2
            "#[derive(Clone)]\n",       // 3
            "pub struct Cfg {\n",       // 4
            "    pub n: u32,\n",        // 5
            "}\n",                      // 6
            "\n",                       // 7
            "/// Documented.\n",        // 8
            "#[inline]\n",              // 9
            "pub fn work() -> u32 {\n", // 10
            "    let x = 1;\n",         // 11
            "    x\n",                  // 12
            "}\n",                      // 13
            "\n",                       // 14
            "pub fn tiny() {}\n",       // 15
        ),
    );
    let a = run(&t);
    let at = |key: &str, prop: &str| -> Option<i64> {
        a.nodes
            .iter()
            .find(|n| n.key == key)?
            .props
            .get(prop)?
            .as_i64()
    };

    // The name's line, not the doc comment's and not the attribute's.
    assert_eq!(at("k::Cfg", "line"), Some(4));
    assert_eq!(at("k::Cfg", "end_line"), Some(6));
    assert_eq!(at("k::work", "line"), Some(10));
    assert_eq!(at("k::work", "end_line"), Some(13));
    // A one-line function is one line, which is the whole point: `snippet`
    // used to answer this with forty.
    assert_eq!(at("k::tiny", "line"), Some(15));
    assert_eq!(at("k::tiny", "end_line"), Some(15));
}

/// A closure body that chains back through its own hop is a cycle, and the
/// hop budget is what ends it. `chain_type_why` used to spend the budget
/// without checking one was left, so `depth - 1` wrapped at zero: a debug
/// build panicked with "attempt to subtract with overflow", and the release
/// build the wasm component is compiled as recursed until the guest stack was
/// gone — `plugin `rust` trapped: call stack exhausted`, which left a plane
/// dropped and never rebuilt.
#[test]
fn a_closure_body_that_chains_through_its_own_hop_ends_on_the_budget() {
    let declared: BTreeSet<String> = ["m::T".to_string()].into_iter().collect();
    let scopes: BTreeMap<String, String> = [("m::T::f".to_string(), "m".to_string())]
        .into_iter()
        .collect();
    // The cycle: typing `self.map()` asks its closure body for what the hop
    // yields, and that body is the same chain again.
    let closure_bodies: BTreeMap<(String, String), String> =
        [(("m::T::f".to_string(), "self.map()".to_string()), "self.map()".to_string())]
            .into_iter()
            .collect();
    let empty_vecs = BTreeMap::new();
    let empty_strs = BTreeMap::new();
    let empty_alias = BTreeMap::new();
    let empty_consts = BTreeMap::new();
    let empty_ctypes = BTreeMap::new();
    let empty_variants = BTreeMap::new();
    let empty_locals = BTreeMap::new();
    let empty_fields = BTreeMap::new();
    let empty_impls = BTreeMap::new();
    let empty_timpls = BTreeMap::new();
    let typing = Typing {
        declared: &declared,
        fns: &empty_vecs,
        types: &empty_vecs,
        traits: &empty_vecs,
        scopes: &scopes,
        aliases: &empty_strs,
        returns_map: &empty_strs,
        alias_map: &empty_alias,
        closure_bodies: &closure_bodies,
        consts_by_name: &empty_consts,
        const_types: &empty_ctypes,
        variant_map: &empty_variants,
        local_inits: &empty_locals,
        fields_map: &empty_fields,
        impls_of: &empty_impls,
        trait_impl_methods: &empty_timpls,
    };

    // Returns rather than recursing: the assertion is that this call ends at
    // all. Whether the hop types is beside the point.
    let _ = typing.chain_type_why("m::T::f", "self.map()", 24);

    // And the budget itself refuses instead of spending what it has not got.
    let out = typing.chain_type_why("m::T::f", "self.map()", 0);
    assert!(
        out.is_err_and(|why| why.contains("too many hops")),
        "a spent budget names itself as the reason"
    );
}
