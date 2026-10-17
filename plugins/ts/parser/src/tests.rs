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

/// A user HOF's function-typed parameter annotation types the inline
/// callback's parameter — cross-file through the import too.
#[test]
fn callback_params_type_through_the_callees_annotation() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "p" }"#),
        (
            "lib.ts",
            "export class Foo { run(): number { return 1; } }\nexport function withFoo(cb: (x: Foo) => void): void { cb(new Foo()); }\n",
        ),
        (
            "app.ts",
            "import { withFoo } from './lib';\nexport function go(): void { withFoo((x) => { x.run(); }); }\n",
        ),
    ]));
    assert!(
        a.edges
            .iter()
            .any(|e| e.ty == "CALLS" && e.src == "p/app.go" && e.dst == "p/lib.Foo.run"),
        "{:?}",
        a.edges
            .iter()
            .filter(|e| e.ty == "CALLS")
            .map(|e| (e.src.as_str(), e.dst.as_str()))
            .collect::<Vec<_>>()
    );
}

/// The `concurrent` value on a CALLS edge, or "" when it carries none.
fn concurrency(a: &Assembled, src: &str, dst: &str) -> String {
    a.edges
        .iter()
        .find(|e| e.src == src && e.ty == "CALLS" && e.dst == dst)
        .and_then(|e| e.props.get("concurrent"))
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// JavaScript has no channel and no goroutine; its whole concurrency is
/// where control stops waiting, and that is written at the call site.
/// `is_async` said a function returns a promise; nothing said whether anyone
/// awaited it, and `await f()`, `Promise.all([f()])` and a bare `f()` were
/// one indistinguishable edge.
#[test]
fn a_call_site_says_whether_anyone_awaits_the_promise() {
    let m = tree(vec![(
        "app.ts",
        r#"
export async function fetchOne(u: string) { return u }
export async function worker() { return 1 }
export function tick() { return 2 }

export async function main() {
  await fetchOne("a")
  await Promise.all([fetchOne("b"), worker()])
  setTimeout(tick, 0)
}
"#,
    )]);
    let a = run(&m);
    assert_eq!(
        concurrency(&a, "package/app.main", "package/app.fetchOne"),
        "scheduled"
    );
    assert_eq!(
        concurrency(&a, "package/app.main", "package/app.worker"),
        "scheduled"
    );
    // Scheduling is evident from the syntax whatever the callee is — and a
    // scheduler handed the function itself rather than a promise runs it
    // beside its caller though nothing in this body ever calls it.
    let handed = a
        .edges
        .iter()
        .find(|e| {
            e.src == "package/app.main" && e.ty == "REFERENCES" && e.dst == "package/app.tick"
        })
        .and_then(|e| e.props.get("concurrent"))
        .and_then(|v| v.as_str());
    assert_eq!(handed, Some("scheduled"), "{:?}", a.edges);
}

/// A floating promise — called as a statement, nothing awaits it, no `.then`
/// follows — is the unhandled-rejection shape and the one the graph could
/// not show at all. A plain call discarded the same way is not concurrency.
#[test]
fn a_floating_promise_is_marked_and_a_plain_statement_is_not() {
    let m = tree(vec![(
        "app.ts",
        r#"
export async function fetchOne(u: string) { return u }
export function log(m: string) { return m }
export async function worker() { return 1 }

export async function main() {
  fetchOne("forgotten")
  void fetchOne("deliberate")
  log("fine")
  fetchOne("chained").then(() => worker())
  await fetchOne("waited")
}
"#,
    )]);
    let a = run(&m);
    // One caller reaches one callee several ways and they fold to one edge,
    // so the union is the honest statement — the awaited site must not
    // swallow the floating one merely by being written last.
    assert_eq!(
        concurrency(&a, "package/app.main", "package/app.fetchOne"),
        "unawaited"
    );
    // Discarding a plain call's result is not concurrency: there was
    // nothing there to await.
    assert_eq!(concurrency(&a, "package/app.main", "package/app.log"), "");
    // A `.then` handles the promise, so the call it follows is not floating.
    assert_eq!(
        concurrency(&a, "package/app.main", "package/app.worker"),
        ""
    );
}

/// The three shapes the JS runners collect by default. A file that merely
/// contains the word test in its name (`latest.ts`) is not one of them.
#[test]
fn test_files_are_flagged_by_the_names_runners_collect() {
    let a = run(&tree(vec![
        ("package.json", "{\"name\": \"p\"}"),
        (
            "src/util.ts",
            "export function fmt(x: string) { return x; }\n",
        ),
        ("src/latest.ts", "export function newest() { return 1; }\n"),
        (
            "src/util.test.ts",
            "import { fmt } from './util';\nexport function checksFmt() { return fmt('a'); }\n",
        ),
        (
            "src/api.spec.ts",
            "export function checksApi() { return 1; }\n",
        ),
        (
            "src/__tests__/deep.ts",
            "export function nested() { return 1; }\n",
        ),
    ]));

    for key in [
        "p/src/util.test",
        "p/src/util.test.checksFmt",
        "p/src/api.spec",
        "p/src/api.spec.checksApi",
        "p/src/__tests__/deep",
        "p/src/__tests__/deep.nested",
    ] {
        assert_eq!(
            node(&a, key).props["test_flag"]["$value"],
            Value::from("filename"),
            "{key}"
        );
        assert_eq!(
            node(&a, key).props["_test_flag_confidence"],
            Value::from("strong"),
            "{key}"
        );
    }

    for key in [
        "p/src/util",
        "p/src/util.fmt",
        "p/src/latest",
        "p/src/latest.newest",
    ] {
        assert!(
            !node(&a, key).props.contains_key("test_flag"),
            "{key} must not be flagged"
        );
    }
}

/// A parameter property declares a field, so its type belongs to the class
/// and types `this.store` — the shorthand most TypeScript services are
/// written with, and which said nothing at all until it was walked.
#[test]
fn a_parameter_property_is_a_field_of_its_class() {
    let a = run(&tree(vec![
        ("package.json", "{\"name\": \"p\"}"),
        ("src/store.ts", "export interface Store { get(k: string): string; }\n"),
        (
            "src/svc.ts",
            "import { Store } from './store';\nexport class Svc {\n               constructor(private store: Store, label: string) {}\n               load(k: string): string { return this.store.get(k); }\n}\n",
        ),
    ]));

    // The class names the type, not the constructor: a field was declared.
    assert!(has_edge(&a, "p/src/svc.Svc", "USES_TYPE", "p/src/store.Store"));
    assert!(!has_edge(
        &a,
        "p/src/svc.Svc.constructor",
        "USES_TYPE",
        "p/src/store.Store"
    ));
    assert_eq!(
        text(&edge(&a, "USES_TYPE", "p/src/store.Store").props["role"]["$value"]),
        "field"
    );

    // The field also types `this.store`, which is what lets a call through it
    // be resolved at all. Whether the call to an *interface* method becomes an
    // edge is a separate question, pinned by its own test.
    assert!(has_edge(&a, "p/src/svc.Svc", "USES_TYPE", "p/src/store.Store"));
}

/// A declared type is a dependency as surely as a call is — where TypeScript
/// wrote the annotation. Fields, parameters and returns say so, through array
/// and generic arguments, folded to one edge per pair.
#[test]
fn annotated_types_become_uses_type_edges() {
    let a = run(&tree(vec![
        ("package.json", "{\"name\": \"p\"}"),
        ("src/job.ts", "export class Job { id = 1; }\n"),
        ("src/cfg.ts", "export class Cfg { n = 1; }\n"),
        (
            "src/pool.ts",
            "import { Job } from './job';\nimport { Cfg } from './cfg';\nexport class Pool {\n  queue: Job[] = [];\n  index: Map<string, Cfg> = new Map();\n  label: string = '';\n}\nexport function work(c: Cfg): Job { return new Job(); }\nexport function roundTrip(c: Cfg): Cfg { return c; }\nexport function loose(x) { return x; }\n",
        ),
    ]));

    let role = |src: &str, dst: &str| -> Option<String> {
        a.edges
            .iter()
            .find(|e| e.ty == "USES_TYPE" && e.src == src && e.dst == dst)
            .map(|e| e.props["role"]["$value"].as_str().unwrap().to_string())
    };

    // Through an array element and a generic argument.
    assert_eq!(
        role("p/src/pool.Pool", "p/src/job.Job").as_deref(),
        Some("field")
    );
    assert_eq!(
        role("p/src/pool.Pool", "p/src/cfg.Cfg").as_deref(),
        Some("field")
    );
    assert_eq!(
        role("p/src/pool.work", "p/src/cfg.Cfg").as_deref(),
        Some("param")
    );
    assert_eq!(
        role("p/src/pool.work", "p/src/job.Job").as_deref(),
        Some("return")
    );
    assert_eq!(
        role("p/src/pool.roundTrip", "p/src/cfg.Cfg").as_deref(),
        Some("param, return")
    );

    // An unannotated parameter states no type — and no absence of one.
    assert!(
        !a.edges
            .iter()
            .any(|e| e.ty == "USES_TYPE" && e.src == "p/src/pool.loose"),
        "nothing was written, so nothing is claimed"
    );
    for e in a.edges.iter().filter(|e| e.ty == "USES_TYPE") {
        assert_ne!(e.src, e.dst, "never a self-loop");
        assert!(!e.dst.ends_with("string"), "a builtin is nobody's edge");
        assert!(!e.dst.ends_with("Map"), "a foreign type is nobody's edge");
    }
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
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "web" }"#),
        (
            "src/api.ts",
            r#"
        import axios from 'axios';
        export function viaAxios(id: string) { return axios.get(`/api/orders/${id}`); }
        export function viaFetch() { return fetch('/api/orders'); }
        export function viaConfig() { return axios({ url: '/api/orders', method: 'post' }); }
        export function viaWrapper() { return request('/bridge/check/1'); }
        export function viaMember(id: number) { return this.$http.delete('/api/orders/' + id); }
        "#,
        ),
    ]));
    assert_eq!(
        requests(&a, "web/src/api.viaAxios"),
        vec!["GET /api/orders/{}".to_string()]
    );
    assert_eq!(
        requests(&a, "web/src/api.viaFetch"),
        vec!["* /api/orders".to_string()],
        "fetch names no verb, and `*` is what that means"
    );
    assert_eq!(
        requests(&a, "web/src/api.viaConfig"),
        vec!["POST /api/orders".to_string()],
        "a config object states both"
    );
    assert_eq!(
        requests(&a, "web/src/api.viaWrapper"),
        vec!["* /bridge/check/1".to_string()],
        "the wrapper is visible because the argument is"
    );
}

/// A template's hole is kept, which is what lets it compare equal to a
/// declaration's `{id}`.
#[test]
fn a_template_path_keeps_its_hole() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "web" }"#),
        (
            "src/api.ts",
            "export function one(id: string) { return fetch(`/api/orders/${id}/items`); }",
        ),
    ]));
    assert_eq!(
        requests(&a, "web/src/api.one"),
        vec!["* /api/orders/{}/items".to_string()]
    );
    let written = node(&a, "web/src/api.one").props["requests_as_written"]["$value"][0]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        written, "/api/orders/${}/items",
        "the source's own spelling"
    );
}

/// An absolute URL names the same route as the path inside it, and a query
/// says nothing about which handler serves it.
#[test]
fn an_absolute_url_and_a_query_reduce_to_the_path() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "web" }"#),
        (
            "src/api.ts",
            "export function go() { return fetch('https://api.example.com/v1/orders?page=2'); }",
        ),
    ]));
    assert_eq!(
        requests(&a, "web/src/api.go"),
        vec!["* /v1/orders".to_string()]
    );
}

/// A string that is not path-shaped is not a route, and a function that asks
/// for nothing carries no property at all.
#[test]
fn only_a_path_shaped_argument_counts() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "web" }"#),
        (
            "src/api.ts",
            r#"
        export function plain() { return compute('orders', 2); }
        export function logs() { console.log('a b c'); }
        "#,
        ),
    ]));
    assert!(requests(&a, "web/src/api.plain").is_empty());
    assert!(requests(&a, "web/src/api.logs").is_empty());
    assert!(
        !node(&a, "web/src/api.plain").props.contains_key("requests"),
        "no property rather than an empty one"
    );
}

/// The account says what was recorded, since a request is only half of a
/// route and the other half is matched later.
#[test]
fn the_requests_are_counted_in_the_report() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "web" }"#),
        (
            "src/api.ts",
            "export function go() { return fetch('/api/orders'); }",
        ),
    ]));
    assert!(
        a.notes.iter().any(|n| n.contains("route(s) asked for")),
        "{:?}",
        a.notes
    );
}

// ---- the routes a registration call declares ------------------------------

/// The routes a symbol serves, as the property records them.
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

/// A registration hands its path to a handler, and that handler is what
/// serves it — the one fact that tells `app.get('/x', h)` from
/// `axios.get('/x')`, which are otherwise the same shape.
#[test]
fn a_registration_names_the_handler_that_serves_it() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "web" }"#),
        (
            "src/handlers.ts",
            "export function listOrders(req: any, res: any) { res.send([]); }",
        ),
        (
            "src/app.ts",
            r#"
        import { listOrders } from './handlers';
        const app = express();
        app.get('/api/orders', listOrders);
        app.post('/api/orders', (req, res) => { res.send(); });
        app.delete('/api/orders/:id', auth, (req, res) => { res.send(); });
        "#,
        ),
    ]));
    assert_eq!(
        serves(&a, "web/src/handlers.listOrders"),
        vec!["GET /api/orders".to_string()],
        "an imported handler is named, not the module that registered it"
    );
    assert_eq!(
        serves(&a, "web/src/app"),
        vec![
            "DELETE /api/orders/{}".to_string(),
            "POST /api/orders".to_string()
        ],
        "a handler written in place has no name, so the module holds the route"
    );
}

/// A router's fluent chain registers every route on it, and the verb comes
/// from each call's own name.
#[test]
fn a_fluent_chain_registers_every_route_on_it() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "web" }"#),
        (
            "src/routes.ts",
            r#"
        router.route('/api/orders').get((req, res) => {}).post((req, res) => {});
        "#,
        ),
    ]));
    assert_eq!(
        serves(&a, "web/src/routes"),
        vec![
            "GET /api/orders".to_string(),
            "POST /api/orders".to_string()
        ],
        "the path is stated once and each verb in the chain serves it"
    );
}

/// A second argument that is not a handler leaves the call a request — which
/// is what keeps `axios.get(url, config)` on the side it belongs to.
#[test]
fn a_config_argument_is_not_a_handler() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "web" }"#),
        (
            "src/api.ts",
            r#"
        const config = { timeout: 1000 };
        export function fetchOrders() { return axios.get('/api/orders', config); }
        "#,
        ),
    ]));
    assert_eq!(
        requests(&a, "web/src/api.fetchOrders"),
        vec!["GET /api/orders".to_string()],
        "a name that is no callable hands the path to nobody"
    );
    assert!(
        serves(&a, "web/src/api.fetchOrders").is_empty(),
        "and so declares nothing"
    );
}

/// The account says what was declared, since a declaration is only half of a
/// route and the other half is matched later.
#[test]
fn the_declarations_are_counted_in_the_report() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "web" }"#),
        (
            "src/app.ts",
            "app.get('/api/orders', (req, res) => { res.send(); });",
        ),
    ]));
    assert!(
        a.notes.iter().any(|n| n.contains("declared by a handler")),
        "{:?}",
        a.notes
    );
}

/// A handler held in a `const` is a function too — the arrow idiom, which is
/// how most TypeScript writes one.
#[test]
fn an_arrow_held_in_a_const_is_a_handler() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "web" }"#),
        (
            "src/app.ts",
            r#"
        export const listOrders = (req: any, res: any) => { res.send([]); };
        app.get('/api/orders', listOrders);
        "#,
        ),
    ]));
    assert_eq!(
        serves(&a, "web/src/app.listOrders"),
        vec!["GET /api/orders".to_string()],
        "the arrow is the handler, and it is where the route lands"
    );
    assert!(
        serves(&a, "web/src/app").is_empty(),
        "so the module holds none of it"
    );
}

/// A decorated class is ordinary TypeScript, and the whole file was dropped
/// without the dialect flag that admits it — a Nest or Angular project
/// contributed nothing at all.
#[test]
fn a_decorated_file_is_parsed_not_dropped() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "web" }"#),
        (
            "src/cats.controller.ts",
            r#"
        @Controller('cats')
        export class CatsController {
            constructor(private readonly svc: CatsService) {}
            @Get(':id') one(@Param('id') id: string) { return this.svc.find(id); }
        }
        "#,
        ),
    ]));
    node(&a, "web/src/cats.controller.CatsController");
    node(&a, "web/src/cats.controller.CatsController.one");
    node(&a, "web/src/cats.controller.CatsController.constructor");
    assert_eq!(a.skipped, 0, "nothing was dropped");
}

// ---- the routes a decorator declares --------------------------------------

/// Nest's own spellings: a mount path with no leading slash, a parameter
/// segment written `:id`, a bare `@Get()` taking the class's path, and
/// several paths on one decorator.
#[test]
fn a_decorated_class_declares_what_it_serves() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "web" }"#),
        (
            "src/cats.controller.ts",
            r#"
        import { Controller, Get, Post, Delete } from '@nestjs/common';

        @Controller('cats')
        export class CatsController {
            @Get(':id') one(@Param('id') id: string) { return id; }
            @Post() create() { return null; }
            @Delete(':id') drop() { return null; }
            @Get(['all', 'list']) many() { return []; }
            @Roles('admin') helper() { return null; }
        }
        "#,
        ),
    ]));
    let key = "web/src/cats.controller.CatsController";
    assert_eq!(
        serves(&a, &format!("{key}.one")),
        vec!["GET /cats/{}".to_string()],
        "`cats` and `:id` need no slash to be a path"
    );
    assert_eq!(
        serves(&a, &format!("{key}.create")),
        vec!["POST /cats".to_string()],
        "no path of its own means the class's"
    );
    assert_eq!(
        serves(&a, &format!("{key}.drop")),
        vec!["DELETE /cats/{}".to_string()]
    );
    assert_eq!(
        serves(&a, &format!("{key}.many")),
        vec!["GET /cats/all".to_string(), "GET /cats/list".to_string()],
        "a decorator may carry several paths"
    );
    assert!(
        !node(&a, &format!("{key}.helper"))
            .props
            .contains_key("serves"),
        "a decorator that names no verb and carries no path declares nothing"
    );
}

/// A class-level path with a slash needs no name to be recognised, and the
/// method's own written spelling is kept beside the comparable form.
#[test]
fn a_path_shaped_class_decorator_needs_no_name() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "web" }"#),
        (
            "src/bridge.ts",
            r#"
        @Path('/bridge')
        export class Bridge {
            @GET @Path('/check/{id}') check() { return ''; }
        }
        "#,
        ),
    ]));
    let key = "web/src/bridge.Bridge.check";
    assert_eq!(
        serves(&a, key),
        vec!["GET /bridge/check/{}".to_string()],
        "two decorators on one method are one route"
    );
    assert_eq!(
        node(&a, key).props["serves_as_written"]["$value"][0],
        serde_json::json!("/bridge/check/{id}"),
        "the source's own spelling, kept"
    );
}

/// A decorated class that is nobody's controller declares nothing: the
/// string an ORM or a DI container is given is just a string.
#[test]
fn a_decorator_that_is_not_a_routes_declares_nothing() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "web" }"#),
        (
            "src/cat.entity.ts",
            r#"
        @Entity('cats')
        export class Cat {
            @Column('varchar') name: string;
            rename(to: string) { this.name = to; }
        }
        "#,
        ),
    ]));
    assert!(
        serves(&a, "web/src/cat.entity.Cat.rename").is_empty(),
        "no verb and no path-shaped literal anywhere"
    );
}

/// A mount gives a router a prefix: `app.use('/api', router)` serves nothing
/// and asks for nothing, and the path belongs to the routes hung on that
/// router rather than to this call.
#[test]
fn a_mount_states_no_route_of_its_own() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "web" }"#),
        ("src/router.ts", "export function router() {}"),
        (
            "src/app.ts",
            r#"
        import { router } from './router';
        const app = express();
        app.use('/api', router);
        "#,
        ),
    ]));
    assert!(
        serves(&a, "web/src/router.router").is_empty(),
        "a mounted router is nobody's handler"
    );
    assert!(
        requests(&a, "web/src/app").is_empty() && serves(&a, "web/src/app").is_empty(),
        "and the module that mounted it states no route either"
    );
}

// ---- a client's base path -------------------------------------------------

/// A client built with a base states its paths relative to it, and the two
/// are usually in different files — which is the whole reason this resolves
/// at assemble rather than where the call is read.
#[test]
fn a_clients_base_reaches_the_paths_it_asks_for() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "web" }"#),
        (
            "src/client.ts",
            "import axios from 'axios';\nexport const api = axios.create({ baseURL: '/api/v2' });",
        ),
        (
            "src/orders.ts",
            r#"
        import { api } from './client';
        import axios from 'axios';
        export function load(id: number) { return api.get(`/orders/${id}`); }
        export function loadBare() { return axios.get('/orders'); }
        "#,
        ),
    ]));
    assert_eq!(
        requests(&a, "web/src/orders.load"),
        vec!["GET /api/v2/orders/{}".to_string()],
        "the base reaches across the import that brought the client in"
    );
    assert_eq!(
        requests(&a, "web/src/orders.loadBare"),
        vec!["GET /orders".to_string()],
        "a client with no base states its path whole"
    );
}

/// A base written as a name resolves to the constant behind it, in this
/// module or the one it was imported from.
#[test]
fn a_base_written_as_a_name_resolves() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "web" }"#),
        ("src/config.ts", "export const API_BASE = '/api';"),
        (
            "src/client.ts",
            r#"
        import axios from 'axios';
        import { API_BASE } from './config';
        export const api = axios.create({ baseURL: API_BASE });
        export function load() { return api.get('/orders'); }
        "#,
        ),
    ]));
    assert_eq!(
        requests(&a, "web/src/client.load"),
        vec!["GET /api/orders".to_string()]
    );
}

/// A base a project's own wrapper states is read the same way: the property
/// is the evidence, not the name of the library.
#[test]
fn a_projects_own_client_wrapper_states_a_base_too() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "web" }"#),
        (
            "src/api.ts",
            r#"
        const client = makeClient({ baseURL: '/bridge', retries: 2 });
        export function check(id: number) { return client.get(`/check/${id}`); }
        "#,
        ),
    ]));
    assert_eq!(
        requests(&a, "web/src/api.check"),
        vec!["GET /bridge/check/{}".to_string()]
    );
}

// ---- a mounted router's prefix --------------------------------------------

/// A router carries its routes in one file and is given its prefix in
/// another, which is how every Express app of any size is laid out.
#[test]
fn a_mounted_routers_prefix_reaches_its_routes() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "web" }"#),
        (
            "src/routes/orders.ts",
            r#"
        import { Router } from 'express';
        export function one(req: any, res: any) { res.send(); }
        export function create(req: any, res: any) { res.send(); }
        export const router = Router();
        router.get('/:id', one);
        router.post('/', create);
        "#,
        ),
        (
            "src/app.ts",
            r#"
        import express from 'express';
        import { router } from './routes/orders';
        const app = express();
        app.use('/api/orders', router);
        "#,
        ),
    ]));
    assert_eq!(
        serves(&a, "web/src/routes/orders.one"),
        vec!["GET /api/orders/{}".to_string()],
        "the prefix crossed the file that mounted it"
    );
    assert_eq!(
        serves(&a, "web/src/routes/orders.create"),
        vec!["POST /api/orders".to_string()],
        "a bare `/` adds nothing to the prefix"
    );
}

/// A router mounted on a router is under both, and the two mounts may be
/// read in either file order.
#[test]
fn a_router_under_a_router_is_under_both() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "web" }"#),
        (
            "src/orders.ts",
            r#"
        export function one(req: any, res: any) { res.send(); }
        export const orders = Router();
        orders.get('/:id', one);
        "#,
        ),
        (
            "src/api.ts",
            r#"
        import { orders } from './orders';
        export const api = Router();
        api.use('/orders', orders);
        "#,
        ),
        (
            "src/app.ts",
            r#"
        import { api } from './api';
        const app = express();
        app.use('/api/v1', api);
        "#,
        ),
    ]));
    assert_eq!(
        serves(&a, "web/src/orders.one"),
        vec!["GET /api/v1/orders/{}".to_string()],
        "two mounts, two files, one path"
    );
}

/// A router nothing mounts states its paths as written, rather than taking a
/// prefix from somewhere it was never given one.
#[test]
fn an_unmounted_router_states_its_paths_whole() {
    let a = run(&tree(vec![
        ("package.json", r#"{ "name": "web" }"#),
        (
            "src/standalone.ts",
            r#"
        export function ping(req: any, res: any) { res.send(); }
        export const router = Router();
        router.get('/ping', ping);
        "#,
        ),
    ]));
    assert_eq!(
        serves(&a, "web/src/standalone.ping"),
        vec!["GET /ping".to_string()]
    );
}
