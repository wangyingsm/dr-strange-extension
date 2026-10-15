//! Kotlin into facts — nodes and edges a parser is certain of, leaving
//! nothing for a model to guess at.
//!
//! Same discipline as the parsers beside it: parse each file alone, resolve
//! across files once in [`assemble`], count whatever could not be resolved.
//! tree-sitter's Kotlin grammar underneath — syntax only, no symbol tables,
//! which is the point.
//!
//! Keys are Kotlin's own qualified names: the `package` declaration, then the
//! declaration — `com.acme.core.Engine`, `com.acme.core.Engine.start`; a
//! nested type is `com.acme.core.Engine.Builder`. A file is not a node: the
//! declaration is the unit and the package is the container, which is why a
//! top-level function sits directly under its package.

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
    /// Empty for a top-level function, which sits in no type.
    pub in_type: String,
    pub kind: CallKind,
    pub line: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallKind {
    /// `helper()` / `this.helper()` — the class's own surface, a top-level
    /// function of this package, or an imported member.
    Own(String),
    /// `Helper.create()` / `com.acme.Helper.create()` — through a type
    /// reference, written down. A companion object answers these.
    Static { type_ref: String, method: String },
    /// `Foo(…)` — Kotlin spells instantiation as a call to the type, and so
    /// does this.
    New(String),
    /// `super.m()` — the one supertype a constructor invocation names.
    Super(String),
    /// `lg.log()` — a value receiver, typed when the source states its type
    /// (a declared local, a parameter, or a property).
    Receiver { name: String, method: String },
    /// `this.prop.m()` — through the property's declared type.
    SelfField { field: String, method: String },
    /// `obj.prop.m()` — one property hop on a typed value.
    FieldChain {
        obj: String,
        field: String,
        method: String,
    },
    /// `Foo.getInstance().bar()` — a static factory chain, typed by the
    /// factory's declared return.
    StaticChain {
        type_ref: String,
        first: String,
        method: String,
    },
}

/// How a name is bound to a type the source states — a declared local, a
/// parameter, or a property. `caller` is a function key for locals and
/// params, a type key for properties.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hint {
    pub caller: String,
    pub name: String,
    /// The type as written (generics stripped to what they subscript).
    pub written: String,
}

/// One name an import binds: `import a.b.C` binds `C`, `import a.b.spin`
/// binds the member `spin`, `import a.b.C as D` binds `D`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Import {
    pub local: String,
    pub target: String,
    /// A lowercase last segment imports a member — a function or a property —
    /// the way `import static` does in Java.
    #[serde(default)]
    pub is_member: bool,
}

/// One type position a declaration writes down — a property, a parameter, a
/// return, an extension receiver — resolved in [`assemble`] into a
/// `USES_TYPE` edge when it names a type this tree declares.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeRef {
    pub owner: String,
    pub role: String,
    pub name: String,
    pub written: String,
}

/// One name a declaration writes beside itself — a supertype or an
/// annotation — resolved in [`assemble`] into the edge its kind names.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Clause {
    pub src: String,
    pub written: String,
    pub ty: String,
    pub line: u64,
    /// An annotation's argument list as written, parentheses stripped.
    #[serde(default)]
    pub args: Option<String>,
    /// A supertype written without a constructor invocation can only be an
    /// interface — but one written *with* one is a class, and that is the
    /// only distinction Kotlin's syntax draws. [`assemble`] corrects this
    /// from the target's own declaration where the tree holds it.
    #[serde(default)]
    pub invoked: bool,
}

impl Clause {
    /// A clause with no arguments — every kind but an annotation's.
    pub fn new(src: &str, written: String, ty: &str, line: u64) -> Self {
        Self {
            src: src.to_string(),
            written,
            ty: ty.to_string(),
            line,
            args: None,
            invoked: false,
        }
    }
}

/// One dependency a bean takes by injection, as the source declares it — the
/// fact Spring's container acts on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Injection {
    /// The type holding the injection point.
    pub owner: String,
    /// The property, or the constructor/setter parameter standing in for it.
    pub name: String,
    /// The declared type's head as written.
    pub written: String,
    /// Its generic arguments as written — the element a collection injects.
    pub args: Vec<String>,
    /// What the evidence was: `annotation` (`@Autowired` / `@Inject` /
    /// `@Resource` written on it) outranks `constructor` (a stereotyped
    /// bean's only constructor, which Spring injects unannotated — in Kotlin
    /// that is the primary constructor, and it is how nearly every Kotlin
    /// bean is written).
    pub kind: String,
    /// `@Qualifier`'s or `@Resource`'s arguments as written; which bean name
    /// they state is read in [`assemble`], with the rest of Spring's rules.
    pub qualifier: Option<String>,
}

/// A declared type: name, kind, and its member surface (for cross-file
/// static and inherited resolution).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeDecl {
    pub key: String,
    pub methods: Vec<String>,
    /// Supertypes written with a constructor invocation — the one class a
    /// Kotlin type can have, and what `super.m()` walks.
    pub extends: Vec<String>,
    /// Supertypes written without one: interfaces, and an interface's own
    /// supertypes. In the member walk, where an interface's default
    /// implementation answers an inherited call.
    #[serde(default)]
    pub implements: Vec<String>,
    /// Its label — `Interface` is the one a call can be dispatched through,
    /// since it declares a member without writing one.
    #[serde(default)]
    pub kind: String,
    /// Its companion object's key, which answers `Type.member()`.
    #[serde(default)]
    pub companion: Option<String>,
}

/// Everything one file contributes — the opaque partial the component
/// serializes between the two phases.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileFacts {
    pub file: String,
    /// The `package` declaration, "" for the default package.
    pub package: String,
    pub failed: bool,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub calls: Vec<Call>,
    pub imports: Vec<Import>,
    /// `import a.b.*` packages, in order.
    pub wildcards: Vec<String>,
    /// Import statements as written, with lines — for IMPORTS edges from
    /// this file's top-level declarations.
    pub import_lines: Vec<(String, u64)>,
    /// Top-level declaration keys in this file (IMPORTS edges hang off them).
    pub top_types: Vec<String>,
    pub types: Vec<TypeDecl>,
    /// Supertypes and annotations, all resolved the same way.
    pub clauses: Vec<Clause>,
    pub opaque: usize,
    /// Type bindings the source states (params, locals, properties).
    pub hints: Vec<Hint>,
    /// The type positions this file's declarations write down.
    #[serde(default)]
    pub type_refs: Vec<TypeRef>,
    /// `(function key, declared return as written)` when it names a plain
    /// type — what types a static-factory chain.
    pub returns: Vec<(String, String)>,
    /// `(simple name, key)` for every top-level function, which a bare call
    /// in the same package reaches without a receiver.
    #[serde(default)]
    pub top_functions: Vec<(String, String)>,
    /// The dependencies this file's types take by injection.
    #[serde(default)]
    pub injections: Vec<Injection>,
    /// The routes this file's functions ask for — a call argument shaped
    /// like a path. Attached to the calling function in [`assemble`].
    #[serde(default)]
    pub requests: Vec<dr_strange_ext::route::RouteRef>,
}

/// The extensions this plugin claims. `.kts` is a Kotlin script — a Gradle
/// build file is the common one, and it is Kotlin, so it parses here.
pub const EXTENSIONS: &[&str] = &["kt", "kts"];

/// The annotations Spring reads as "inject this".
pub const INJECT_ANNOTATIONS: &[&str] = &["Autowired", "Inject", "Resource"];

/// The annotations that make a type a bean Spring would instantiate.
pub const STEREOTYPES: &[&str] = &[
    "Service",
    "Component",
    "Repository",
    "Controller",
    "RestController",
    "Configuration",
    "ControllerAdvice",
    "RestControllerAdvice",
];

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
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .is_err()
    {
        return failed(path);
    }
    let Some(tree) = parser.parse(text, None) else {
        return failed(path);
    };
    let root = tree.root_node();
    if root.kind() != "source_file" {
        return failed(path);
    }

    let mut facts = FileFacts {
        file: path.to_string(),
        ..Default::default()
    };
    Walker::new(&mut facts, text, include_source).source_file(root);
    // tree-sitter always yields a tree; one that errored and produced no
    // facts at all did not parse in any useful sense.
    if root.has_error() && facts.nodes.is_empty() && facts.package.is_empty() {
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
/// `src/test/kotlin` is Gradle's and Maven's source-set layout and
/// `src/androidTest` is the Android plugin's — build conventions they
/// enforce, and ones a build file can still redraw. A test annotation is
/// JUnit's, TestNG's and kotlin.test's own definition of a test, so it
/// overwrites the layout flag on the function it marks and on the type
/// holding that function: a class with a `@Test` in it is a test class.
fn flag_test_code(facts: &mut FileFacts, path: &str) {
    if is_test_source_set(path) {
        for n in facts.nodes.iter_mut() {
            set_test_flag(
                &mut n.props,
                "build-layout",
                "test code: under a test source set (`src/test`, `src/androidTest`), which Gradle and Maven compile apart from the main one",
                "strong",
            );
        }
    }
    let marked: Vec<String> = facts
        .clauses
        .iter()
        .filter(|c| c.ty == "ANNOTATED_BY" && is_test_annotation(&c.written))
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
                "annotation",
                "test code: carries a JUnit/TestNG/kotlin.test test annotation, or holds a function that does",
                "definitive",
            );
        }
    }
}

/// The test source sets Gradle, Maven and the Android plugin lay out.
/// `src/test/java` counts: a Kotlin file is perfectly at home there, and
/// mixed modules put it there every day.
fn is_test_source_set(path: &str) -> bool {
    let p = path.replace('\\', "/");
    ["src/test/", "src/androidTest/", "src/commonTest/"]
        .iter()
        .any(|dir| p.contains(dir))
}

/// The annotations JUnit, TestNG and kotlin.test define a test by — the test
/// functions themselves and the lifecycle hooks that only exist to serve
/// them. Written unqualified or fully qualified alike; the last segment is
/// the name.
const TEST_ANNOTATIONS: &[&str] = &[
    "Test",
    "ParameterizedTest",
    "RepeatedTest",
    "TestFactory",
    "TestTemplate",
    "BeforeEach",
    "AfterEach",
    "BeforeAll",
    "AfterAll",
    "BeforeTest",
    "AfterTest",
    "Before",
    "After",
    "BeforeClass",
    "AfterClass",
];

fn is_test_annotation(written: &str) -> bool {
    let last = written.rsplit('.').next().unwrap_or(written);
    TEST_ANNOTATIONS.contains(&last)
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
