//! The parser's own tests, running natively.

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
    let mut paths = m.list(".c").unwrap();
    paths.extend(m.list(".h").unwrap());
    paths.sort();
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

/// External linkage is one flat namespace — that is C's own rule, so a
/// non-static function's key is its bare name; a static is file-local.
#[test]
fn linkage_decides_the_key() {
    let a = run(vec![(
        "src/util.c",
        "int shared(void) { return 1; }\n\nstatic int helper(void) { return 2; }\n",
    )]);
    node(&a, "shared");
    node(&a, "util.helper");
    assert_eq!(text(&node(&a, "util.helper").props["visibility"]), "static");
    // The file contains both.
    assert!(has_edge(&a, "src/util.c", "CONTAINS", "shared"));
    assert!(has_edge(&a, "src/util.c", "CONTAINS", "util.helper"));
}

/// A header declares what a source defines: one key, and the definition
/// wins the node — its line, its body, its file.
#[test]
fn the_definition_beats_the_declaration() {
    let a = run(vec![
        (
            "util.h",
            "/** Parses a row. */\nint parse_row(const char *s);\n",
        ),
        (
            "util.c",
            "#include \"util.h\"\n\nint parse_row(const char *s) { return 0; }\n",
        ),
    ]);
    let f = node(&a, "parse_row");
    assert_eq!(text(&f.props["file"]), "util.c");
    assert_eq!(f.props["line"], Value::from(3u64));
    note_containing(&a, "definition wins");
    // Two files, one interface: the source imports its own header, and
    // both are File nodes — a header is a file, not an alias of its .c.
    assert!(has_edge(&a, "util.c", "IMPORTS", "util.h"));
    assert_eq!(node(&a, "util.h").label, "File");
}

/// Structs carry fields, enums carry variants, typedefs carry what they
/// alias — values and types as written.
#[test]
fn types_keep_their_shape() {
    let a = run(vec![(
        "types.c",
        r#"struct point { int x; int y; char *label; };

enum level { DEBUG, WARN = 10, ERROR };

typedef struct point point_t;

typedef unsigned long size_type;
"#,
    )]);
    let s = node(&a, "point");
    assert_eq!(s.label, "Struct");
    let fields: Vec<&str> = s.props["fields"]["$value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(fields, vec!["x: int", "y: int", "label: char"]);

    let e = node(&a, "level");
    let variants: Vec<&str> = e.props["variants"]["$value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(variants, vec!["DEBUG", "WARN = 10", "ERROR"]);

    assert_eq!(node(&a, "point_t").label, "TypeAlias");
    assert_eq!(
        text(&node(&a, "size_type").props["signature"]),
        "unsigned long"
    );
}

/// An object-like #define is a Const with its body as written; a
/// function-like one is a Macro. Include guards are bookkeeping.
#[test]
fn defines_are_consts_and_macros() {
    let a = run(vec![(
        "cfg.h",
        "#ifndef CFG_H\n#define CFG_H\n\n#define MAX_RETRIES (4 * 8)\n#define MIN(a, b) ((a) < (b) ? (a) : (b))\n\n#endif\n",
    )]);
    let c = node(&a, "MAX_RETRIES");
    assert_eq!(c.label, "Const");
    assert_eq!(text(&c.props["value"]), "(4 * 8)");
    let m = node(&a, "MIN");
    assert_eq!(m.label, "Macro");
    assert_eq!(text(&m.props["signature"]), "MIN(a, b)");
    assert!(!a.nodes.iter().any(|n| n.key == "CFG_H"));
}

/// Globals are Vars with type and initializer as written; `extern` is a
/// declaration and yields to the definition.
#[test]
fn globals_keep_type_and_value() {
    let a = run(vec![
        ("state.h", "extern int counter;\n"),
        ("state.c", "int counter = 40 + 2;\n"),
    ]);
    let v = node(&a, "counter");
    assert_eq!(v.label, "Var");
    assert_eq!(text(&v.props["signature"]), "int");
    assert_eq!(text(&v.props["value"]), "40 + 2");
    assert_eq!(text(&v.props["file"]), "state.c");
}

// ---- resolution -----------------------------------------------------------

/// Calls bind to the flat global namespace; a static in the calling file
/// shadows a global of the same name — the compiler's own rule.
#[test]
fn statics_shadow_globals_in_their_file() {
    let a = run(vec![
        (
            "a.c",
            "static void helper(void) {}\n\nvoid run_a(void) { helper(); }\n",
        ),
        (
            "b.c",
            "void helper(void) {}\n\nvoid run_b(void) { helper(); }\n",
        ),
    ]);
    assert!(has_edge(&a, "run_a", "CALLS", "a.helper"));
    assert!(has_edge(&a, "run_b", "CALLS", "helper"));
}

/// A call to a prototype whose definition lives elsewhere still binds —
/// the header pattern, end to end.
#[test]
fn calls_resolve_through_headers() {
    let a = run(vec![
        ("api.h", "void do_work(void);\n"),
        ("api.c", "#include \"api.h\"\n\nvoid do_work(void) {}\n"),
        (
            "main.c",
            "#include \"api.h\"\n\nint main(void) {\n    do_work();\n    return 0;\n}\n",
        ),
    ]);
    assert!(has_edge(&a, "main", "CALLS", "do_work"));
    let f = node(&a, "do_work");
    assert_eq!(text(&f.props["file"]), "api.c");
}

/// libc calls are external by name; a function pointer is a value's
/// business, counted.
#[test]
fn libc_is_external_and_pointers_are_counted() {
    let a = run(vec![(
        "m.c",
        "#include <stdio.h>\n\nvoid log_it(void (*cb)(void)) {\n    printf(\"x\");\n    cb();\n}\n",
    )]);
    let p = node(&a, "printf");
    assert!(p.extra_labels.contains(&"External".to_string()));
    assert!(has_edge(&a, "log_it", "CALLS", "printf"));
    note_containing(&a, "function pointer");
    // The system include is an external File.
    assert!(has_edge(&a, "m.c", "IMPORTS", "stdio.h"));
}

/// Quoted includes resolve against the parsed tree — same directory first,
/// then an unambiguous tail anywhere; ambiguity is counted, not guessed.
#[test]
fn includes_resolve_in_tree() {
    let a = run(vec![
        (
            "src/core/engine.c",
            "#include \"engine.h\"\n#include \"util/str.h\"\n\nvoid start(void) {}\n",
        ),
        ("src/core/engine.h", "void start(void);\n"),
        ("src/util/str.h", "int str_len(const char *s);\n"),
    ]);
    assert!(has_edge(
        &a,
        "src/core/engine.c",
        "IMPORTS",
        "src/core/engine.h"
    ));
    assert!(has_edge(
        &a,
        "src/core/engine.c",
        "IMPORTS",
        "src/util/str.h"
    ));
}

/// `#ifdef` arms hold ordinary declarations — walked, not skipped.
#[test]
fn preprocessor_arms_are_walked() {
    let a = run(vec![(
        "port.c",
        "#ifdef _WIN32\nvoid platform_init(void) {}\n#else\nvoid platform_init_unix(void) {}\n#endif\n",
    )]);
    node(&a, "platform_init");
    node(&a, "platform_init_unix");
}

/// Documentation above a declaration: /** */, /* */, and // runs all count
/// — C documents in all three.
#[test]
fn comments_above_are_docs() {
    let a = run(vec![(
        "m.c",
        "/** Block doc. */\nvoid a(void) {}\n\n// Line one.\n// Line two.\nvoid b(void) {}\n",
    )]);
    assert_eq!(text(&node(&a, "a").props["doc_comment"]), "Block doc.");
    assert_eq!(
        text(&node(&a, "b").props["doc_comment"]),
        "Line one.\nLine two."
    );
}

// ---- provenance and robustness --------------------------------------------

/// Every definition knows its file and line, and every written relation
/// knows the line it is written on.
#[test]
fn lines_and_files_are_recorded() {
    let a = run(vec![
        (
            "a.c",
            "#include \"b.h\"\n\nvoid caller(void) {\n    helper();\n}\n",
        ),
        ("b.h", "void helper(void);\n"),
        ("b.c", "void helper(void) {}\n"),
    ]);
    let f = node(&a, "caller");
    assert_eq!(f.props["line"], Value::from(3u64));
    assert_eq!(text(&f.props["file"]), "a.c");
    assert_eq!(edge(&a, "CALLS", "helper").props["line"], Value::from(4u64));
    assert_eq!(edge(&a, "IMPORTS", "b.h").props["line"], Value::from(1u64));
    // The file node: no line, no `path` (the key is the path), and its
    // includes resolved to the keys of the files they name — link-shaped.
    let file = node(&a, "a.c");
    assert!(!file.props.contains_key("line"));
    assert!(!file.props.contains_key("path"));
    assert_eq!(text(&file.props["includes"]), "b.h");
}

/// A file that will not parse is counted, and takes nothing else down.
#[test]
fn a_parse_error_is_counted_not_fatal() {
    let a = run(vec![
        ("ok.c", "void fine(void) {}\n"),
        ("bad.c", "template <class T> struct nope {};"),
    ]);
    assert_eq!(a.skipped, 1);
    node(&a, "fine");
}

/// The result must not depend on where the chunk boundaries fell, and the
/// same tree twice gives the same facts.
#[test]
fn deterministic_and_chunk_independent() {
    let files = vec![
        ("a.c", "#include \"b.h\"\n\nvoid go(void) { helper(); }\n"),
        ("b.h", "void helper(void);\n"),
        ("b.c", "void helper(void) {}\n"),
    ];
    let m = MapFiles { files };
    let mut paths = m.list(".c").unwrap();
    paths.extend(m.list(".h").unwrap());
    paths.sort();

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
    let facts = parse_document("snippet.c", b"int solo(void) { return 1; }\n", false);
    let a = assemble(facts);
    node(&a, "solo");
}

/// include_source attaches the definition as written under `_code`.
#[test]
fn include_source_attaches_the_definition() {
    let m = MapFiles {
        files: vec![("m.c", "int shown(void) { return 42; }\n")],
    };
    let paths = m.list(".c").unwrap();
    let a = assemble(parse_chunk(&m, &paths, true));
    let n = node(&a, "shown");
    assert!(
        text(&n.props["_code"]["$value"]).contains("return 42"),
        "{:?}",
        n.props
    );
}
