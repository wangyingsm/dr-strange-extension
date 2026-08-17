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

/// Parse every claimed file in one chunk and assemble — the two phases, end
/// to end, the way the component drives them.
fn run(m: &MapFiles) -> Assembled {
    let mut paths = Vec::new();
    for ext in EXTENSIONS {
        paths.extend(m.list(&format!(".{ext}")).unwrap());
    }
    paths.sort();
    paths.dedup();
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

// ---- keys ----------------------------------------------------------------

/// Keys are logical module identity: the nearest package.json names the
/// package, the manifest-relative path names the module, `/index` collapses.
#[test]
fn keys_use_the_manifest_and_collapse_index() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "acme" }"#),
        ("index.ts", "export function boot() {}\n"),
        ("src/util.ts", "export function fmt() {}\n"),
        ("src/api/index.ts", "export function call() {}\n"),
    ]));
    node(&a, "acme.boot"); // root index.ts names the package itself
    node(&a, "acme/src/util.fmt");
    node(&a, "acme/src/api.call"); // index collapses to its directory
}

/// A nested manifest starts its own package — monorepos resolve to the
/// nearest one, the way workspace crates do.
#[test]
fn the_nearest_manifest_wins() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "root" }"#),
        ("main.ts", "export function top() {}\n"),
        ("packages/web/package.json", r#"{ "name": "@acme/web" }"#),
        ("packages/web/src/app.ts", "export function render() {}\n"),
    ]));
    node(&a, "root/main.top");
    node(&a, "@acme/web/src/app.render");
}

/// No manifest anywhere: the host's label is what the tree is called.
#[test]
fn the_label_is_the_fallback_package() {
    let m = MapFiles {
        files: vec![("x.ts", "export function f() {}\n")],
        label: "myrepo",
    };
    let a = run(&m);
    node(&a, "myrepo/x.f");
}

// ---- declarations ---------------------------------------------------------

/// `const f = (…) => …` *is* the function — the TS idiom for most of them.
#[test]
fn an_arrow_const_is_a_function() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        (
            "a.ts",
            "/** Greets. */\nexport const greet = async (name: string): Promise<string> => name;\n\nexport const LIMIT = 4;\n",
        ),
    ]));
    let f = node(&a, "p/a.greet");
    assert_eq!(f.label, "Function");
    assert!(
        text(&f.props["signature"]).contains("(name: string): Promise<string>"),
        "{:?}",
        f.props
    );
    assert_eq!(f.props["is_async"], Value::Bool(true));
    assert_eq!(text(&f.props["doc_comment"]), "Greets.");

    let c = node(&a, "p/a.LIMIT");
    assert_eq!(c.label, "Const");
    assert_eq!(text(&c.props["value"]), "4");
}

/// A value is recorded as written, never evaluated, with its annotation
/// under `signature` — the family's const/var convention.
#[test]
fn consts_and_vars_carry_type_and_value_as_written() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        (
            "a.ts",
            "export const BUDGET: number = 256 * 1024;\nlet registry: Map<string, number>;\n",
        ),
    ]));
    let c = node(&a, "p/a.BUDGET");
    assert_eq!(text(&c.props["signature"]), "number");
    assert_eq!(text(&c.props["value"]), "256 * 1024");
    let v = node(&a, "p/a.registry");
    assert_eq!(v.label, "Var");
    assert_eq!(text(&v.props["signature"]), "Map<string, number>");
    assert!(!v.props.contains_key("value"));
}

/// A class carries its fields the way the Rust parser writes them — a list
/// of `name: type` in declaration order — and contains its methods.
#[test]
fn a_class_has_fields_and_methods() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        (
            "db.ts",
            r#"/** A connection. */
export class Client {
  private url: string;
  retries = 3;

  constructor(url: string) { this.url = url; }

  /** Opens. */
  async connect(): Promise<void> { this.log("open"); }

  private log(msg: string) {}
}
"#,
        ),
    ]));
    let c = node(&a, "p/db.Client");
    assert_eq!(c.label, "Class");
    let fields = c.props["fields"]["$value"].as_array().unwrap();
    assert_eq!(fields[0], Value::String("url: string".into()));
    assert_eq!(fields[1], Value::String("retries".into()));

    let m = node(&a, "p/db.Client.connect");
    assert_eq!(m.label, "Method");
    assert_eq!(m.props["is_async"], Value::Bool(true));
    assert_eq!(text(&m.props["doc_comment"]), "Opens.");
    assert!(has_edge(
        &a,
        "p/db.Client",
        "CONTAINS",
        "p/db.Client.connect"
    ));

    let log = node(&a, "p/db.Client.log");
    assert_eq!(text(&log.props["visibility"]), "private");

    // `this.log(…)` resolves lexically — the one receiver a parser does know.
    assert!(has_edge(
        &a,
        "p/db.Client.connect",
        "CALLS",
        "p/db.Client.log"
    ));
    node(&a, "p/db.Client.constructor");
}

/// An interface's demanded methods are nodes reached by HAS_METHOD (the
/// Trait and Go convention); its properties are fields.
#[test]
fn an_interface_has_method_nodes_and_fields() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        (
            "i.ts",
            "export interface Store {\n  size: number;\n  /** Fetches. */\n  get(key: string): Promise<string>;\n}\n",
        ),
    ]));
    let m = node(&a, "p/i.Store.get");
    assert_eq!(m.label, "Method");
    assert!(text(&m.props["signature"]).contains("get(key: string)"));
    assert_eq!(text(&m.props["doc_comment"]), "Fetches.");
    assert!(!m.props.contains_key("visibility"));
    assert!(has_edge(&a, "p/i.Store", "HAS_METHOD", "p/i.Store.get"));

    let i = node(&a, "p/i.Store");
    let fields = i.props["fields"]["$value"].as_array().unwrap();
    assert_eq!(fields[0], Value::String("size: number".into()));
}

/// Enum variants are the enum, values as written — the Rust enum shape.
#[test]
fn enum_variants_are_recorded_as_written() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        (
            "e.ts",
            "export enum Level {\n  Debug,\n  Warn = 10,\n  Error = Warn * 2,\n}\n",
        ),
    ]));
    let e = node(&a, "p/e.Level");
    assert_eq!(e.label, "Enum");
    let variants = e.props["variants"]["$value"].as_array().unwrap();
    assert_eq!(variants[0], Value::String("Debug".into()));
    assert_eq!(variants[1], Value::String("Warn = 10".into()));
    assert_eq!(variants[2], Value::String("Error = Warn * 2".into()));
}

/// A type alias puts the aliased type under `signature`.
#[test]
fn a_type_alias_records_its_type() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        ("t.ts", "export type Pair = [number, number];\n"),
    ]));
    let t = node(&a, "p/t.Pair");
    assert_eq!(t.label, "TypeAlias");
    assert_eq!(text(&t.props["signature"]), "[number, number]");
}

/// `export default function named` keys under its own name and exports as
/// `default`; an anonymous default is simply `default`.
#[test]
fn default_exports_keep_their_declared_name() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        ("a.ts", "export default function boot() {}\n"),
        (
            "b.ts",
            "import start from './a';\n\nexport function go() { start(); }\n",
        ),
    ]));
    node(&a, "p/a.boot");
    // The default import binds to the declaration behind `default`.
    assert!(has_edge(&a, "p/b.go", "CALLS", "p/a.boot"));
}

// ---- resolution -----------------------------------------------------------

/// A named import binds through the file's import table, alias included.
#[test]
fn named_imports_resolve_alias_included() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        ("util.ts", "export function fmt() {}\n"),
        (
            "main.ts",
            "import { fmt as pretty } from './util';\n\nexport function run() { pretty(); }\n",
        ),
    ]));
    assert!(has_edge(&a, "p/main.run", "CALLS", "p/util.fmt"));
    assert!(has_edge(&a, "p/main", "IMPORTS", "p/util"));
}

/// `ns.foo()` resolves through a namespace import — the one member call
/// whose receiver a parser does know.
#[test]
fn namespace_member_calls_resolve() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        ("util.ts", "export function fmt() {}\n"),
        (
            "main.ts",
            "import * as util from './util';\n\nexport function run() { util.fmt(); }\n",
        ),
    ]));
    assert!(has_edge(&a, "p/main.run", "CALLS", "p/util.fmt"));
}

/// `./x.js` in an ESM import means `x.ts` on disk; `./dir` means its index.
#[test]
fn specifiers_probe_extensions_and_index() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        ("lib/index.ts", "export function inner() {}\n"),
        ("util.ts", "export function fmt() {}\n"),
        (
            "main.ts",
            "import { fmt } from './util.js';\nimport { inner } from './lib';\n\nexport function run() { fmt(); inner(); }\n",
        ),
    ]));
    assert!(has_edge(&a, "p/main.run", "CALLS", "p/util.fmt"));
    assert!(has_edge(&a, "p/main.run", "CALLS", "p/lib.inner"));
}

/// A barrel file republishes another module's surface; the chain is chased.
#[test]
fn reexport_chains_resolve() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        ("core/impl.ts", "export function deep() {}\n"),
        (
            "core/index.ts",
            "export { deep } from './impl';\nexport * from './wide';\n",
        ),
        ("core/wide.ts", "export function wide() {}\n"),
        (
            "main.ts",
            "import { deep, wide } from './core';\n\nexport function run() { deep(); wide(); }\n",
        ),
    ]));
    assert!(has_edge(&a, "p/main.run", "CALLS", "p/core/impl.deep"));
    assert!(has_edge(&a, "p/main.run", "CALLS", "p/core/wide.wide"));
}

/// A call into another package becomes an external stand-in carrying the
/// specifier and nothing else — and the note says how many.
#[test]
fn external_calls_become_stand_ins() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        (
            "main.ts",
            "import { z } from 'zod';\nimport traverse from '@babel/traverse';\n\nexport function run() { z(); traverse(); }\n",
        ),
    ]));
    let ext = node(&a, "zod.z");
    assert_eq!(ext.label, "Function");
    assert_eq!(ext.extra_labels, vec!["External".to_string()]);
    assert!(ext.props.is_empty());
    assert_eq!(node(&a, "zod").label, "Package");
    // A scoped package keeps both segments; its default import is the package.
    assert!(has_edge(&a, "p/main.run", "CALLS", "@babel/traverse"));
    note_containing(&a, "other packages");
}

/// A member call on a value is what a checker would know — counted, never
/// guessed.
#[test]
fn member_calls_are_counted_unresolved() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        (
            "a.ts",
            "export function run(db: { query(): void }) { db.query(); }\n",
        ),
    ]));
    note_containing(&a, "left unresolved");
    assert!(!a.edges.iter().any(|e| e.ty == "CALLS"));
}

/// `implements` and `extends` are syntax here, not inference — resolved
/// through the same import table, external when foreign.
#[test]
fn implements_and_extends_resolve() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        (
            "shapes.ts",
            "export interface Drawable { draw(): void; }\n\nexport class Base {}\n",
        ),
        (
            "main.ts",
            "import { Drawable, Base } from './shapes';\nimport { Component } from 'react';\n\nexport class Widget extends Base implements Drawable {\n  draw() {}\n}\n\nexport interface Fancy extends Drawable {}\n\nexport class App extends Component {}\n",
        ),
    ]));
    assert!(has_edge(&a, "p/main.Widget", "EXTENDS", "p/shapes.Base"));
    assert!(has_edge(
        &a,
        "p/main.Widget",
        "IMPLEMENTS",
        "p/shapes.Drawable"
    ));
    assert!(has_edge(&a, "p/main.Fancy", "EXTENDS", "p/shapes.Drawable"));
    let ext = node(&a, "react.Component");
    assert!(ext.extra_labels.contains(&"External".to_string()));
    assert!(has_edge(&a, "p/main.App", "EXTENDS", "react.Component"));
}

/// A rendered JSX component is a function called.
#[test]
fn jsx_components_are_calls() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        ("button.tsx", "export const Button = () => null;\n"),
        (
            "app.tsx",
            "import { Button } from './button';\n\nexport const App = () => <div><Button /></div>;\n",
        ),
    ]));
    assert!(has_edge(&a, "p/app.App", "CALLS", "p/button.Button"));
}

/// Plain JavaScript parses with the same conventions — same parser, another
/// syntax flag.
#[test]
fn plain_javascript_parses_too() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        ("legacy.js", "export function oldie() {}\n"),
        (
            "new.mjs",
            "import { oldie } from './legacy.js';\n\nexport const run = () => oldie();\n",
        ),
    ]));
    assert!(has_edge(&a, "p/new.run", "CALLS", "p/legacy.oldie"));
}

// ---- provenance -----------------------------------------------------------

/// Every definition knows its file and line, and every written relation
/// knows the line it is written on.
#[test]
fn lines_and_files_are_recorded() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        (
            "a.ts",
            "import { helper } from './b';\n\nexport function caller() {\n  helper();\n}\n",
        ),
        ("b.ts", "export function helper() {}\n"),
    ]));
    let f = node(&a, "p/a.caller");
    assert_eq!(f.props["line"], Value::from(3u64));
    assert_eq!(text(&f.props["file"]), "a.ts");
    assert_eq!(
        edge(&a, "CALLS", "p/b.helper").props["line"],
        Value::from(4u64)
    );
    assert_eq!(edge(&a, "IMPORTS", "p/b").props["line"], Value::from(1u64));
    assert_eq!(
        edge(&a, "CONTAINS", "p/a.caller").props["line"],
        Value::from(3u64)
    );
    // The module owns its file: `path` says which one, no line of its own.
    let m = node(&a, "p/a");
    assert_eq!(text(&m.props["path"]), "a.ts");
    assert!(!m.props.contains_key("line"));
}

/// The package node exists and contains its modules.
#[test]
fn the_package_contains_its_modules() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        ("a.ts", "export function f() {}\n"),
    ]));
    assert_eq!(node(&a, "p").label, "Package");
    assert!(has_edge(&a, "p", "CONTAINS", "p/a"));
}

// ---- robustness -----------------------------------------------------------

/// A file that will not parse is counted, and takes nothing else down.
#[test]
fn a_parse_error_is_counted_not_fatal() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        ("ok.ts", "export function fine() {}\n"),
        ("bad.ts", "export function {{{\n"),
    ]));
    assert_eq!(a.skipped, 1);
    node(&a, "p/ok.fine");
}

/// Unresolvable import specifiers — assets, files outside the tree — are
/// counted, not guessed.
#[test]
fn missing_specifiers_are_counted() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        (
            "a.ts",
            "import './styles.css';\nimport { gone } from './deleted';\n\nexport function f() {}\n",
        ),
    ]));
    note_containing(&a, "named no file");
}

/// The result must not depend on where the chunk boundaries fell.
#[test]
fn chunk_boundaries_do_not_change_the_result() {
    let m = tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        (
            "a.ts",
            "import { b } from './b';\n\nexport const run = () => b();\n",
        ),
        ("b.ts", "export function b() {}\n"),
    ]);
    let mut paths = m.list(".ts").unwrap();
    paths.sort();

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
        ("package.json", r#"{ "name": "p" }"#),
        (
            "a.tsx",
            "import * as u from './u';\n\nexport class C { m() { this.m(); u.go(); } }\n\nexport const V = <C />;\n",
        ),
        ("u.ts", "export function go() {}\n"),
    ]);
    let paths = {
        let mut p = m.list(".ts").unwrap();
        p.extend(m.list(".tsx").unwrap());
        p.sort();
        p
    };
    let dump = |a: &Assembled| serde_json::to_string(&(&a.nodes, &a.edges, &a.notes)).unwrap();
    let left = dump(&assemble(parse_chunk(&m, &paths, false)));
    let right = dump(&assemble(parse_chunk(&m, &paths, false)));
    assert_eq!(left, right);
}

/// A pushed document parses alone, its stem standing in for a package.
#[test]
fn a_document_parses_alone() {
    let facts = parse_document("snippet.ts", b"export function solo() {}\n", false);
    let a = assemble(facts);
    node(&a, "snippet.solo");
}

/// include_source attaches the declaration as written under `_code`.
#[test]
fn include_source_attaches_the_declaration() {
    let m = tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        ("a.ts", "export function shown(): number { return 42; }\n"),
    ]);
    let paths = m.list(".ts").unwrap();
    let a = assemble(parse_chunk(&m, &paths, true));
    let n = node(&a, "p/a.shown");
    assert!(
        text(&n.props["_code"]["$value"]).contains("return 42"),
        "{:?}",
        n.props
    );
}

// ---- CommonJS -------------------------------------------------------------

/// `require` is an import wearing a call's syntax, and `module.exports` is
/// the export list — the module system may be older than ESM, but it is
/// written down all the same.
#[test]
fn commonjs_requires_and_exports_resolve() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        (
            "util.js",
            "const helper = () => {};\nconst also = () => {};\n\nmodule.exports = { helper, renamed: also };\n",
        ),
        (
            "main.js",
            "const util = require('./util');\nconst { helper } = require('./util');\nconst express = require('express');\n\nexports.run = () => { util.helper(); helper(); util.renamed(); express(); };\n",
        ),
    ]));
    // The whole-module binding reaches exports by member…
    assert!(has_edge(&a, "p/main.run", "CALLS", "p/util.helper"));
    // …a destructured one binds the name directly…
    assert!(has_edge(&a, "p/main.run", "CALLS", "p/util.also"));
    // …and a bare require of another package is external.
    assert!(has_edge(&a, "p/main.run", "CALLS", "express"));
    assert!(has_edge(&a, "p/main", "IMPORTS", "p/util"));
    assert!(has_edge(&a, "p/main", "IMPORTS", "express"));
    // module.exports marked the locals exported.
    assert_eq!(
        text(&node(&a, "p/util.helper").props["visibility"]),
        "exported"
    );
    // exports.run declared a Function node.
    assert_eq!(node(&a, "p/main.run").label, "Function");
}

/// `module.exports = fn` makes the module *be* that function; requiring and
/// calling it lands on the declaration.
#[test]
fn commonjs_default_module_resolves() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        (
            "logger.js",
            "const make = () => {};\n\nmodule.exports = make;\n",
        ),
        (
            "app.js",
            "const makeLogger = require('./logger');\n\nexports.boot = () => { makeLogger(); };\n",
        ),
    ]));
    assert!(has_edge(&a, "p/app.boot", "CALLS", "p/logger.make"));
}

/// A lazy require inside a body is still an import — recorded as one, never
/// counted as an unresolved call.
#[test]
fn a_lazy_require_is_an_import() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        (
            "a.js",
            "exports.later = () => { const m = require('./b'); };\n",
        ),
        ("b.js", "module.exports = {};\n"),
    ]));
    assert!(has_edge(&a, "p/a", "IMPORTS", "p/b"));
    for n in &a.notes {
        assert!(
            !n.contains("unresolved"),
            "require must not count as a call: {n}"
        );
    }
}

// ---- baseline Tier B: receiver typing, inheritance, super, field chains --
// (mined from codegraph + codebase-memory-mcp ts/tsx resolution suites)

/// An annotated parameter names the receiver's class — and two same-named
/// methods never cross-attribute.
#[test]
fn annotated_params_type_receivers_without_crossing() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        (
            "svc.ts",
            "export class Logger { log(): number { return 1; } }\nexport class Other { log(): number { return 2; } }\nexport function use(lg: Logger): number { return lg.log(); }\nexport function useOther(o: Other): number { return o.log(); }\n",
        ),
    ]));
    assert!(
        has_edge(&a, "p/svc.use", "CALLS", "p/svc.Logger.log"),
        "{:?}",
        a.edges
    );
    assert!(has_edge(&a, "p/svc.useOther", "CALLS", "p/svc.Other.log"));
    assert!(!has_edge(&a, "p/svc.use", "CALLS", "p/svc.Other.log"));
    assert!(!has_edge(&a, "p/svc.useOther", "CALLS", "p/svc.Logger.log"));
}

/// `const lg = new Logger()` types the local; so does an annotated const.
#[test]
fn new_and_annotations_type_locals() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        (
            "m.ts",
            "export class Logger { log(): number { return 1; } }\nexport function use(): number { const lg = new Logger(); return lg.log(); }\nexport function useAnn(l: Logger): number { const x: Logger = l; return x.log(); }\n",
        ),
    ]));
    assert!(
        has_edge(&a, "p/m.use", "CALLS", "p/m.Logger.log"),
        "{:?}",
        a.edges
    );
    assert!(has_edge(&a, "p/m.useAnn", "CALLS", "p/m.Logger.log"));
}

/// A factory's declared return types the value — and `await make()` unwraps
/// `Promise<T>` to the same class, cross-file through the import.
#[test]
fn declared_returns_type_factory_results() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        (
            "lib.ts",
            "export class Conn { ping(): number { return 1; } }\nexport function open(): Conn { return new Conn(); }\nexport async function connect(): Promise<Conn> { return new Conn(); }\n",
        ),
        (
            "app.ts",
            "import { open, connect } from './lib';\nexport function go(): number { const c = open(); return c.ping(); }\nexport async function goAsync(): Promise<number> { const c = await connect(); return c.ping(); }\n",
        ),
    ]));
    assert!(
        has_edge(&a, "p/app.go", "CALLS", "p/lib.Conn.ping"),
        "{:?}",
        a.edges
    );
    assert!(has_edge(&a, "p/app.goAsync", "CALLS", "p/lib.Conn.ping"));
}

/// `this.m()` walks the extends chain; a method on the base resolves from
/// the subclass — and instance dispatch through a typed param does too.
#[test]
fn inheritance_walks_the_extends_chain() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        (
            "m.ts",
            "export class Animal { breathe(): number { return 1; } }\nexport class Dog extends Animal { bark(): number { return this.breathe(); } }\nexport function walk(d: Dog): number { return d.breathe(); }\n",
        ),
    ]));
    assert!(
        has_edge(&a, "p/m.Dog.bark", "CALLS", "p/m.Animal.breathe"),
        "{:?}",
        a.edges
    );
    assert!(has_edge(&a, "p/m.walk", "CALLS", "p/m.Animal.breathe"));
}

/// `super.run()` starts at the bases — never the own class, even when it
/// overrides the name.
#[test]
fn super_calls_resolve_on_the_base() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        (
            "m.ts",
            "export class Base { run(): number { return 1; } }\nexport class Child extends Base { run(): number { return super.run(); } }\n",
        ),
    ]));
    assert!(
        has_edge(&a, "p/m.Child.run", "CALLS", "p/m.Base.run"),
        "{:?}",
        a.edges
    );
    assert!(!has_edge(&a, "p/m.Child.run", "CALLS", "p/m.Child.run"));
}

/// A class receiver reaches static methods: `Util.helper()`.
#[test]
fn class_receivers_reach_statics() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        (
            "m.ts",
            "export class Util { static helper(): number { return 1; } }\nexport function use(): number { return Util.helper(); }\n",
        ),
    ]));
    assert!(
        has_edge(&a, "p/m.use", "CALLS", "p/m.Util.helper"),
        "{:?}",
        a.edges
    );
}

/// `this.logger.log()` resolves through the declared property's class; a
/// one-hop field chain on a typed local (`o.tool.fire()`) resolves too.
#[test]
fn declared_properties_type_field_chains() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        (
            "m.ts",
            "export class Logger { log(): number { return 1; } }\nexport class Tool { fire(): number { return 2; } }\nexport class App {\n  logger: Logger = new Logger();\n  tool = new Tool();\n  render(): number { return this.logger.log(); }\n}\nexport function run(app: App): number { return app.tool.fire(); }\n",
        ),
    ]));
    assert!(
        has_edge(&a, "p/m.App.render", "CALLS", "p/m.Logger.log"),
        "{:?}",
        a.edges
    );
    assert!(has_edge(&a, "p/m.run", "CALLS", "p/m.Tool.fire"));
}

/// The typed paths carry stamps naming the strategy that bound them.
#[test]
fn receiver_resolutions_are_stamped() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        (
            "m.ts",
            "export class Logger { log(): number { return 1; } }\nexport function use(lg: Logger): number { return lg.log(); }\n",
        ),
    ]));
    let e = a
        .edges
        .iter()
        .find(|e| e.src == "p/m.use" && e.dst == "p/m.Logger.log")
        .expect("edge");
    assert_eq!(
        e.props.get("_resolved_by").and_then(|v| v.as_str()),
        Some("receiver")
    );
}

/// C4 (narrow): a bare in-tree function passed as an argument is a
/// REFERENCES fact — a class passed as a value is NOT (not a fn value in
/// TS); never a self-loop.
#[test]
fn functions_passed_as_values_become_references() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        (
            "m.ts",
            "export function handler(): void {}\nexport class Strategy {}\nexport function accept(cb: () => void): void { cb(); }\nexport function wire(): void { accept(handler); }\nexport function classy(consume: (x: unknown) => void): void { consume(Strategy); }\nexport function retry(): void { accept(retry); }\n",
        ),
    ]));
    assert!(
        a.edges
            .iter()
            .any(|e| e.ty == "REFERENCES" && e.src == "p/m.wire" && e.dst == "p/m.handler"),
        "{:?}",
        a.edges
            .iter()
            .filter(|e| e.ty == "REFERENCES")
            .collect::<Vec<_>>()
    );
    assert!(
        !a.edges
            .iter()
            .any(|e| e.ty == "REFERENCES" && e.dst == "p/m.Strategy"),
        "a class is not a fn value in TS"
    );
    assert!(
        !a.edges
            .iter()
            .any(|e| e.ty == "REFERENCES" && e.src == "p/m.retry" && e.dst == "p/m.retry"),
        "no self-loop"
    );
}
