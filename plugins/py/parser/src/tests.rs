//! The parser's own tests, running natively — the reason it is a plain
//! library under the wasm component rather than one crate with it.

use super::*;

/// The host, as a plain map — the tests exercise the parser, not a
/// filesystem.
struct MapFiles {
    files: Vec<(&'static str, &'static str)>,
    label: &'static str,
}

impl Files for MapFiles {
    fn list(&self, suffix: &str) -> Result<Vec<String>, String> {
        let mut out: Vec<String> = self
            .files
            .iter()
            .filter(|(p, _)| p.ends_with(suffix))
            .map(|(p, _)| p.to_string())
            .collect();
        out.sort();
        Ok(out)
    }

    fn read(&self, path: &str) -> Result<Vec<u8>, String> {
        self.files
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, s)| s.as_bytes().to_vec())
            .ok_or_else(|| format!("not found: {path}"))
    }

    fn label(&self) -> Option<String> {
        (!self.label.is_empty()).then(|| self.label.to_string())
    }
}

fn tree(files: Vec<(&'static str, &'static str)>) -> MapFiles {
    MapFiles { files, label: "" }
}

fn run(m: &MapFiles) -> Assembled {
    let paths = m.list(".py").unwrap();
    assemble(parse_chunk(m, &paths, false))
}

fn node<'a>(a: &'a Assembled, key: &str) -> &'a Node {
    a.nodes.iter().find(|n| n.key == key).unwrap_or_else(|| {
        panic!(
            "no node {key} in {:?}",
            a.nodes.iter().map(|n| &n.key).collect::<Vec<_>>()
        )
    })
}

fn has_edge(a: &Assembled, src: &str, ty: &str, dst: &str) -> bool {
    a.edges
        .iter()
        .any(|e| e.src == src && e.ty == ty && e.dst == dst)
}

fn edge<'a>(a: &'a Assembled, ty: &str, dst: &str) -> &'a Edge {
    a.edges
        .iter()
        .find(|e| e.ty == ty && e.dst == dst)
        .unwrap_or_else(|| panic!("no {ty} edge to {dst}"))
}

fn note_containing<'a>(a: &'a Assembled, want: &str) -> &'a str {
    a.notes
        .iter()
        .find(|n| n.contains(want))
        .unwrap_or_else(|| panic!("no note containing {want:?} in {:?}", a.notes))
}

fn text(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

// ---- module identity ------------------------------------------------------

/// Module ids follow the language's own rule: `__init__.py` marks a package,
/// and the first directory without one is the sys.path root — so a `src/`
/// layout needs no special case at all.
#[test]
fn module_ids_follow_the_init_rule() {
    let a = run(&tree(vec![
        ("src/mypkg/__init__.py", "\"\"\"The package.\"\"\"\n"),
        ("src/mypkg/core/__init__.py", ""),
        ("src/mypkg/core/utils.py", "def parse_row():\n    pass\n"),
        ("scripts/deploy.py", "def main():\n    pass\n"),
    ]));
    node(&a, "mypkg.core.utils.parse_row");
    // A loose script is just its stem.
    node(&a, "deploy.main");
    // The package tree is CONTAINS, both ends parsed.
    assert!(has_edge(&a, "mypkg", "CONTAINS", "mypkg.core"));
    assert!(has_edge(&a, "mypkg.core", "CONTAINS", "mypkg.core.utils"));
    // The package's docstring lands on its __init__ module node.
    assert!(text(&node(&a, "mypkg").props["doc_comment"]).contains("The package"));
}

// ---- declarations ---------------------------------------------------------

/// Functions carry their signature as written, docstring as doc_comment.
#[test]
fn functions_carry_signature_and_docstring() {
    let a = run(&tree(vec![(
        "m.py",
        "async def fetch(url: str, timeout: float = 5.0) -> bytes:\n    \"\"\"Fetches a URL.\"\"\"\n    return b\"\"\n",
    )]));
    let f = node(&a, "m.fetch");
    assert_eq!(f.label, "Function");
    assert_eq!(
        text(&f.props["signature"]),
        "def fetch(url: str, timeout: float = 5.0) -> bytes"
    );
    assert_eq!(text(&f.props["doc_comment"]), "Fetches a URL.");
    assert_eq!(f.props["is_async"], Value::Bool(true));
    assert_eq!(text(&f.props["visibility"]), "exported");
}

/// A class declares its fields both ways Python writes them: class-level
/// annotations, and what `__init__` assigns onto `self`.
#[test]
fn a_class_has_fields_methods_and_self_calls() {
    let a = run(&tree(vec![(
        "db.py",
        r#"class Client:
    """A connection."""

    retries: int = 3
    _pool = None

    def __init__(self, url: str):
        self.url: str = url
        self.open = False

    def connect(self):
        self._log("open")

    def _log(self, msg):
        pass
"#,
    )]));
    let c = node(&a, "db.Client");
    assert_eq!(c.label, "Class");
    let fields: Vec<&str> = c.props["fields"]["$value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(fields, vec!["retries: int", "_pool", "url: str", "open"]);
    assert!(text(&c.props["doc_comment"]).contains("A connection"));

    assert!(has_edge(&a, "db.Client", "CONTAINS", "db.Client.connect"));
    assert_eq!(node(&a, "db.Client.connect").label, "Method");
    // `self._log(…)` resolves lexically — the one receiver a parser knows.
    assert!(has_edge(&a, "db.Client.connect", "CALLS", "db.Client._log"));
}

/// PEP 8's own rule sorts constants from variables, values as written.
#[test]
fn consts_and_vars_follow_pep8() {
    let a = run(&tree(vec![(
        "m.py",
        "MAX_RETRIES: int = 4 * 8\nregistry = {}\n",
    )]));
    let c = node(&a, "m.MAX_RETRIES");
    assert_eq!(c.label, "Const");
    assert_eq!(text(&c.props["signature"]), "int");
    assert_eq!(text(&c.props["value"]), "4 * 8");
    assert_eq!(node(&a, "m.registry").label, "Var");
}

/// The 3.12 `type` statement is a TypeAlias, the aliased type under
/// signature.
#[test]
fn type_statements_are_aliases() {
    let a = run(&tree(vec![("m.py", "type Pair = tuple[int, int]\n")]));
    let t = node(&a, "m.Pair");
    assert_eq!(t.label, "TypeAlias");
    assert_eq!(text(&t.props["signature"]), "tuple[int, int]");
}

/// `__all__` is the written export list; without it, Python's own
/// star-import rule (no leading underscore) decides.
#[test]
fn dunder_all_is_the_export_list() {
    let a = run(&tree(vec![
        (
            "a.py",
            "__all__ = [\"shown\"]\n\ndef shown():\n    pass\n\ndef also_public():\n    pass\n",
        ),
        (
            "b.py",
            "def public():\n    pass\n\ndef _hidden():\n    pass\n",
        ),
    ]));
    assert_eq!(text(&node(&a, "a.shown").props["visibility"]), "exported");
    assert!(!node(&a, "a.also_public").props.contains_key("visibility"));
    assert_eq!(text(&node(&a, "b.public").props["visibility"]), "exported");
    assert!(!node(&a, "b._hidden").props.contains_key("visibility"));
}

// ---- resolution -----------------------------------------------------------

/// `from pkg.mod import name` binds through the module set — aliased too.
#[test]
fn from_imports_resolve() {
    let a = run(&tree(vec![
        ("pkg/__init__.py", ""),
        ("pkg/util.py", "def fmt():\n    pass\n"),
        (
            "main.py",
            "from pkg.util import fmt as pretty\n\ndef run():\n    pretty()\n",
        ),
    ]));
    assert!(has_edge(&a, "main.run", "CALLS", "pkg.util.fmt"));
    assert!(has_edge(&a, "main", "IMPORTS", "pkg.util"));
}

/// `import pkg.util` binds the root; the chain walks the rest at the call.
#[test]
fn dotted_chains_walk_modules() {
    let a = run(&tree(vec![
        ("pkg/__init__.py", ""),
        ("pkg/util.py", "def fmt():\n    pass\n"),
        (
            "main.py",
            "import pkg.util\n\ndef run():\n    pkg.util.fmt()\n",
        ),
    ]));
    assert!(has_edge(&a, "main.run", "CALLS", "pkg.util.fmt"));
}

/// Relative imports are the package's own geometry — `from .` and `from ..`
/// resolve where the file sits.
#[test]
fn relative_imports_resolve() {
    let a = run(&tree(vec![
        ("pkg/__init__.py", ""),
        ("pkg/a.py", "def helper():\n    pass\n"),
        ("pkg/sub/__init__.py", ""),
        (
            "pkg/sub/b.py",
            "from .. import a\nfrom ..a import helper\n\ndef run():\n    helper()\n    a.helper()\n",
        ),
    ]));
    assert!(has_edge(&a, "pkg.sub.b.run", "CALLS", "pkg.a.helper"));
    assert!(has_edge(&a, "pkg.sub.b", "IMPORTS", "pkg.a"));
}

/// A star import reaches the target's export surface.
#[test]
fn star_imports_reach_the_export_surface() {
    let a = run(&tree(vec![
        (
            "util.py",
            "__all__ = [\"fmt\"]\n\ndef fmt():\n    pass\n\ndef _inner():\n    pass\n",
        ),
        ("main.py", "from util import *\n\ndef run():\n    fmt()\n"),
    ]));
    assert!(has_edge(&a, "main.run", "CALLS", "util.fmt"));
}

/// A call into a package this tree does not hold becomes an external
/// stand-in carrying the dotted path and nothing else.
#[test]
fn external_calls_become_stand_ins() {
    let a = run(&tree(vec![(
        "main.py",
        "import numpy as np\nfrom flask import Flask\n\ndef run():\n    np.zeros(3)\n    Flask(__name__)\n",
    )]));
    let z = node(&a, "numpy.zeros");
    assert_eq!(z.label, "Function");
    assert_eq!(z.extra_labels, vec!["External".to_string()]);
    assert!(has_edge(&a, "main.run", "CALLS", "flask.Flask"));
    assert_eq!(node(&a, "numpy").label, "Package");
    note_containing(&a, "other packages");
}

/// A method on a value is a checker's business — counted, never guessed;
/// a builtin is nobody's edge.
#[test]
fn member_calls_are_counted_and_builtins_skipped() {
    let a = run(&tree(vec![(
        "m.py",
        "def run(db):\n    db.query()\n    print(len([]))\n",
    )]));
    note_containing(&a, "left unresolved");
    // Counted — and now shown (P1): the receiver-typed call lands in the
    // unresolved ledger as a real node with the reason on the edge, while
    // the builtin still produces nothing at all.
    let ledger = edge(&a, "CALLS", "?::m.py::db.query");
    assert_eq!(
        ledger.props.get("_resolved_by"),
        Some(&Value::String("unresolved".into()))
    );
    assert!(node(&a, "?::m.py::db.query").label == "UnresolvedRef");
    assert!(
        !a.edges
            .iter()
            .any(|e| e.ty == "CALLS" && e.dst.contains("print")),
        "builtins earn no ledger entry"
    );
}

/// Bases are syntax: in-tree ones bind, foreign ones stand in as Classes,
/// and a subscripted base extends what it subscripts.
#[test]
fn class_bases_become_extends() {
    let a = run(&tree(vec![
        ("base.py", "class Base:\n    pass\n"),
        (
            "main.py",
            "from base import Base\nfrom pydantic import BaseModel\nfrom typing import Generic, TypeVar\n\nT = TypeVar(\"T\")\n\nclass Widget(Base):\n    pass\n\nclass Config(BaseModel):\n    pass\n\nclass Box(Generic[T]):\n    pass\n",
        ),
    ]));
    assert!(has_edge(&a, "main.Widget", "EXTENDS", "base.Base"));
    let bm = node(&a, "pydantic.BaseModel");
    assert_eq!(bm.label, "Class");
    assert!(bm.extra_labels.contains(&"External".to_string()));
    assert!(has_edge(&a, "main.Config", "EXTENDS", "pydantic.BaseModel"));
    assert!(has_edge(&a, "main.Box", "EXTENDS", "typing.Generic"));
}

/// A decorator is a call written down — `@app.route` names the router.
#[test]
fn decorators_are_calls() {
    let a = run(&tree(vec![
        ("deco.py", "def cached(f):\n    return f\n"),
        (
            "main.py",
            "from deco import cached\nfrom flask import Flask\n\napp = Flask(__name__)\n\n@cached\n@app.route(\"/x\")\ndef handler():\n    pass\n",
        ),
    ]));
    assert!(has_edge(&a, "main.handler", "CALLS", "deco.cached"));
    // `app` is a value, not an import — its route() is a checker's business.
    note_containing(&a, "left unresolved");
}

/// `if TYPE_CHECKING:` imports and `try:` fallbacks are module structure —
/// walked, not skipped.
#[test]
fn conditional_imports_are_seen() {
    let a = run(&tree(vec![
        ("t.py", "def target():\n    pass\n"),
        (
            "main.py",
            "try:\n    from t import target\nexcept ImportError:\n    target = None\n\ndef run():\n    target()\n",
        ),
    ]));
    assert!(has_edge(&a, "main.run", "CALLS", "t.target"));
}

// ---- provenance -----------------------------------------------------------

/// Every definition knows its file and line, and every written relation
/// knows the line it is written on.
#[test]
fn lines_and_files_are_recorded() {
    let a = run(&tree(vec![
        (
            "a.py",
            "from b import helper\n\ndef caller():\n    helper()\n", // def line 3, call line 4
        ),
        ("b.py", "def helper():\n    pass\n"),
    ]));
    let f = node(&a, "a.caller");
    assert_eq!(f.props["line"], Value::from(3u64));
    assert_eq!(text(&f.props["file"]), "a.py");
    assert_eq!(
        edge(&a, "CALLS", "b.helper").props["line"],
        Value::from(4u64)
    );
    assert_eq!(edge(&a, "IMPORTS", "b").props["line"], Value::from(1u64));
    assert_eq!(
        edge(&a, "CONTAINS", "a.caller").props["line"],
        Value::from(3u64)
    );
    let m = node(&a, "a");
    assert_eq!(text(&m.props["path"]), "a.py");
    assert!(!m.props.contains_key("line"));
}

// ---- robustness -----------------------------------------------------------

/// A file that will not parse is counted, and takes nothing else down.
#[test]
fn a_parse_error_is_counted_not_fatal() {
    let a = run(&tree(vec![
        ("ok.py", "def fine():\n    pass\n"),
        ("bad.py", "def broken(:\n"),
    ]));
    assert_eq!(a.skipped, 1);
    node(&a, "ok.fine");
}

/// The result must not depend on where the chunk boundaries fell.
#[test]
fn chunk_boundaries_do_not_change_the_result() {
    let m = tree(vec![
        ("pkg/__init__.py", ""),
        ("pkg/a.py", "from .b import b\n\ndef run():\n    b()\n"),
        ("pkg/b.py", "def b():\n    pass\n"),
    ]);
    let paths = m.list(".py").unwrap();

    let one = assemble(parse_chunk(&m, &paths, false));
    let mut split = Vec::new();
    for p in &paths {
        split.extend(parse_chunk(&m, std::slice::from_ref(p), false));
    }
    let other = assemble(split);

    let dump = |a: &Assembled| serde_json::to_string(&(&a.nodes, &a.edges, &a.notes)).unwrap();
    assert_eq!(dump(&one), dump(&other));
}

/// And the whole run is deterministic: twice in, byte-identical out.
#[test]
fn the_same_tree_twice_gives_the_same_facts() {
    let m = tree(vec![
        ("pkg/__init__.py", "from .core import Engine\n"),
        (
            "pkg/core.py",
            "class Engine:\n    def start(self):\n        self.warm()\n    def warm(self):\n        pass\n",
        ),
    ]);
    let paths = m.list(".py").unwrap();
    let dump = |a: &Assembled| serde_json::to_string(&(&a.nodes, &a.edges, &a.notes)).unwrap();
    let left = dump(&assemble(parse_chunk(&m, &paths, false)));
    let right = dump(&assemble(parse_chunk(&m, &paths, false)));
    assert_eq!(left, right);
}

/// A pushed document parses alone, its stem standing in for a module.
#[test]
fn a_document_parses_alone() {
    let facts = parse_document("snippet.py", b"def solo():\n    pass\n", false);
    let a = assemble(facts);
    node(&a, "snippet.solo");
}

/// include_source attaches the declaration as written under `_code`.
#[test]
fn include_source_attaches_the_declaration() {
    let m = tree(vec![("m.py", "def shown():\n    return 42\n")]);
    let paths = m.list(".py").unwrap();
    let a = assemble(parse_chunk(&m, &paths, true));
    let n = node(&a, "m.shown");
    assert!(
        text(&n.props["_code"]["$value"]).contains("return 42"),
        "{:?}",
        n.props
    );
}

// ---- P0 eval harness: known recall gaps, un-ignored as their phase lands.
// `just eval` runs these; CI's normal `cargo test` skips them.

/// hermes-agent's idiom, and the benchmark failure that started the revision:
/// every production caller of `resolve_anthropic_token` imports it inside the
/// calling function. The import table must include function-scoped imports.
#[test]
fn function_scoped_imports_resolve_calls() {
    let a = run(&tree(vec![
        ("pkg/__init__.py", ""),
        ("pkg/util.py", "def fmt(x):\n    return x\n"),
        (
            "main.py",
            "def run():\n    from pkg.util import fmt\n    return fmt(1)\n",
        ),
    ]));
    assert!(
        has_edge(&a, "main.run", "CALLS", "pkg.util.fmt"),
        "call through a function-scoped `from … import` must resolve"
    );
}

/// The `import x` (non-from) flavor of the same gap, plus attribute calls
/// through it.
#[test]
fn function_scoped_module_import_resolves_attribute_calls() {
    let a = run(&tree(vec![
        ("pkg/__init__.py", ""),
        ("pkg/util.py", "def fmt(x):\n    return x\n"),
        (
            "main.py",
            "def run():\n    import pkg.util\n    return pkg.util.fmt(1)\n",
        ),
    ]));
    assert!(has_edge(&a, "main.run", "CALLS", "pkg.util.fmt"));
}

/// mock.patch("pkg.util.fmt") names a symbol by string — 17 such sites were
/// impact-relevant in the benchmark. A string literal that is exactly a
/// known key's dotted form becomes a REFERENCES edge (never CALLS).
#[test]
fn string_names_of_known_symbols_become_references() {
    let a = run(&tree(vec![
        ("pkg/__init__.py", ""),
        ("pkg/util.py", "def fmt(x):\n    return x\n"),
        (
            "test_util.py",
            "def test_patched():\n    from unittest.mock import patch\n    with patch(\"pkg.util.fmt\"):\n        pass\n",
        ),
    ]));
    assert!(
        has_edge(&a, "test_util.test_patched", "REFERENCES", "pkg.util.fmt"),
        "a string naming a known symbol is an impact fact"
    );
}

/// The ledger key is an edge target; the implied-node pass must not mint a
/// second, bare node for it (the duplicate-key regression the hermes digest
/// surfaced).
#[test]
fn an_unresolved_ref_is_one_node_not_two() {
    let a = run(&tree(vec![("m.py", "def go(db):\n    db.query(1)\n")]));
    let hits = a
        .nodes
        .iter()
        .filter(|n| n.key == "?::m.py::db.query")
        .count();
    assert_eq!(hits, 1, "one UnresolvedRef, no implied double");
}

// ---- baseline Tier B: receiver typing, inheritance, class receivers ------
// (mined from codegraph + codebase-memory-mcp python resolution suites)

/// An annotated parameter names the receiver's class — and two same-named
/// methods never cross-attribute (the Logger/Other decoy discipline).
#[test]
fn annotated_params_type_receivers_without_crossing() {
    let a = run(&tree(vec![(
        "m.py",
        "class Logger:\n    def log(self):\n        return 1\n\nclass Other:\n    def log(self):\n        return 2\n\ndef use(lg: Logger):\n    return lg.log()\n\ndef use_other(o: Other):\n    return o.log()\n",
    )]));
    assert!(has_edge(&a, "m.use", "CALLS", "m.Logger.log"));
    assert!(has_edge(&a, "m.use_other", "CALLS", "m.Other.log"));
    assert!(!has_edge(&a, "m.use", "CALLS", "m.Other.log"));
    assert!(!has_edge(&a, "m.use_other", "CALLS", "m.Logger.log"));
}

/// A constructor result types the local: `f = Foo(); f.method()`.
#[test]
fn constructor_results_type_locals() {
    let a = run(&tree(vec![(
        "m.py",
        "class Foo:\n    def method(self):\n        return 1\n\ndef use():\n    f = Foo()\n    return f.method()\n",
    )]));
    assert!(has_edge(&a, "m.use", "CALLS", "m.Foo.method"));
}

/// A factory's declared return types the value it makes — including the
/// imported, cross-file form.
#[test]
fn declared_returns_type_factory_results() {
    let a = run(&tree(vec![
        (
            "lib.py",
            "class Conn:\n    def ping(self):\n        return 1\n\ndef make() -> Conn:\n    return Conn()\n",
        ),
        (
            "app.py",
            "from lib import make\n\ndef go():\n    c = make()\n    return c.ping()\n",
        ),
    ]));
    assert!(
        has_edge(&a, "app.go", "CALLS", "lib.Conn.ping"),
        "{:?}",
        a.edges
    );
}

/// `self.shared()` resolves through the declared bases when the own class
/// lacks the method.
#[test]
fn self_calls_walk_the_bases() {
    let a = run(&tree(vec![(
        "m.py",
        "class Base:\n    def shared(self):\n        return 1\n\nclass Child(Base):\n    def go(self):\n        return self.shared()\n",
    )]));
    assert!(has_edge(&a, "m.Child.go", "CALLS", "m.Base.shared"));
}

/// `super().greet()` starts at the bases — never the own class, even when
/// it defines the same name.
#[test]
fn super_calls_resolve_on_the_base() {
    let a = run(&tree(vec![(
        "m.py",
        "class Base:\n    def greet(self):\n        return 1\n\nclass Child(Base):\n    def greet(self):\n        return super().greet()\n",
    )]));
    assert!(
        has_edge(&a, "m.Child.greet", "CALLS", "m.Base.greet"),
        "{:?}",
        a.edges
    );
    assert!(!has_edge(&a, "m.Child.greet", "CALLS", "m.Child.greet"));
}

/// `self.cfg.display()` resolves when `__init__` states what `cfg` is —
/// here through an annotated parameter assigned onto self.
#[test]
fn init_assigned_attributes_type_self_chains() {
    let a = run(&tree(vec![(
        "m.py",
        "class Config:\n    def display(self):\n        return 1\n\nclass App:\n    def __init__(self, cfg: Config):\n        self.cfg = cfg\n\n    def use(self):\n        return self.cfg.display()\n",
    )]));
    assert!(
        has_edge(&a, "m.App.use", "CALLS", "m.Config.display"),
        "{:?}",
        a.edges
    );
}

/// A class-level annotation types `self.x.method()` the same way.
#[test]
fn class_annotations_type_self_chains() {
    let a = run(&tree(vec![(
        "m.py",
        "class Foo:\n    def method(self):\n        return 1\n\nclass C:\n    x: Foo\n\n    def use(self):\n        return self.x.method()\n",
    )]));
    assert!(has_edge(&a, "m.C.use", "CALLS", "m.Foo.method"));
}

/// A class receiver reaches classmethods/staticmethods: `C.make()`.
#[test]
fn class_receivers_reach_class_methods() {
    let a = run(&tree(vec![(
        "m.py",
        "class C:\n    @classmethod\n    def make(cls):\n        return cls()\n\ndef use():\n    return C.make()\n",
    )]));
    assert!(has_edge(&a, "m.use", "CALLS", "m.C.make"), "{:?}", a.edges);
}

/// The receiver stamps say which strategy bound the call.
#[test]
fn receiver_resolutions_are_stamped() {
    let a = run(&tree(vec![(
        "m.py",
        "class Logger:\n    def log(self):\n        return 1\n\ndef use(lg: Logger):\n    return lg.log()\n",
    )]));
    let e = a
        .edges
        .iter()
        .find(|e| e.src == "m.use" && e.dst == "m.Logger.log")
        .expect("edge");
    assert_eq!(
        e.props.get("_resolved_by").and_then(|v| v.as_str()),
        Some("receiver")
    );
}

/// C4 (narrow): a bare in-tree function — or class, the class-as-value
/// idiom — passed as a call argument is a REFERENCES fact; no self-loops.
#[test]
fn functions_passed_as_values_become_references() {
    let a = run(&tree(vec![(
        "m.py",
        "def handler():\n    pass\n\nclass Strategy:\n    pass\n\ndef accept(cb):\n    cb()\n\ndef wire():\n    accept(handler)\n    accept(Strategy)\n\ndef retry():\n    accept(retry)\n",
    )]));
    assert!(
        has_edge(&a, "m.wire", "REFERENCES", "m.handler"),
        "{:?}",
        a.edges
            .iter()
            .filter(|e| e.ty == "REFERENCES")
            .collect::<Vec<_>>()
    );
    assert!(has_edge(&a, "m.wire", "REFERENCES", "m.Strategy"));
    assert!(
        !has_edge(&a, "m.retry", "REFERENCES", "m.retry"),
        "no self-loop"
    );
}

/// The `concurrent` value on a CALLS edge, or "" when it carries none.
fn concurrency(a: &Assembled, src: &str, dst: &str) -> String {
    a.edges
        .iter()
        .find(|e| e.src == src && e.ty == "CALLS" && e.dst == dst)
        .and_then(|e| e.props.get("concurrent"))
        .and_then(|v| v.get("$value").cloned())
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Python has no channel, so the whole of its concurrency is where control
/// stops waiting — and that is written at the call site, not the
/// declaration. `is_async` says a function may suspend; only the call says
/// whether anyone waited. `await f()`, `create_task(f())` and a bare `f()`
/// were one indistinguishable edge.
#[test]
fn a_call_site_says_whether_anyone_waits_for_the_coroutine() {
    let t = tree(vec![(
        "app.py",
        r#"
import asyncio

async def fetch(u): return u
async def worker(): return 1
def plain(): return 2

async def main():
    await fetch("a")
    asyncio.create_task(worker())
    await asyncio.gather(fetch("b"), fetch("c"))
    asyncio.to_thread(plain)
"#,
    )]);
    let a = run(&t);
    // Awaited is the expectation inside an async body and says nothing;
    // marking every one would put a property on nearly every edge.
    assert_eq!(concurrency(&a, "app.main", "app.fetch"), "scheduled");
    assert_eq!(concurrency(&a, "app.main", "app.worker"), "scheduled");
    // Scheduling is evident from the syntax whatever the callee is — and a
    // scheduler handed the callable itself rather than a coroutine runs it
    // beside its caller though nothing in this body ever calls it.
    let handed = a
        .edges
        .iter()
        .find(|e| e.src == "app.main" && e.ty == "REFERENCES" && e.dst == "app.plain")
        .and_then(|e| e.props.get("concurrent"))
        .and_then(|v| v.get("$value").and_then(|v| v.as_str()));
    assert_eq!(handed, Some("scheduled"), "{:?}", a.edges);
}

/// A coroutine called as a statement is started and left. That is the
/// bug-prone shape, and it is the one the graph could not show at all.
#[test]
fn a_coroutine_nobody_awaits_is_marked_and_a_plain_call_is_not() {
    let t = tree(vec![(
        "app.py",
        r#"
async def fetch(u): return u
def log(m): return m

async def main():
    fetch("forgotten")
    log("fine")
    await fetch("waited")
"#,
    )]);
    let a = run(&t);
    // One caller reaches one callee two ways and they fold to one edge, so
    // the union is the honest statement — the awaited site must not swallow
    // the unawaited one merely by being written first.
    assert_eq!(concurrency(&a, "app.main", "app.fetch"), "unawaited");
    // A statement whose result is discarded is not concurrency when there
    // was nothing to await: `log("fine")` is simply a call.
    assert_eq!(concurrency(&a, "app.main", "app.log"), "");
}

/// A queue is where Python puts what Go puts in a channel, and
/// `q = asyncio.Queue()` states its type as plainly as a local class would.
/// Untyped, every `q.put(...)` and `q.get()` went to the ledger.
#[test]
fn a_local_typed_by_an_external_constructor_resolves_its_methods() {
    let t = tree(vec![(
        "app.py",
        r#"
import asyncio

async def consume(q: asyncio.Queue):
    return await q.get()

async def produce():
    q = asyncio.Queue()
    await q.put(1)
    return q
"#,
    )]);
    let a = run(&t);
    assert!(
        has_edge(&a, "app.produce", "CALLS", "asyncio.Queue.put"),
        "{:?}",
        a.edges
            .iter()
            .filter(|e| e.src == "app.produce")
            .map(|e| e.dst.as_str())
            .collect::<Vec<_>>()
    );
    // An annotated parameter is typed the same way, which is what joins the
    // consumer to the queue the producer filled.
    assert!(has_edge(&a, "app.consume", "CALLS", "asyncio.Queue.get"));
    // And nothing lands in the ledger for either.
    assert!(
        !a.nodes.iter().any(|n| n.label == "UnresolvedRef"),
        "{:?}",
        a.nodes.iter().map(|n| &n.key).collect::<Vec<_>>()
    );
}

/// Two rules, ranked: a filename is what the runners collect by default and
/// a config file can move it, so it is `strong`; a `unittest.TestCase`
/// subclass is unittest's own definition of a test, so it is `definitive`
/// and overwrites the weaker flag on the class it covers.
#[test]
fn test_code_is_flagged_by_filename_and_by_unittest_base() {
    let a = run(&tree(vec![
        ("pkg/__init__.py", ""),
        ("pkg/svc.py", "def helper():\n    return 1\n"),
        (
            "pkg/test_svc.py",
            "from pkg.svc import helper\n\ndef test_helper():\n    assert helper()\n",
        ),
        (
            "pkg/cases.py",
            "import unittest as ut\nfrom unittest import TestCase as TC\n\nclass Direct(ut.TestCase):\n    def test_one(self):\n        pass\n\nclass Aliased(TC):\n    def test_two(self):\n        pass\n\nclass NotATest:\n    def run(self):\n        pass\n",
        ),
        (
            "pkg/decoy.py",
            "class TestCase:\n    pass\n\nclass Mine(TestCase):\n    def go(self):\n        pass\n",
        ),
    ]));

    // Filename evidence covers the module and everything in it.
    for key in ["pkg.test_svc", "pkg.test_svc.test_helper"] {
        assert_eq!(
            node(&a, key).props["test_flag"]["$value"],
            Value::from("filename")
        );
        assert_eq!(
            node(&a, key).props["_test_flag_confidence"],
            Value::from("strong")
        );
    }

    // Base-class evidence covers the class and its methods, in a file whose
    // name says nothing.
    for key in [
        "pkg.cases.Direct",
        "pkg.cases.Direct.test_one",
        "pkg.cases.Aliased",
        "pkg.cases.Aliased.test_two",
    ] {
        assert_eq!(
            node(&a, key).props["test_flag"]["$value"],
            Value::from("base-class"),
            "{key}"
        );
        assert_eq!(
            node(&a, key).props["_test_flag_confidence"],
            Value::from("definitive"),
            "{key}"
        );
    }

    // A local class named TestCase is somebody else's TestCase.
    for key in [
        "pkg.svc",
        "pkg.svc.helper",
        "pkg.cases.NotATest",
        "pkg.decoy.Mine",
    ] {
        assert!(
            !node(&a, key).props.contains_key("test_flag"),
            "{key} must not be flagged"
        );
    }
}

/// A declared type is a dependency as surely as a call is — where Python
/// wrote the annotation. Fields, parameters and returns say so, through
/// subscripts and unions, folded to one edge per pair.
#[test]
fn annotated_types_become_uses_type_edges() {
    let a = run(&tree(vec![
        ("pkg/__init__.py", ""),
        ("pkg/job.py", "class Job:\n    pass\n"),
        ("pkg/cfg.py", "class Cfg:\n    pass\n"),
        (
            "pkg/pool.py",
            "from pkg.job import Job\nfrom pkg.cfg import Cfg\n\nclass Pool:\n    queue: list[Job]\n    index: dict[str, Cfg]\n    label: str\n\ndef work(c: Cfg) -> Job:\n    return Job()\n\ndef round_trip(c: Cfg) -> Cfg:\n    return c\n\ndef loose(x):\n    return x\n",
        ),
    ]));

    let role = |src: &str, dst: &str| -> Option<String> {
        a.edges
            .iter()
            .find(|e| e.ty == "USES_TYPE" && e.src == src && e.dst == dst)
            .map(|e| e.props["role"]["$value"].as_str().unwrap().to_string())
    };

    // Through a subscript, which is how annotated Python names a collection.
    assert_eq!(
        role("pkg.pool.Pool", "pkg.job.Job").as_deref(),
        Some("field")
    );
    assert_eq!(
        role("pkg.pool.Pool", "pkg.cfg.Cfg").as_deref(),
        Some("field")
    );
    assert_eq!(
        role("pkg.pool.work", "pkg.cfg.Cfg").as_deref(),
        Some("param")
    );
    assert_eq!(
        role("pkg.pool.work", "pkg.job.Job").as_deref(),
        Some("return")
    );
    assert_eq!(
        role("pkg.pool.round_trip", "pkg.cfg.Cfg").as_deref(),
        Some("param, return")
    );

    // An unannotated parameter states no type — and no absence of one.
    assert!(
        !a.edges
            .iter()
            .any(|e| e.ty == "USES_TYPE" && e.src == "pkg.pool.loose"),
        "nothing was written, so nothing is claimed"
    );
    for e in a.edges.iter().filter(|e| e.ty == "USES_TYPE") {
        assert_ne!(e.src, e.dst, "never a self-loop");
        for builtin in ["str", "list", "dict"] {
            assert!(
                !e.dst.ends_with(&format!(".{builtin}")),
                "{} is a builtin and nobody's edge",
                e.dst
            );
        }
    }
}

/// `from asyncio import Queue` binds the type itself, so the annotation is
/// written as one bare segment. Reading it through the imports must reach
/// `asyncio.Queue` all the same — the middle segments a dotted path walks
/// are simply none of them.
#[test]
fn a_bare_name_bound_by_a_from_import_types_its_receiver() {
    let t = tree(vec![(
        "app.py",
        r#"
from asyncio import Queue

async def consume(q: Queue):
    return await q.get()
"#,
    )]);
    let a = run(&t);
    assert!(
        has_edge(&a, "app.consume", "CALLS", "asyncio.Queue.get"),
        "{:?}",
        a.edges
            .iter()
            .filter(|e| e.src == "app.consume")
            .map(|e| e.dst.as_str())
            .collect::<Vec<_>>()
    );
}

// ---- the routes a call asks for -------------------------------------------

/// The routes a function asks for, as the property records them.
fn requests(a: &Assembled, key: &str) -> Vec<String> {
    node(a, key)
        .props
        .get("requests")
        .and_then(|v| v.get("$value"))
        .and_then(|v| v.as_array())
        .map(|xs| {
            xs.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Every client spelling, and the one that matters most: a project-local
/// wrapper that names no library this reader could have been taught.
#[test]
fn a_call_with_a_path_argument_asks_for_a_route() {
    let a = run(&tree(vec![(
        "api.py",
        "import requests\n\
         \n\
         def via_requests(order_id):\n\
         \x20   return requests.get(f\"/api/orders/{order_id}\")\n\
         \n\
         def via_session(s):\n\
         \x20   return s.request(\"POST\", \"/api/orders\")\n\
         \n\
         def via_kwargs(s):\n\
         \x20   return s.request(method=\"DELETE\", url=\"/api/orders/1\")\n\
         \n\
         def via_wrapper():\n\
         \x20   return fetch_json(\"/bridge/check/1\")\n",
    )]));
    assert_eq!(
        requests(&a, "api.via_requests"),
        vec!["GET /api/orders/{}".to_string()]
    );
    assert_eq!(
        requests(&a, "api.via_session"),
        vec!["POST /api/orders".to_string()],
        "the verb is positional, which is the only shape the name hides"
    );
    assert_eq!(
        requests(&a, "api.via_kwargs"),
        vec!["DELETE /api/orders/1".to_string()],
        "keyword arguments state both"
    );
    assert_eq!(
        requests(&a, "api.via_wrapper"),
        vec!["* /bridge/check/1".to_string()],
        "the wrapper is visible because the argument is"
    );
}

/// An f-string's hole is kept, which is what lets it compare equal to a
/// declaration's `{id}`.
#[test]
fn an_f_string_path_keeps_its_hole() {
    let a = run(&tree(vec![(
        "api.py",
        "def one(order_id):\n\
         \x20   return httpx.get(f\"/api/orders/{order_id}/items\")\n",
    )]));
    assert_eq!(
        requests(&a, "api.one"),
        vec!["GET /api/orders/{}/items".to_string()]
    );
    assert_eq!(
        text(&node(&a, "api.one").props["requests_as_written"]["$value"][0]),
        "/api/orders/${}/items",
        "the source's own spelling, in the convention's hole"
    );
}

/// An absolute URL names the same route as the path inside it, and a query
/// says nothing about which handler serves it.
#[test]
fn an_absolute_url_and_a_query_reduce_to_the_path() {
    let a = run(&tree(vec![(
        "api.py",
        "def go():\n\
         \x20   return httpx.get(\"https://api.example.com/v1/orders?page=2\")\n",
    )]));
    assert_eq!(requests(&a, "api.go"), vec!["GET /v1/orders".to_string()]);
}

/// A string that is not path-shaped is not a route, and a function that asks
/// for nothing carries no property at all.
#[test]
fn only_a_path_shaped_argument_counts() {
    let a = run(&tree(vec![(
        "api.py",
        "def plain():\n\
         \x20   return compute(\"orders\", 2)\n\
         \n\
         def logs():\n\
         \x20   print(\"a b c\")\n",
    )]));
    assert!(requests(&a, "api.plain").is_empty());
    assert!(requests(&a, "api.logs").is_empty());
    assert!(
        !node(&a, "api.plain").props.contains_key("requests"),
        "no property rather than an empty one"
    );
}

/// The account says what was recorded, since a request is only half of a
/// route and the other half is matched later.
#[test]
fn the_requests_are_counted_in_the_report() {
    let a = run(&tree(vec![(
        "api.py",
        "def go():\n\
         \x20   return httpx.get(\"/api/orders\")\n",
    )]));
    assert!(
        a.notes.iter().any(|n| n.contains("route(s) asked for")),
        "{:?}",
        a.notes
    );
}

// ---- the routes a decorator declares --------------------------------------

/// The routes a function serves, as the property records them.
fn serves(a: &Assembled, key: &str) -> Vec<String> {
    node(a, key)
        .props
        .get("serves")
        .and_then(|v| v.get("$value"))
        .and_then(|v| v.as_array())
        .map(|xs| {
            xs.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// FastAPI and Flask write a route the same way, and the verb is read from
/// whichever of the two places states it.
#[test]
fn a_decorated_function_declares_what_it_serves() {
    let a = run(&tree(vec![(
        "api.py",
        "@app.get(\"/items/{item_id}\")\n\
         def read_item(item_id: int):\n\
         \x20   return item_id\n\
         \n\
         @router.delete(\"/items/{item_id}\")\n\
         def drop(item_id: int):\n\
         \x20   return None\n\
         \n\
         @app.route(\"/legacy\", methods=[\"POST\", \"PUT\"])\n\
         def legacy():\n\
         \x20   return None\n\
         \n\
         @app.route(\"/bridge/check\")\n\
         def check():\n\
         \x20   return None\n\
         \n\
         @lru_cache(maxsize=8)\n\
         def helper():\n\
         \x20   return None\n",
    )]));
    assert_eq!(
        serves(&a, "api.read_item"),
        vec!["GET /items/{}".to_string()],
        "FastAPI's own spelling of a path variable"
    );
    assert_eq!(
        serves(&a, "api.drop"),
        vec!["DELETE /items/{}".to_string()],
        "a router declares like an app"
    );
    assert_eq!(
        serves(&a, "api.legacy"),
        vec!["POST /legacy".to_string(), "PUT /legacy".to_string()],
        "Flask states its verbs in a keyword, and each one is a route"
    );
    assert_eq!(
        serves(&a, "api.check"),
        vec!["* /bridge/check".to_string()],
        "`route` with no methods names no verb, and `*` matches every one"
    );
    assert!(
        !node(&a, "api.helper").props.contains_key("serves"),
        "a decorator that carries no path declares no route"
    );
}

/// `patch` names a verb and `mock.patch` is everywhere; its argument is a
/// dotted name, not a path, and strict path-shape is what keeps it out.
#[test]
fn a_mock_patch_target_is_not_a_route() {
    let a = run(&tree(vec![(
        "test_api.py",
        "@patch(\"pkg.mod.fetch\")\n\
         def test_one(m):\n\
         \x20   return m\n",
    )]));
    assert!(
        serves(&a, "test_api.test_one").is_empty(),
        "a module path is no route"
    );
}

/// The source's own spelling is kept beside the comparable form, and the
/// account says what was declared.
#[test]
fn the_declarations_are_written_and_counted() {
    let a = run(&tree(vec![(
        "api.py",
        "@app.get(\"/items/{item_id}\")\n\
         def one(item_id: int):\n\
         \x20   return item_id\n",
    )]));
    assert_eq!(
        text(&node(&a, "api.one").props["serves_as_written"]["$value"][0]),
        "/items/{item_id}",
        "FastAPI's own spelling"
    );
    assert!(
        a.notes.iter().any(|n| n.contains("declared by a handler")),
        "{:?}",
        a.notes
    );
}

/// A Django URL conf hands each path to a view, and that view is what serves
/// it — written with no leading slash, which only the handing makes readable
/// as a path at all.
#[test]
fn a_url_conf_names_the_view_that_serves_each_path() {
    let a = run(&tree(vec![
        ("app/__init__.py", ""),
        (
            "app/views.py",
            "def one(request, order_id):\n\
             \x20   return order_id\n\
             \n\
             class Detail:\n\
             \x20   def get(self, request):\n\
             \x20       return None\n",
        ),
        (
            "app/urls.py",
            "from django.urls import path\n\
             \n\
             from . import views\n\
             from .views import Detail\n\
             \n\
             urlpatterns = [\n\
             \x20   path(\"orders/<int:order_id>\", views.one),\n\
             \x20   path(\"detail/\", Detail.as_view()),\n\
             \x20   path(\"\", views.one),\n\
             ]\n",
        ),
    ]));
    assert_eq!(
        serves(&a, "app.views.one"),
        vec!["* /".to_string(), "* /orders/{}".to_string()],
        "the view is named, not the module that registered it"
    );
    assert_eq!(
        serves(&a, "app.views.Detail"),
        vec!["* /detail".to_string()],
        "a class-based view is the class the `as_view()` call is made on"
    );
}

/// A module-level registry that is not a router declares nothing: a bare word
/// is no path, however it is handed on.
#[test]
fn a_registry_that_is_no_router_declares_nothing() {
    let a = run(&tree(vec![
        ("app/__init__.py", ""),
        (
            "app/registry.py",
            "def write_csv(rows):\n\
         \x20   return rows\n\
         \n\
         WRITERS = [\n\
         \x20   register(\"csv\", write_csv),\n\
         ]\n",
        ),
    ]));
    assert!(
        serves(&a, "app.registry.write_csv").is_empty(),
        "`csv` has no separator and no converter, so it is a word"
    );
}

/// A view this tree does not declare leaves the route on the module that
/// mounted it — the only symbol there is, rather than a guess.
#[test]
fn an_unresolvable_view_leaves_the_route_on_its_module() {
    let a = run(&tree(vec![
        ("app/__init__.py", ""),
        (
            "app/urls.py",
            "urlpatterns = [\n\
             \x20   path(\"health/\", elsewhere.check),\n\
             ]\n",
        ),
    ]));
    assert_eq!(serves(&a, "app.urls"), vec!["* /health".to_string()]);
}

// ---- a router's and a client's own path ------------------------------------

/// A router built with a prefix declares its routes under it, and a client
/// built with a base URL asks for its paths under that — one rule, read from
/// the keyword rather than from the type's name.
#[test]
fn a_prefix_reaches_the_routes_stated_through_it() {
    let a = run(&tree(vec![
        ("app/__init__.py", ""),
        (
            "app/orders.py",
            "from fastapi import APIRouter\n\
             \n\
             router = APIRouter(prefix=\"/api/orders\")\n\
             \n\
             @router.get(\"/{order_id}\")\n\
             def one(order_id: int):\n\
             \x20   return order_id\n\
             \n\
             @router.post(\"/\")\n\
             def create():\n\
             \x20   return None\n",
        ),
        (
            "app/legacy.py",
            "from flask import Blueprint\n\
             \n\
             bp = Blueprint(\"legacy\", __name__, url_prefix=\"/legacy\")\n\
             \n\
             @bp.route(\"/ping\", methods=[\"GET\"])\n\
             def ping():\n\
             \x20   return None\n",
        ),
        (
            "app/client.py",
            "import httpx\n\
             \n\
             client = httpx.Client(base_url=\"/api\")\n\
             \n\
             def fetch_orders():\n\
             \x20   return client.get(\"/orders\")\n",
        ),
    ]));
    assert_eq!(
        serves(&a, "app.orders.one"),
        vec!["GET /api/orders/{}".to_string()],
        "FastAPI's router prefix joins the path its decorator states"
    );
    assert_eq!(
        serves(&a, "app.orders.create"),
        vec!["POST /api/orders".to_string()],
        "a trailing slash on the path adds nothing to the prefix"
    );
    assert_eq!(
        serves(&a, "app.legacy.ping"),
        vec!["GET /legacy/ping".to_string()],
        "a Flask blueprint states its prefix the same way"
    );
    assert_eq!(
        requests(&a, "app.client.fetch_orders"),
        vec!["GET /api/orders".to_string()],
        "and a client's base URL is the same fact on the other side"
    );
}

/// A prefix written as a name resolves to the binding behind it, in this
/// module or the one it was imported from — and a router imported from
/// elsewhere carries its prefix with it.
#[test]
fn a_prefix_written_as_a_name_resolves_across_modules() {
    let a = run(&tree(vec![
        ("app/__init__.py", ""),
        ("app/config.py", "API_PREFIX = \"/api/v2\"\n"),
        (
            "app/routers.py",
            "from fastapi import APIRouter\n\
             \n\
             from .config import API_PREFIX\n\
             \n\
             router = APIRouter(prefix=API_PREFIX)\n",
        ),
        (
            "app/orders.py",
            "from .routers import router\n\
             \n\
             @router.get(\"/orders\")\n\
             def all_orders():\n\
             \x20   return []\n",
        ),
    ]));
    assert_eq!(
        serves(&a, "app.orders.all_orders"),
        vec!["GET /api/v2/orders".to_string()],
        "the prefix crossed two modules to reach the path"
    );
}

/// A name nothing states a prefix for leaves the path whole, rather than
/// guessing one.
#[test]
fn a_name_with_no_prefix_leaves_the_path_whole() {
    let a = run(&tree(vec![(
        "api.py",
        "app = FastAPI()\n\
         \n\
         @app.get(\"/health\")\n\
         def health():\n\
         \x20   return None\n",
    )]));
    assert_eq!(serves(&a, "api.health"), vec!["GET /health".to_string()]);
}
