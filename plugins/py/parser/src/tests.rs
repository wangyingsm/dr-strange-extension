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
