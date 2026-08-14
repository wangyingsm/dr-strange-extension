//! The parser's own tests, running natively — the reason it is a plain
//! library under the wasm component rather than one crate with it.

use super::*;

struct MapFiles {
    files: Vec<(&'static str, &'static str)>,
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
        None
    }
}

fn run(files: Vec<(&'static str, &'static str)>) -> Assembled {
    let m = MapFiles { files };
    let paths = m.list(".java").unwrap();
    assemble(parse_chunk(&m, &paths, false))
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

// ---- keys and shapes ------------------------------------------------------

/// Keys are Java's own qualified names: package, type, member — and the
/// package contains its types, packages nest where both were parsed.
#[test]
fn keys_are_qualified_names() {
    let a = run(vec![
        (
            "core/Engine.java",
            "package com.acme.core;\n\npublic class Engine {\n    public void start() {}\n}\n",
        ),
        ("acme/Root.java", "package com.acme;\n\nclass Root {}\n"),
    ]);
    node(&a, "com.acme.core.Engine");
    node(&a, "com.acme.core.Engine.start");
    assert!(has_edge(
        &a,
        "com.acme.core",
        "CONTAINS",
        "com.acme.core.Engine"
    ));
    assert!(has_edge(&a, "com.acme", "CONTAINS", "com.acme.core"));
    assert_eq!(node(&a, "com.acme.core").label, "Package");
}

/// A class carries javadoc, visibility, fields as `name: type`, and its
/// methods with signatures as written.
#[test]
fn a_class_has_fields_methods_and_docs() {
    let a = run(vec![(
        "Client.java",
        r#"package db;

/** A connection. */
public class Client {
    private String url;
    static final int RETRIES = 3;

    /** Opens. */
    public void connect(int timeout) {
        this.log("open");
    }

    private void log(String msg) {}
}
"#,
    )]);
    let c = node(&a, "db.Client");
    assert_eq!(c.label, "Class");
    assert_eq!(text(&c.props["visibility"]), "public");
    assert!(text(&c.props["doc_comment"]).contains("A connection"));
    let fields: Vec<&str> = c.props["fields"]["$value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(fields, vec!["url: String", "RETRIES: int"]);

    let m = node(&a, "db.Client.connect");
    assert_eq!(m.label, "Method");
    assert_eq!(text(&m.props["signature"]), "void connect(int timeout)");
    assert_eq!(text(&m.props["doc_comment"]), "Opens.");
    assert!(has_edge(&a, "db.Client", "CONTAINS", "db.Client.connect"));
    // `this.log(…)` is the class's own surface.
    assert!(has_edge(&a, "db.Client.connect", "CALLS", "db.Client.log"));
}

/// An interface demands its methods — HAS_METHOD, the family's convention.
#[test]
fn an_interface_demands_its_methods() {
    let a = run(vec![(
        "Store.java",
        "package db;\n\npublic interface Store {\n    /** Fetches. */\n    byte[] get(String key);\n}\n",
    )]);
    let m = node(&a, "db.Store.get");
    assert_eq!(m.label, "Method");
    assert!(has_edge(&a, "db.Store", "HAS_METHOD", "db.Store.get"));
    assert_eq!(text(&m.props["doc_comment"]), "Fetches.");
}

/// Enum constants are the enum, and a record declares its fields in its
/// header.
#[test]
fn enums_and_records_keep_their_shape() {
    let a = run(vec![
        (
            "Level.java",
            "package m;\n\npublic enum Level {\n    DEBUG, WARN, ERROR;\n\n    public boolean loud() { return this == ERROR; }\n}\n",
        ),
        (
            "Point.java",
            "package m;\n\npublic record Point(int x, int y) {}\n",
        ),
    ]);
    let e = node(&a, "m.Level");
    assert_eq!(e.label, "Enum");
    let variants: Vec<&str> = e.props["variants"]["$value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(variants, vec!["DEBUG", "WARN", "ERROR"]);
    node(&a, "m.Level.loud");

    let r = node(&a, "m.Point");
    assert_eq!(r.label, "Record");
    let fields: Vec<&str> = r.props["fields"]["$value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(fields, vec!["x: int", "y: int"]);
}

/// A nested type chains its key through the outer one.
#[test]
fn nested_types_chain_their_keys() {
    let a = run(vec![(
        "Outer.java",
        "package m;\n\npublic class Outer {\n    public static class Inner {\n        void go() {}\n    }\n}\n",
    )]);
    node(&a, "m.Outer.Inner");
    node(&a, "m.Outer.Inner.go");
    assert!(has_edge(&a, "m.Outer", "CONTAINS", "m.Outer.Inner"));
}

// ---- resolution -----------------------------------------------------------

/// Same-package types resolve with no import at all — that is Java's rule.
#[test]
fn same_package_types_resolve_without_imports() {
    let a = run(vec![
        (
            "A.java",
            "package m;\n\nclass A {\n    void go() {\n        Helper.assist();\n        new Helper();\n    }\n}\n",
        ),
        (
            "Helper.java",
            "package m;\n\nclass Helper {\n    static void assist() {}\n}\n",
        ),
    ]);
    assert!(has_edge(&a, "m.A.go", "CALLS", "m.Helper.assist"));
    assert!(has_edge(&a, "m.A.go", "CALLS", "m.Helper"));
}

/// Imports bind type references across packages; wildcards resolve against
/// what this tree actually holds.
#[test]
fn imports_and_wildcards_resolve() {
    let a = run(vec![
        (
            "app/Main.java",
            "package app;\n\nimport lib.Helper;\nimport util.*;\n\nclass Main {\n    void go() {\n        Helper.assist();\n        Tool.run();\n    }\n}\n",
        ),
        (
            "lib/Helper.java",
            "package lib;\n\npublic class Helper {\n    public static void assist() {}\n}\n",
        ),
        (
            "util/Tool.java",
            "package util;\n\npublic class Tool {\n    public static void run() {}\n}\n",
        ),
    ]);
    assert!(has_edge(&a, "app.Main.go", "CALLS", "lib.Helper.assist"));
    assert!(has_edge(&a, "app.Main.go", "CALLS", "util.Tool.run"));
    assert!(has_edge(&a, "app.Main", "IMPORTS", "lib.Helper"));
}

/// A static import binds the method name itself.
#[test]
fn static_imports_bind_methods() {
    let a = run(vec![
        (
            "Main.java",
            "package app;\n\nimport static lib.Asserts.check;\n\nclass Main {\n    void go() {\n        check();\n    }\n}\n",
        ),
        (
            "Asserts.java",
            "package lib;\n\npublic class Asserts {\n    public static void check() {}\n}\n",
        ),
    ]);
    assert!(has_edge(&a, "app.Main.go", "CALLS", "lib.Asserts.check"));
}

/// Inherited methods resolve up the in-tree extends chain — `helper()` with
/// no receiver, and `super.helper()` alike.
#[test]
fn inherited_calls_walk_the_extends_chain() {
    let a = run(vec![
        (
            "Base.java",
            "package m;\n\nclass Base {\n    void helper() {}\n}\n",
        ),
        (
            "Sub.java",
            "package m;\n\nclass Sub extends Base {\n    void go() {\n        helper();\n        super.helper();\n    }\n}\n",
        ),
    ]);
    assert!(has_edge(&a, "m.Sub", "EXTENDS", "m.Base"));
    assert!(has_edge(&a, "m.Sub.go", "CALLS", "m.Base.helper"));
}

/// Calls into libraries this tree does not hold become external stand-ins;
/// `java.lang` needs no import to be known.
#[test]
fn external_calls_become_stand_ins() {
    let a = run(vec![(
        "Main.java",
        "package app;\n\nimport com.google.gson.Gson;\n\nclass Main {\n    void go() {\n        new Gson();\n        System.currentTimeMillis();\n        var s = String.valueOf(1);\n    }\n}\n",
    )]);
    let g = node(&a, "com.google.gson.Gson");
    assert_eq!(g.label, "Class");
    assert!(g.extra_labels.contains(&"External".to_string()));
    assert!(has_edge(
        &a,
        "app.Main.go",
        "CALLS",
        "java.lang.System.currentTimeMillis"
    ));
    assert!(has_edge(
        &a,
        "app.Main.go",
        "CALLS",
        "java.lang.String.valueOf"
    ));
    note_containing(&a, "other libraries");
}

/// A lowercase receiver is a value — a compiler's business, counted.
#[test]
fn value_receivers_are_counted_unresolved() {
    let a = run(vec![(
        "Main.java",
        "package app;\n\nclass Main {\n    void go(Object db) {\n        db.hashCode();\n    }\n}\n",
    )]);
    note_containing(&a, "left unresolved");
}

/// extends/implements resolve like any type reference — generic bases
/// extend what they subscript, foreign ones stand in with their kind.
#[test]
fn extends_and_implements_resolve() {
    let a = run(vec![
        (
            "Shape.java",
            "package m;\n\npublic interface Shape {\n    double area();\n}\n",
        ),
        (
            "Circle.java",
            "package m;\n\nimport java.util.ArrayList;\n\npublic class Circle extends ArrayList<Double> implements Shape {\n    public double area() { return 0; }\n}\n",
        ),
    ]);
    assert!(has_edge(&a, "m.Circle", "IMPLEMENTS", "m.Shape"));
    assert!(has_edge(&a, "m.Circle", "EXTENDS", "java.util.ArrayList"));
    let ext = node(&a, "java.util.ArrayList");
    assert!(ext.extra_labels.contains(&"External".to_string()));
}

/// An annotation is a reference written down — resolved like a type, with
/// `java.lang`'s own markers left out as noise.
#[test]
fn annotations_are_recorded_but_not_the_noise() {
    let a = run(vec![(
        "Api.java",
        "package web;\n\nimport org.spring.GetMapping;\n\nclass Api {\n    @Override\n    public String toString() { return \"\"; }\n\n    @GetMapping\n    void list() {}\n}\n",
    )]);
    assert!(has_edge(
        &a,
        "web.Api.list",
        "ANNOTATED_BY",
        "org.spring.GetMapping"
    ));
    assert_eq!(node(&a, "org.spring.GetMapping").label, "Annotation");
    assert!(!a.edges.iter().any(|e| e.dst.contains("Override")));
}

// ---- provenance and robustness --------------------------------------------

/// Every definition knows its file and line, and every written relation
/// knows the line it is written on.
#[test]
fn lines_and_files_are_recorded() {
    let a = run(vec![
        (
            "A.java",
            "package m;\n\nimport lib.B;\n\nclass A {\n    void go() {\n        B.help();\n    }\n}\n",
        ),
        (
            "B.java",
            "package lib;\n\npublic class B {\n    public static void help() {}\n}\n",
        ),
    ]);
    let f = node(&a, "m.A.go");
    assert_eq!(f.props["line"], Value::from(6u64));
    assert_eq!(text(&f.props["file"]), "A.java");
    assert_eq!(
        edge(&a, "CALLS", "lib.B.help").props["line"],
        Value::from(7u64)
    );
    assert_eq!(
        edge(&a, "IMPORTS", "lib.B").props["line"],
        Value::from(3u64)
    );
}

/// A file that will not parse is counted, and takes nothing else down.
#[test]
fn a_parse_error_is_counted_not_fatal() {
    let a = run(vec![
        (
            "Ok.java",
            "package m;\n\nclass Ok {\n    void fine() {}\n}\n",
        ),
        ("Bad.java", "class {{{{"),
    ]);
    assert_eq!(a.skipped, 1);
    node(&a, "m.Ok.fine");
}

/// The result must not depend on where the chunk boundaries fell, and the
/// same tree twice gives the same facts.
#[test]
fn deterministic_and_chunk_independent() {
    let files = vec![
        (
            "A.java",
            "package m;\n\nclass A extends B {\n    void go() { helper(); }\n}\n",
        ),
        (
            "B.java",
            "package m;\n\nclass B {\n    void helper() {}\n}\n",
        ),
    ];
    let m = MapFiles { files };
    let paths = m.list(".java").unwrap();

    let one = assemble(parse_chunk(&m, &paths, false));
    let mut split = Vec::new();
    for p in &paths {
        split.extend(parse_chunk(&m, std::slice::from_ref(p), false));
    }
    let other = assemble(split);
    let again = assemble(parse_chunk(&m, &paths, false));

    let dump = |a: &Assembled| serde_json::to_string(&(&a.nodes, &a.edges, &a.notes)).unwrap();
    assert_eq!(dump(&one), dump(&other));
    assert_eq!(dump(&one), dump(&again));
}

/// A pushed document parses alone.
#[test]
fn a_document_parses_alone() {
    let facts = parse_document(
        "Snippet.java",
        b"package s;\n\nclass Snippet {\n    void solo() {}\n}\n",
        false,
    );
    let a = assemble(facts);
    node(&a, "s.Snippet.solo");
}

/// include_source attaches the declaration as written under `_code`.
#[test]
fn include_source_attaches_the_declaration() {
    let m = MapFiles {
        files: vec![(
            "M.java",
            "package m;\n\nclass M {\n    int shown() { return 42; }\n}\n",
        )],
    };
    let paths = m.list(".java").unwrap();
    let a = assemble(parse_chunk(&m, &paths, true));
    let n = node(&a, "m.M.shown");
    assert!(
        text(&n.props["_code"]["$value"]).contains("return 42"),
        "{:?}",
        n.props
    );
}
