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

/// A class's interfaces are ancestry too: an interface's `default` answers an
/// inherited call.
#[test]
fn an_interface_default_method_answers_an_inherited_call() {
    let a = run(vec![
        (
            "Greeter.java",
            "package m;\n\ninterface Greeter {\n    default void hello() {}\n}\n",
        ),
        (
            "Host.java",
            "package m;\n\nclass Host implements Greeter {\n    void go() {\n        hello();\n    }\n}\n",
        ),
    ]);
    assert!(has_edge(&a, "m.Host", "IMPLEMENTS", "m.Greeter"));
    assert!(has_edge(&a, "m.Host.go", "CALLS", "m.Greeter.hello"));
}

/// `super.m()` is the superclass's, never an interface's — Java spells the
/// other one `Greeter.super.m()`. An external superclass leaves it unresolved
/// rather than binding to the interface that happens to declare the name.
#[test]
fn super_does_not_reach_an_implemented_interface() {
    let a = run(vec![
        (
            "Greeter.java",
            "package m;\n\ninterface Greeter {\n    default void hello() {}\n}\n",
        ),
        (
            "Host.java",
            "package m;\n\nimport java.util.ArrayList;\n\nclass Host extends ArrayList<String> implements Greeter {\n    void go() {\n        super.hello();\n    }\n}\n",
        ),
    ]);
    assert!(!has_edge(&a, "m.Host.go", "CALLS", "m.Greeter.hello"));
    note_containing(&a, "left unresolved");
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

/// An annotation's arguments ride its edge as written — on a Spring codebase
/// the argument is the fact (`@Qualifier("fast")`, a route), and what it means
/// is the framework's business, not a parser's.
#[test]
fn an_annotation_keeps_its_arguments_as_written() {
    let a = run(vec![(
        "Api.java",
        "package web;\n\nimport org.spring.PostMapping;\nimport org.spring.RestController;\n\n@RestController\nclass Api {\n    @PostMapping(\"/orders\")\n    void create() {}\n}\n",
    )]);
    assert_eq!(
        text(&edge(&a, "ANNOTATED_BY", "org.spring.PostMapping").props["args"]["$value"]),
        "\"/orders\""
    );
    assert!(
        !edge(&a, "ANNOTATED_BY", "org.spring.RestController")
            .props
            .contains_key("args")
    );
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

// ---- baseline Tier B: receiver typing, chains, fields (mined) ------------

/// A declared local types the receiver — and two same-named methods never
/// cross-attribute (the Logger/Other decoy discipline).
#[test]
fn declared_locals_type_receivers_without_crossing() {
    let a = run(vec![(
        "m/Main.java",
        "package m;\nclass Logger { int log() { return 1; } }\nclass Other { int log() { return 2; } }\nclass Main {\n  int use() { Logger lg = new Logger(); return lg.log(); }\n  int useOther() { Other o = new Other(); return o.log(); }\n}\n",
    )]);
    assert!(
        has_edge(&a, "m.Main.use", "CALLS", "m.Logger.log"),
        "{:?}",
        a.edges
    );
    assert!(has_edge(&a, "m.Main.useOther", "CALLS", "m.Other.log"));
    assert!(!has_edge(&a, "m.Main.use", "CALLS", "m.Other.log"));
    assert!(!has_edge(&a, "m.Main.useOther", "CALLS", "m.Logger.log"));
}

/// A parameter's declared type and a `var` local's `new` initializer both
/// dispatch; inheritance walks to the base where the method lives.
#[test]
fn params_vars_and_inheritance_type_receivers() {
    let a = run(vec![(
        "m/Main.java",
        "package m;\nclass Animal { int breathe() { return 1; } }\nclass Dog extends Animal { }\nclass Main {\n  int walk(Dog d) { return d.breathe(); }\n  int walkVar() { var d = new Dog(); return d.breathe(); }\n}\n",
    )]);
    assert!(
        has_edge(&a, "m.Main.walk", "CALLS", "m.Animal.breathe"),
        "{:?}",
        a.edges
    );
    assert!(has_edge(&a, "m.Main.walkVar", "CALLS", "m.Animal.breathe"));
}

/// A field's declared type resolves `this.field.m()` and bare `field.m()`;
/// a one-hop chain through another object's field resolves too.
#[test]
fn field_types_resolve_member_chains() {
    let a = run(vec![(
        "m/Main.java",
        "package m;\nclass Repo { int all() { return 1; } }\nclass App {\n  Repo repo;\n  int render() { return this.repo.all(); }\n  int bare() { return repo.all(); }\n}\nclass Main {\n  int use(App app) { return app.repo.all(); }\n}\n",
    )]);
    assert!(
        has_edge(&a, "m.App.render", "CALLS", "m.Repo.all"),
        "{:?}",
        a.edges
    );
    assert!(has_edge(&a, "m.App.bare", "CALLS", "m.Repo.all"));
    assert!(has_edge(&a, "m.Main.use", "CALLS", "m.Repo.all"));
}

/// codegraph #645: a static factory chain resolves through the factory's
/// declared return — and a same-named decoy method never wins.
#[test]
fn static_factory_chains_resolve_through_declared_returns() {
    let a = run(vec![(
        "m/Main.java",
        "package m;\nclass Aaa { int bar() { return 1; } }\nclass Foo {\n  static Foo getInstance() { return new Foo(); }\n  int bar() { return 2; }\n}\nclass Main {\n  int use() { return Foo.getInstance().bar(); }\n}\n",
    )]);
    assert!(
        has_edge(&a, "m.Main.use", "CALLS", "m.Foo.bar"),
        "{:?}",
        a.edges
    );
    assert!(!has_edge(&a, "m.Main.use", "CALLS", "m.Aaa.bar"));
}

/// cbm (load-bearing): enum methods are walked and receiver-qualified.
#[test]
fn enum_methods_resolve_like_any_other() {
    let a = run(vec![(
        "m/Main.java",
        "package m;\nenum Day {\n  SAT, SUN;\n  int label() { return 1; }\n}\nclass Main {\n  int use(Day day) { return day.label(); }\n}\n",
    )]);
    assert!(
        has_edge(&a, "m.Main.use", "CALLS", "m.Day.label"),
        "{:?}",
        a.edges
    );
}

/// The typed paths carry the family stamps.
#[test]
fn receiver_resolutions_are_stamped() {
    let a = run(vec![(
        "m/Main.java",
        "package m;\nclass Logger { int log() { return 1; } }\nclass Main {\n  int use(Logger lg) { return lg.log(); }\n}\n",
    )]);
    let e = a
        .edges
        .iter()
        .find(|e| e.src == "m.Main.use" && e.dst == "m.Logger.log")
        .expect("edge");
    assert_eq!(
        e.props.get("_resolved_by").and_then(|v| v.as_str()),
        Some("receiver")
    );
}

/// Two rules, ranked: the source set Maven and Gradle compile apart is
/// `strong`, and a JUnit annotation — which is JUnit's own definition of a
/// test — is `definitive`, reaching the type that holds the method too.
#[test]
fn test_code_is_flagged_by_source_set_and_by_annotation() {
    let a = run(vec![
        (
            "src/main/java/com/acme/Engine.java",
            "package com.acme;\npublic class Engine {\n  public void start() {}\n}\n",
        ),
        (
            "src/test/java/com/acme/EngineTest.java",
            "package com.acme;\nimport org.junit.jupiter.api.Test;\npublic class EngineTest {\n  @Test\n  public void starts() {}\n  public void helper() {}\n}\n",
        ),
        (
            "src/main/java/com/acme/OddlyPlaced.java",
            "package com.acme;\nimport org.junit.jupiter.api.Test;\npublic class OddlyPlaced {\n  @Test\n  public void alsoATest() {}\n}\n",
        ),
    ]);

    // Annotation wins over layout on the method and on its class.
    for key in ["com.acme.EngineTest", "com.acme.EngineTest.starts"] {
        assert_eq!(
            node(&a, key).props["test_flag"]["$value"],
            Value::from("annotation"),
            "{key}"
        );
        assert_eq!(
            node(&a, key).props["_test_flag_confidence"],
            Value::from("definitive"),
            "{key}"
        );
    }

    // An unannotated helper in the test source set keeps the weaker flag.
    assert_eq!(
        node(&a, "com.acme.EngineTest.helper").props["test_flag"]["$value"],
        Value::from("build-layout")
    );
    assert_eq!(
        node(&a, "com.acme.EngineTest.helper").props["_test_flag_confidence"],
        Value::from("strong")
    );

    // An annotated test in the main source set is still a test.
    assert_eq!(
        node(&a, "com.acme.OddlyPlaced.alsoATest").props["test_flag"]["$value"],
        Value::from("annotation")
    );

    for key in ["com.acme.Engine", "com.acme.Engine.start"] {
        assert!(
            !node(&a, key).props.contains_key("test_flag"),
            "{key} must not be flagged"
        );
    }
}

/// A declared type is a dependency as surely as a call is: fields (a record's
/// components included), parameters and returns say so, through generic
/// arguments, and folded to one edge per pair.
#[test]
fn declared_types_become_uses_type_edges() {
    let a = run(vec![
        (
            "src/main/java/com/acme/Job.java",
            "package com.acme;\npublic class Job {}\n",
        ),
        (
            "src/main/java/com/acme/Cfg.java",
            "package com.acme;\npublic class Cfg {}\n",
        ),
        (
            "src/main/java/com/acme/Pool.java",
            "package com.acme;\nimport java.util.List;\npublic class Pool {\n  private List<Job> queue;\n  private Cfg cfg;\n  private String label;\n  public Job work(Cfg c) { return null; }\n  public Cfg roundTrip(Cfg c) { return c; }\n}\n",
        ),
        (
            "src/main/java/com/acme/Pair.java",
            "package com.acme;\npublic record Pair(Job left, Cfg right) {}\n",
        ),
        (
            "src/main/java/com/acme/Node.java",
            "package com.acme;\npublic class Node { private Node next; }\n",
        ),
    ]);

    let role = |src: &str, dst: &str| -> Option<String> {
        a.edges
            .iter()
            .find(|e| e.ty == "USES_TYPE" && e.src == src && e.dst == dst)
            .map(|e| e.props["role"]["$value"].as_str().unwrap().to_string())
    };

    // A field through a generic argument, which is how most Java code depends
    // on a type.
    assert_eq!(
        role("com.acme.Pool", "com.acme.Job").as_deref(),
        Some("field")
    );
    assert_eq!(
        role("com.acme.Pool", "com.acme.Cfg").as_deref(),
        Some("field")
    );
    // Parameter and return, then both folded onto one edge.
    assert_eq!(
        role("com.acme.Pool.work", "com.acme.Cfg").as_deref(),
        Some("param")
    );
    assert_eq!(
        role("com.acme.Pool.work", "com.acme.Job").as_deref(),
        Some("return")
    );
    assert_eq!(
        role("com.acme.Pool.roundTrip", "com.acme.Cfg").as_deref(),
        Some("param, return"),
        "one dependency, two positions, one edge"
    );
    // A record's components are its fields.
    assert_eq!(
        role("com.acme.Pair", "com.acme.Job").as_deref(),
        Some("field")
    );
    assert_eq!(
        role("com.acme.Pair", "com.acme.Cfg").as_deref(),
        Some("field")
    );

    // Foreign types stay text, and a type never points at itself.
    for e in a.edges.iter().filter(|e| e.ty == "USES_TYPE") {
        assert_ne!(e.src, e.dst, "never a self-loop");
        for foreign in ["String", "List"] {
            assert!(!e.dst.ends_with(foreign), "{} must not get an edge", e.dst);
        }
    }
}

/// How a dependency arrives rides its `USES_TYPE` edge: an `@Autowired` is
/// written down, a stereotyped bean's only constructor is Spring's convention,
/// and a dependency the owner builds itself is neither.
#[test]
fn an_injected_dependency_says_what_wired_it() {
    let a = run(vec![
        (
            "Port.java",
            "package m;\n\npublic interface Port {\n    void go();\n}\n",
        ),
        (
            "Other.java",
            "package m;\n\npublic interface Other {\n    void go();\n}\n",
        ),
        (
            "Wired.java",
            "package m;\n\nimport org.spring.Autowired;\nimport org.spring.Service;\n\n@Service\nclass Wired {\n    @Autowired\n    private Port port;\n\n    void use() {\n        port.go();\n    }\n}\n",
        ),
        (
            "Built.java",
            "package m;\n\nclass Built {\n    private Port port = null;\n}\n",
        ),
        (
            "Ctor.java",
            "package m;\n\nimport org.spring.Service;\n\n@Service\nclass Ctor {\n    private final Port port;\n    private final Other other;\n\n    Ctor(Port port, Other other) {\n        this.port = port;\n        this.other = other;\n    }\n}\n",
        ),
        (
            "Plain.java",
            "package m;\n\nclass Plain {\n    private final Port port;\n\n    Plain(Port port) {\n        this.port = port;\n    }\n}\n",
        ),
    ]);

    let injected = |src: &str, dst: &str| -> Option<String> {
        a.edges
            .iter()
            .find(|e| e.ty == "USES_TYPE" && e.src == src && e.dst == dst)
            .and_then(|e| e.props.get("injected"))
            .map(|v| text(&v["$value"]).to_string())
    };
    let confidence = |src: &str, dst: &str| -> Option<String> {
        a.edges
            .iter()
            .find(|e| e.ty == "USES_TYPE" && e.src == src && e.dst == dst)
            .and_then(|e| e.props.get("_injected_confidence"))
            .map(|v| text(v).to_string())
    };

    assert_eq!(injected("m.Wired", "m.Port").as_deref(), Some("annotation"));
    assert_eq!(
        confidence("m.Wired", "m.Port").as_deref(),
        Some("definitive")
    );
    assert_eq!(injected("m.Ctor", "m.Port").as_deref(), Some("constructor"));
    assert_eq!(
        injected("m.Ctor", "m.Other").as_deref(),
        Some("constructor")
    );
    assert_eq!(confidence("m.Ctor", "m.Port").as_deref(), Some("strong"));
    // A field the owner assigns itself, and a constructor on a type no
    // container knows about, are not injection points.
    assert_eq!(injected("m.Built", "m.Port"), None);
    assert_eq!(injected("m.Plain", "m.Port"), None);
    note_containing(&a, "injection points");
}

/// A setter Spring calls is an injection point too. A parameter carries no
/// node of its own, so its `@Qualifier` rides the injection fact — where the
/// dispatch rule reads it — rather than an edge.
#[test]
fn a_setter_injection_keeps_its_qualifier() {
    let m = MapFiles {
        files: vec![(
            "Host.java",
            "package m;\n\nimport org.spring.Autowired;\nimport org.spring.Qualifier;\nimport org.spring.Service;\n\n@Service\nclass Host {\n    private Port port;\n\n    @Autowired\n    void setPort(@Qualifier(\"fast\") Port port) {\n        this.port = port;\n    }\n}\n",
        )],
    };
    let facts = parse_chunk(&m, &m.list(".java").unwrap(), false);
    let i = facts[0]
        .injections
        .iter()
        .find(|i| i.name == "port")
        .expect("the setter's parameter is an injection point");
    assert_eq!(i.owner, "m.Host");
    assert_eq!(i.kind, "annotation");
    assert_eq!(i.written, "Port");
    assert_eq!(i.qualifier.as_deref(), Some("\"fast\""));
}

/// A collection injection point keeps what it subscripts: Spring hands it
/// every bean of the element type, and the head (`List`) says nothing.
#[test]
fn a_collection_injection_point_keeps_its_element() {
    let m = MapFiles {
        files: vec![(
            "Dispatcher.java",
            "package m;\n\nimport java.util.List;\nimport org.spring.Autowired;\nimport org.spring.Service;\n\n@Service\nclass Dispatcher {\n    @Autowired\n    private List<Handler> handlers;\n}\n",
        )],
    };
    let facts = parse_chunk(&m, &m.list(".java").unwrap(), false);
    let i = &facts[0].injections[0];
    assert_eq!(i.written, "List");
    assert_eq!(i.args, vec!["Handler".to_string()]);
}

// ---- Spring injection -----------------------------------------------------

/// The Spring shape in full: a controller injects a port, the bean that
/// implements it is the only one, and the call reaches it — while the call to
/// the port stays recorded as the source wrote it.
#[test]
fn a_call_through_a_port_reaches_the_only_bean() {
    let a = run(vec![
        (
            "OrderService.java",
            "package m;\n\npublic interface OrderService {\n    void place(String id);\n}\n",
        ),
        (
            "OrderServiceImpl.java",
            "package m;\n\nimport org.spring.Service;\n\n@Service\npublic class OrderServiceImpl implements OrderService {\n    public void place(String id) {}\n}\n",
        ),
        (
            "OrderController.java",
            "package m;\n\nimport org.spring.Autowired;\nimport org.spring.RestController;\n\n@RestController\npublic class OrderController {\n    @Autowired\n    private OrderService orderService;\n\n    public void create(String id) {\n        orderService.place(id);\n    }\n}\n",
        ),
    ]);
    assert!(has_edge(
        &a,
        "m.OrderController.create",
        "CALLS",
        "m.OrderService.place"
    ));
    let wired = a
        .edges
        .iter()
        .find(|e| {
            e.ty == "CALLS"
                && e.src == "m.OrderController.create"
                && e.dst == "m.OrderServiceImpl.place"
        })
        .expect("the call reaches the bean Spring wires");
    assert_eq!(text(&wired.props["_resolved_by"]), "spring-bean");
    assert_eq!(text(&wired.props["_confidence"]), "high");
    assert_eq!(text(&wired.props["_via"]["$value"]), "m.OrderService.place");
    note_containing(&a, "the bean Spring wires");
}

/// A constructor-injected port dispatches the same way, and the bean's own
/// call reaches the next bean down — the chain the issue asks for.
#[test]
fn a_constructor_injected_chain_reaches_the_bottom() {
    let a = run(vec![
        (
            "Repo.java",
            "package m;\n\npublic interface Repo {\n    void save(String id);\n}\n",
        ),
        (
            "JpaRepo.java",
            "package m;\n\nimport org.spring.Repository;\n\n@Repository\nclass JpaRepo implements Repo {\n    public void save(String id) {}\n}\n",
        ),
        (
            "Svc.java",
            "package m;\n\nimport org.spring.Service;\n\n@Service\nclass Svc {\n    private final Repo repo;\n\n    Svc(Repo repo) {\n        this.repo = repo;\n    }\n\n    void place(String id) {\n        this.repo.save(id);\n    }\n}\n",
        ),
    ]);
    assert!(has_edge(&a, "m.Svc.place", "CALLS", "m.Repo.save"));
    assert!(has_edge(&a, "m.Svc.place", "CALLS", "m.JpaRepo.save"));
}

/// Two beans and a `@Qualifier`: the name the injection point states picks the
/// bean, and the bean's own name is Spring's — the stereotype's argument, else
/// the simple name decapitalized.
#[test]
fn a_qualifier_picks_the_bean_by_name() {
    let a = run(vec![
        (
            "Port.java",
            "package m;\n\npublic interface Port {\n    void go();\n}\n",
        ),
        (
            "Fast.java",
            "package m;\n\nimport org.spring.Service;\n\n@Service(\"fast\")\nclass Fast implements Port {\n    public void go() {}\n}\n",
        ),
        (
            "Slow.java",
            "package m;\n\nimport org.spring.Service;\n\n@Service\nclass Slow implements Port {\n    public void go() {}\n}\n",
        ),
        (
            "ByName.java",
            "package m;\n\nimport org.spring.Autowired;\nimport org.spring.Qualifier;\nimport org.spring.Service;\n\n@Service\nclass ByName {\n    @Autowired\n    @Qualifier(\"fast\")\n    private Port port;\n\n    void use() {\n        port.go();\n    }\n}\n",
        ),
        (
            "BySimpleName.java",
            "package m;\n\nimport org.spring.Autowired;\nimport org.spring.Qualifier;\nimport org.spring.Service;\n\n@Service\nclass BySimpleName {\n    @Autowired\n    @Qualifier(\"slow\")\n    private Port port;\n\n    void use() {\n        port.go();\n    }\n}\n",
        ),
    ]);
    assert!(has_edge(&a, "m.ByName.use", "CALLS", "m.Fast.go"));
    assert!(!has_edge(&a, "m.ByName.use", "CALLS", "m.Slow.go"));
    assert!(has_edge(&a, "m.BySimpleName.use", "CALLS", "m.Slow.go"));
    assert!(!has_edge(&a, "m.BySimpleName.use", "CALLS", "m.Fast.go"));
    let e = a
        .edges
        .iter()
        .find(|e| e.ty == "CALLS" && e.src == "m.ByName.use" && e.dst == "m.Fast.go")
        .unwrap();
    assert_eq!(text(&e.props["_resolved_by"]), "spring-qualifier");
}

/// `@Primary` breaks a tie the injection point does not.
#[test]
fn primary_breaks_a_tie_between_two_beans() {
    let a = run(vec![
        (
            "Port.java",
            "package m;\n\npublic interface Port {\n    void go();\n}\n",
        ),
        (
            "Chosen.java",
            "package m;\n\nimport org.spring.Primary;\nimport org.spring.Service;\n\n@Service\n@Primary\nclass Chosen implements Port {\n    public void go() {}\n}\n",
        ),
        (
            "Other.java",
            "package m;\n\nimport org.spring.Service;\n\n@Service\nclass Other implements Port {\n    public void go() {}\n}\n",
        ),
        (
            "Host.java",
            "package m;\n\nimport org.spring.Autowired;\nimport org.spring.Service;\n\n@Service\nclass Host {\n    @Autowired\n    private Port port;\n\n    void use() {\n        port.go();\n    }\n}\n",
        ),
    ]);
    assert!(has_edge(&a, "m.Host.use", "CALLS", "m.Chosen.go"));
    assert!(!has_edge(&a, "m.Host.use", "CALLS", "m.Other.go"));
    let e = a
        .edges
        .iter()
        .find(|e| e.ty == "CALLS" && e.src == "m.Host.use" && e.dst == "m.Chosen.go")
        .unwrap();
    assert_eq!(text(&e.props["_resolved_by"]), "spring-primary");
}

/// Two beans, nothing to choose between them: the call stays at the port and
/// the account says why. Spring would not start on this either.
#[test]
fn two_beans_and_no_tie_break_resolve_to_nothing() {
    let a = run(vec![
        (
            "Port.java",
            "package m;\n\npublic interface Port {\n    void go();\n}\n",
        ),
        (
            "One.java",
            "package m;\n\nimport org.spring.Service;\n\n@Service\nclass One implements Port {\n    public void go() {}\n}\n",
        ),
        (
            "Two.java",
            "package m;\n\nimport org.spring.Service;\n\n@Service\nclass Two implements Port {\n    public void go() {}\n}\n",
        ),
        (
            "Host.java",
            "package m;\n\nimport org.spring.Autowired;\nimport org.spring.Service;\n\n@Service\nclass Host {\n    @Autowired\n    private Port port;\n\n    void use() {\n        port.go();\n    }\n}\n",
        ),
    ]);
    assert!(has_edge(&a, "m.Host.use", "CALLS", "m.Port.go"));
    assert!(!has_edge(&a, "m.Host.use", "CALLS", "m.One.go"));
    assert!(!has_edge(&a, "m.Host.use", "CALLS", "m.Two.go"));
    note_containing(&a, "stop at their port");
}

/// A container-full of a port is every bean of it: Spring injects them all, so
/// a call through a value of that port inside the asking type reaches each, at
/// a confidence that says which one runs is not written down.
#[test]
fn a_collection_injection_point_reaches_every_bean() {
    let a = run(vec![
        (
            "Handler.java",
            "package m;\n\npublic interface Handler {\n    void handle();\n}\n",
        ),
        (
            "A.java",
            "package m;\n\nimport org.spring.Service;\n\n@Service\nclass A implements Handler {\n    public void handle() {}\n}\n",
        ),
        (
            "B.java",
            "package m;\n\nimport org.spring.Service;\n\n@Service\nclass B implements Handler {\n    public void handle() {}\n}\n",
        ),
        (
            "Fanout.java",
            "package m;\n\nimport java.util.List;\nimport org.spring.Autowired;\nimport org.spring.Service;\n\n@Service\nclass Fanout {\n    @Autowired\n    private List<Handler> handlers;\n\n    void all() {\n        Handler h = handlers.get(0);\n        h.handle();\n    }\n}\n",
        ),
    ]);
    assert!(has_edge(&a, "m.Fanout.all", "CALLS", "m.A.handle"));
    assert!(has_edge(&a, "m.Fanout.all", "CALLS", "m.B.handle"));
    let e = a
        .edges
        .iter()
        .find(|e| e.ty == "CALLS" && e.src == "m.Fanout.all" && e.dst == "m.A.handle")
        .unwrap();
    assert_eq!(text(&e.props["_resolved_by"]), "spring-bean-collection");
    assert_eq!(text(&e.props["_confidence"]), "medium");
}

/// A port Spring generates the implementation for — a Spring Data repository —
/// has no bean in the tree, so the chain honestly ends at the port.
#[test]
fn a_port_with_no_bean_stops_at_the_port() {
    let a = run(vec![
        (
            "OrderRepo.java",
            "package m;\n\nimport org.data.JpaRepository;\n\npublic interface OrderRepo extends JpaRepository<String, Long> {\n    void save(String id);\n}\n",
        ),
        (
            "Svc.java",
            "package m;\n\nimport org.spring.Autowired;\nimport org.spring.Service;\n\n@Service\nclass Svc {\n    @Autowired\n    private OrderRepo repo;\n\n    void go() {\n        repo.save(\"x\");\n    }\n}\n",
        ),
    ]);
    assert!(has_edge(&a, "m.Svc.go", "CALLS", "m.OrderRepo.save"));
    assert_eq!(
        a.edges
            .iter()
            .filter(|e| e.ty == "CALLS" && e.src == "m.Svc.go")
            .count(),
        1,
        "nothing is invented for a port the tree holds no bean for"
    );
}

/// A `@Bean` method in a `@Configuration` registers what it returns, and an
/// abstract class between port and bean is walked to where the code is.
#[test]
fn a_bean_method_registers_what_it_returns() {
    let a = run(vec![
        (
            "Port.java",
            "package m;\n\npublic interface Port {\n    void go();\n}\n",
        ),
        (
            "Base.java",
            "package m;\n\nabstract class Base implements Port {\n    public void go() {}\n}\n",
        ),
        ("Impl.java", "package m;\n\nclass Impl extends Base {\n}\n"),
        (
            "Config.java",
            "package m;\n\nimport org.spring.Bean;\nimport org.spring.Configuration;\n\n@Configuration\nclass Config {\n    @Bean\n    Impl impl() {\n        return new Impl();\n    }\n}\n",
        ),
        (
            "Host.java",
            "package m;\n\nimport org.spring.Autowired;\nimport org.spring.Service;\n\n@Service\nclass Host {\n    @Autowired\n    private Port port;\n\n    void use() {\n        port.go();\n    }\n}\n",
        ),
    ]);
    // The method is declared on the abstract base, which is where a reader
    // goes — not on the bean that inherits it.
    assert!(has_edge(&a, "m.Host.use", "CALLS", "m.Base.go"));
    assert!(has_edge(&a, "m.Host.use", "CALLS", "m.Port.go"));
}

/// A port value that nothing injects is not dispatched: whoever built it chose
/// the implementation, and that is data flow a parser must not guess at.
#[test]
fn an_uninjected_port_value_is_not_dispatched() {
    let a = run(vec![
        (
            "Port.java",
            "package m;\n\npublic interface Port {\n    void go();\n}\n",
        ),
        (
            "Only.java",
            "package m;\n\nimport org.spring.Service;\n\n@Service\nclass Only implements Port {\n    public void go() {}\n}\n",
        ),
        (
            "Caller.java",
            "package m;\n\nclass Caller {\n    void use(Port port) {\n        port.go();\n    }\n}\n",
        ),
    ]);
    assert!(has_edge(&a, "m.Caller.use", "CALLS", "m.Port.go"));
    assert!(!has_edge(&a, "m.Caller.use", "CALLS", "m.Only.go"));
}

/// An enhanced-for states its variable's type as plainly as a declaration
/// does, so a call on the loop variable resolves — and under Spring, reaches
/// every bean the container filled the collection with.
#[test]
fn an_enhanced_for_variable_is_typed_by_its_declaration() {
    let a = run(vec![
        (
            "Handler.java",
            "package m;\n\npublic interface Handler {\n    void handle();\n}\n",
        ),
        (
            "A.java",
            "package m;\n\nimport org.spring.Service;\n\n@Service\nclass A implements Handler {\n    public void handle() {}\n}\n",
        ),
        (
            "B.java",
            "package m;\n\nimport org.spring.Service;\n\n@Service\nclass B implements Handler {\n    public void handle() {}\n}\n",
        ),
        (
            "Fanout.java",
            "package m;\n\nimport java.util.List;\nimport org.spring.Autowired;\nimport org.spring.Service;\n\n@Service\nclass Fanout {\n    @Autowired\n    private List<Handler> handlers;\n\n    void all() {\n        for (Handler h : handlers) {\n            h.handle();\n        }\n    }\n}\n",
        ),
    ]);
    assert!(has_edge(&a, "m.Fanout.all", "CALLS", "m.Handler.handle"));
    assert!(has_edge(&a, "m.Fanout.all", "CALLS", "m.A.handle"));
    assert!(has_edge(&a, "m.Fanout.all", "CALLS", "m.B.handle"));
}
