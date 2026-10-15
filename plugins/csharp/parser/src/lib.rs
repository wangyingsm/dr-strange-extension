//! C# into facts — nodes and edges a parser is certain of, leaving nothing
//! for a model to guess at.
//!
//! Same discipline as the parsers beside it: parse each file alone, resolve
//! across files once in [`assemble`], count whatever could not be resolved.
//! tree-sitter's C# grammar underneath — syntax only, no symbol tables,
//! which is the point.
//!
//! Keys are C#'s own qualified names: the `namespace` declaration, then the
//! type, then the member — `Acme.Core.Engine.Start`; a nested type is
//! `Acme.Core.Engine.Builder`. A file is not a node: the type is the unit
//! and the namespace is the container, and a `partial` type is one
//! declaration written in several files.

mod resolve;
mod walk;

pub use resolve::{Assembled, assemble};
pub use walk::Walker;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What the parser reads through — the plugin contract's host.
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

/// One call site, held until every type is known.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Call {
    pub caller: String,
    /// The type the caller sits in, for own-member and inherited resolution.
    pub in_type: String,
    pub kind: CallKind,
    pub line: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallKind {
    /// `Helper()` / `this.Helper()` — the type's own surface, or a member of
    /// a `using static` type.
    Own(String),
    /// `Helper.Create()` / `Acme.Util.Helper.Create()` — through a type
    /// reference, written down.
    Static { type_ref: String, method: String },
    /// `new Foo(…)` — instantiation is a call to the type.
    New(String),
    /// `base.M()` — the base class, where this tree holds it.
    Super(String),
    /// `log.Write()` — a value receiver, typed when the source states its
    /// type (a declared local, a parameter, a field, or a property).
    Receiver { name: String, method: String },
    /// `this.repo.Save()` — through the member's declared type.
    SelfField { field: String, method: String },
    /// `obj.repo.Save()` — one member hop on a typed value.
    FieldChain {
        obj: String,
        field: String,
        method: String,
    },
    /// `Foo.Instance().Bar()` — a static factory chain, typed by the
    /// factory's declared return.
    StaticChain {
        type_ref: String,
        first: String,
        method: String,
    },
}

/// How a name is bound to a type the source states — a declared local, a
/// parameter, a field or a property. `caller` is a member key for locals and
/// params, a type key for fields and properties.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hint {
    pub caller: String,
    pub name: String,
    /// The type as written (generics stripped to what they subscript).
    pub written: String,
}

/// One name a `using` binds. C#'s plain `using` imports a *namespace*, which
/// is this plugin's wildcard; an alias and a `using static` bind one name
/// each.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Import {
    pub local: String,
    pub target: String,
    /// `using static Acme.Util.Helper` — the type's members are in scope
    /// under their own names.
    #[serde(default)]
    pub is_static: bool,
}

/// One type position a declaration writes down — a field, a property, a
/// parameter, a return — resolved in [`assemble`] into a `USES_TYPE` edge
/// when it names a type this tree declares.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeRef {
    pub owner: String,
    pub role: String,
    pub name: String,
    pub written: String,
}

/// One name a declaration writes beside itself — a base type or an
/// attribute — resolved in [`assemble`] into the edge its kind names.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Clause {
    pub src: String,
    pub written: String,
    pub ty: String,
    pub line: u64,
    /// An attribute's argument list as written, parentheses stripped.
    #[serde(default)]
    pub args: Option<String>,
    /// First in the base list. C# writes the base class there and nowhere
    /// else, and interfaces after it — the only distinction its syntax
    /// draws. [`assemble`] corrects this from the target's own declaration
    /// where the tree holds it.
    #[serde(default)]
    pub first: bool,
}

impl Clause {
    /// A clause with no arguments — every kind but an attribute's.
    pub fn new(src: &str, written: String, ty: &str, line: u64) -> Self {
        Self {
            src: src.to_string(),
            written,
            ty: ty.to_string(),
            line,
            args: None,
            first: false,
        }
    }
}

/// One service registration, as the source writes it — the fact the
/// container acts on.
///
/// `services.AddScoped<IRepo, JpaRepo>()` is not configuration a parser has
/// to guess at: it is a call, with both types written down in it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registration {
    /// The service type asked for at an injection point.
    pub service: String,
    /// The type the container builds for it — the same as `service` for a
    /// self-registration.
    pub implementation: String,
    /// `AddKeyedScoped<IRepo, Fast>("fast")` — the key a
    /// `[FromKeyedServices]` names.
    pub key: Option<String>,
}

/// One dependency a type takes by injection, as the source declares it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Injection {
    /// The type holding the injection point.
    pub owner: String,
    /// The constructor parameter, which is the only place ASP.NET Core
    /// injects.
    pub name: String,
    /// The declared type's head as written.
    pub written: String,
    /// Its generic arguments as written — the element a collection injects.
    pub args: Vec<String>,
    /// What the evidence was: `attribute` (`[FromServices]` /
    /// `[FromKeyedServices]` written on the parameter) outranks
    /// `constructor` (a registered type's only constructor, which the
    /// container calls).
    pub kind: String,
    /// `[FromKeyedServices("fast")]`'s argument as written; which key it
    /// states is read in [`assemble`].
    pub keyed: Option<String>,
}

/// A declared type: name, kind, and its member surface (for cross-file
/// static and inherited resolution).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeDecl {
    pub key: String,
    pub methods: Vec<String>,
    /// The base types as written, first one first.
    pub bases: Vec<String>,
    /// Its label — `Interface` is the one a call can be dispatched through,
    /// since it declares a member without writing one.
    #[serde(default)]
    pub kind: String,
}

/// Everything one file contributes — the opaque partial the component
/// serializes between the two phases.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileFacts {
    pub file: String,
    /// The namespace this file's declarations sit in, "" for the global one.
    pub namespace: String,
    pub failed: bool,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub calls: Vec<Call>,
    pub imports: Vec<Import>,
    /// `using Acme.Util` namespaces, in order.
    pub wildcards: Vec<String>,
    /// `global using Acme.Util` namespaces, which are every file's.
    #[serde(default)]
    pub globals: Vec<String>,
    /// `using` directives as written, with lines — for IMPORTS edges from
    /// this file's top-level types.
    pub import_lines: Vec<(String, u64)>,
    /// Top-level type keys in this file (IMPORTS edges hang off them).
    pub top_types: Vec<String>,
    pub types: Vec<TypeDecl>,
    /// Base types and attributes, all resolved the same way.
    pub clauses: Vec<Clause>,
    /// Every namespace this file declares.
    #[serde(default)]
    pub namespaces: Vec<String>,
    pub opaque: usize,
    /// Type bindings the source states (params, locals, fields, properties).
    pub hints: Vec<Hint>,
    /// The type positions this file's declarations write down.
    #[serde(default)]
    pub type_refs: Vec<TypeRef>,
    /// `(member key, declared return as written)` when it names a plain
    /// type — what types a static-factory chain.
    pub returns: Vec<(String, String)>,
    /// The service registrations this file's code makes.
    #[serde(default)]
    pub registrations: Vec<Registration>,
    /// The dependencies this file's types take by injection.
    #[serde(default)]
    pub injections: Vec<Injection>,
    /// The routes this file's members ask for — a call argument shaped like
    /// a path. Attached to the calling member in [`assemble`].
    #[serde(default)]
    pub requests: Vec<dr_strange_ext::route::RouteRef>,
}

/// The extensions this plugin claims.
pub const EXTENSIONS: &[&str] = &["cs"];

/// The attributes that say "inject this" where a parameter alone would not.
pub const INJECT_ATTRIBUTES: &[&str] = &["FromServices", "FromKeyedServices"];

/// Parse one chunk of paths into per-file facts.
pub fn parse_chunk(files: &dyn Files, paths: &[String], include_source: bool) -> Vec<FileFacts> {
    paths
        .iter()
        .map(|p| match files.read(p) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => parse_file(p, &text, include_source),
                Err(_) => failed(p),
            },
            Err(_) => failed(p),
        })
        .collect()
}

/// Parse one pushed document.
pub fn parse_document(name: &str, bytes: &[u8], include_source: bool) -> Vec<FileFacts> {
    match String::from_utf8(bytes.to_vec()) {
        Ok(text) => vec![parse_file(name, &text, include_source)],
        Err(_) => vec![failed(name)],
    }
}

fn failed(path: &str) -> FileFacts {
    FileFacts {
        file: path.to_string(),
        failed: true,
        ..Default::default()
    }
}

fn parse_file(path: &str, text: &str, include_source: bool) -> FileFacts {
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
        .is_err()
    {
        return failed(path);
    }
    let Some(tree) = parser.parse(text, None) else {
        return failed(path);
    };
    let root = tree.root_node();
    if root.kind() != "compilation_unit" {
        return failed(path);
    }

    let mut facts = FileFacts {
        file: path.to_string(),
        ..Default::default()
    };
    Walker::new(&mut facts, text, include_source).compilation_unit(root);
    // tree-sitter always yields a tree; one that errored and produced no
    // facts at all did not parse in any useful sense.
    if root.has_error() && facts.nodes.is_empty() && facts.import_lines.is_empty() {
        return failed(path);
    }
    for n in facts.nodes.iter_mut() {
        n.props
            .insert("file".into(), Value::String(path.to_string()));
    }
    flag_test_code(&mut facts, path);
    facts
}

/// Test-ness, weakest evidence first so the strongest is what remains.
///
/// A test project is a directory the solution names — `Acme.Tests`,
/// `tests/` — which is a convention every .NET repository follows and none
/// enforces. A test attribute is xUnit's, NUnit's and MSTest's own
/// definition of a test, so it overwrites the layout flag on the method it
/// marks and on the type holding that method: a class with a `[Fact]` in it
/// is a test class.
fn flag_test_code(facts: &mut FileFacts, path: &str) {
    if is_test_project(path) {
        for n in facts.nodes.iter_mut() {
            set_test_flag(
                &mut n.props,
                "project-layout",
                "test code: under a test project directory — a `.Tests`/`.Test` project or a `tests/` tree",
                "strong",
            );
        }
    }
    let marked: Vec<String> = facts
        .clauses
        .iter()
        .filter(|c| c.ty == "ANNOTATED_BY" && is_test_attribute(&c.written))
        .map(|c| c.src.clone())
        .collect();
    for key in marked {
        let owner = key.rsplit_once('.').map(|(o, _)| o.to_string());
        for n in facts
            .nodes
            .iter_mut()
            .filter(|n| n.key == key || owner.as_deref().is_some_and(|o| n.key == o))
        {
            set_test_flag(
                &mut n.props,
                "attribute",
                "test code: carries an xUnit/NUnit/MSTest test attribute, or holds a member that does",
                "definitive",
            );
        }
    }
}

/// Is this path inside a test project? The `.csproj` that would say so names
/// no `.cs` file, so the directory is the evidence this plugin has.
fn is_test_project(path: &str) -> bool {
    let p = path.replace('\\', "/");
    p.split('/').any(|segment| {
        let lower = segment.to_ascii_lowercase();
        lower == "tests" || lower == "test" || lower.ends_with(".tests") || lower.ends_with(".test")
    })
}

/// The attributes xUnit, NUnit and MSTest define a test by — the test
/// methods themselves and the lifecycle hooks that only exist to serve them.
/// Written with or without the `Attribute` suffix, unqualified or fully
/// qualified alike.
const TEST_ATTRIBUTES: &[&str] = &[
    "Fact",
    "Theory",
    "Test",
    "TestCase",
    "TestCaseSource",
    "TestFixture",
    "SetUp",
    "TearDown",
    "OneTimeSetUp",
    "OneTimeTearDown",
    "TestMethod",
    "DataTestMethod",
    "TestClass",
    "TestInitialize",
    "TestCleanup",
    "ClassInitialize",
    "ClassCleanup",
];

fn is_test_attribute(written: &str) -> bool {
    let last = written.rsplit('.').next().unwrap_or(written);
    let bare = last.strip_suffix("Attribute").unwrap_or(last);
    TEST_ATTRIBUTES.contains(&bare)
}

/// Mark a node as test code: what the evidence was, and — in the companion
/// `_` property the renderers keep out of sight and the embedder out of its
/// vectors — how much that evidence is worth. Two properties rather than one
/// because the second is for a reader weighing the first, not for display.
fn set_test_flag(props: &mut Props, kind: &str, desc: &str, confidence: &str) {
    props.insert(
        "test_flag".into(),
        serde_json::json!({ "$desc": desc, "$value": kind }),
    );
    props.insert(
        "_test_flag_confidence".into(),
        Value::String(confidence.into()),
    );
}

fn edge_at(src: &str, dst: &str, ty: &str, line: u64) -> Edge {
    let mut props = Props::new();
    props.insert("line".into(), Value::from(line));
    Edge {
        src: src.to_string(),
        dst: dst.to_string(),
        ty: ty.to_string(),
        props,
    }
}

#[cfg(test)]
mod tests;
