//! TypeScript and JavaScript into facts — nodes and edges a parser is
//! certain of, leaving nothing for a model to guess at.
//!
//! Same discipline as the Rust and Go parsers beside it: parse each file
//! alone (so chunks run concurrently in instances that share nothing),
//! resolve across files once in [`assemble`], and count whatever could not
//! be resolved in the notes rather than dropping it silently. Parse-only —
//! swc's parser, no transforms and no type checker, which is the point:
//! what a checker would have to infer is exactly what this refuses to guess.
//!
//! Keys are logical module identity: the nearest `package.json` names the
//! package (the way the nearest `go.mod` or `Cargo.toml` decides), then the
//! manifest-relative file path without its extension, `/index` collapsing to
//! its directory the way `mod.rs` names its parent. `pkg/src/util.format`
//! is a declaration; `pkg/src/api.Client.connect` is a class member.

mod resolve;

pub use resolve::{Assembled, assemble};

use dr_strange_ext::route;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use swc_common::comments::{Comment, CommentKind, SingleThreadedComments};
use swc_common::errors::SourceMapper as _;
use swc_common::{BytePos, FileName, SourceMap, Span, Spanned, sync::Lrc};
use swc_ecma_ast as ast;
use swc_ecma_parser::{EsSyntax, Parser, StringInput, Syntax, TsSyntax, lexer::Lexer};
use swc_ecma_visit::{Visit, VisitWith};

/// What the parser reads through — the plugin contract's host, as one small
/// trait so tests can hand in a plain directory walker.
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
    /// Labels asserted rather than chosen — `External` on a stand-in for
    /// something outside the tree that was read.
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

/// One call site, held until every file is known.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Call {
    pub caller: String,
    pub kind: CallKind,
    pub name: String,
    pub line: u64,
    /// How the call site relates to the promise's settlement, when it
    /// departs from waiting for it — see [`Shape`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<Shape>,
}

/// What a call site says about concurrency, when it says anything.
///
/// JavaScript has no channel and no goroutine; its whole concurrency is
/// *where control stops waiting*, and that is written at the call site.
/// `is_async` on a function says it returns a promise; only the call says
/// whether anyone awaited it. `await f()`, `Promise.all([f(), g()])` and a
/// bare `f()` are three different programs and were one CALLS edge.
///
/// Only the departures are recorded: inside an async body an `await` is the
/// expectation, and marking every one would put a property on nearly every
/// edge to say nothing unusual happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Shape {
    /// Started beside its siblings — `Promise.all`, `setTimeout`,
    /// `queueMicrotask`.
    Scheduled,
    /// A floating promise: called as a statement, nothing here awaits it and
    /// no `.then` follows. The unhandled-rejection shape.
    Unawaited,
}

impl Shape {
    fn as_str(self) -> &'static str {
        match self {
            Shape::Scheduled => "scheduled",
            Shape::Unawaited => "unawaited",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallKind {
    /// `foo()` — a bare name: a local declaration or an imported binding.
    Plain,
    /// `ns.foo()` — through a binding that may be a namespace import.
    Qualified(String),
    /// `this.foo()` inside a class body — lexical, so resolvable.
    This(String),
    /// `new Foo()` — instantiation is a call to the class.
    New,
    /// `super.m()` inside a class body — resolved over the base chain only.
    Super(String),
    /// `this.attr.m()` — resolvable when the class states `attr`'s class.
    SelfAttr { class: String, attr: String },
    /// `o.field.m()` — resolvable when both the local and the field have
    /// stated classes.
    FieldChain { obj: String, field: String },
}

/// One type position a declaration writes down — a field, a parameter, a
/// return — resolved in [`assemble`] into a `USES_TYPE` edge when it names a
/// type this tree declares.
///
/// Only where an annotation exists: this is a parse-only reader, so an
/// unannotated parameter states no type and no absence of dependency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeRef {
    pub owner: String,
    pub role: String,
    pub name: String,
    pub written: String,
}

/// How a name is bound to a class the source states — an annotation, a
/// `new` expression, or a factory call whose declared return names it.
/// `caller` is a function key for params and locals, a CLASS key for
/// declared properties (`this.name`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hint {
    pub caller: String,
    pub name: String,
    /// The class as written — `Foo`, or `ns.Foo` through a namespace import.
    pub written: String,
    /// True when `written` was a plain call (`open()`): a factory whose
    /// declared return types the value.
    pub constructed: bool,
}

/// One imported binding: `import { a as b } from './x'` binds `b`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportBinding {
    /// The name bound in this file.
    pub local: String,
    /// What it names in the target: an export name, `default`, or `*` for a
    /// namespace import.
    pub imported: String,
    pub specifier: String,
}

/// One export this module declares, by name, to a declared key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Export {
    pub name: String,
    pub key: String,
}

/// `export { a as b } from './x'` — republishing without declaring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReExport {
    pub name: String,
    pub original: String,
    pub specifier: String,
}

/// A local declaration, for call resolution: name → key, and whether the
/// name declares a type-ish thing (interface, type alias) that a call could
/// never target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalDecl {
    pub name: String,
    pub key: String,
    pub callable: bool,
}

/// Everything one file contributes — the opaque partial the component
/// serializes between the two phases.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileFacts {
    pub file: String,
    pub module_id: String,
    pub package: String,
    pub failed: bool,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub calls: Vec<Call>,
    pub bindings: Vec<ImportBinding>,
    /// Import statements as written, with their lines — for IMPORTS edges
    /// and the module's `imports` property.
    pub imports: Vec<(String, u64)>,
    pub exports: Vec<Export>,
    pub reexports: Vec<ReExport>,
    /// `export * from './x'` — the whole surface of another module,
    /// republished. Chased transitively at assemble, cycle-guarded.
    pub reexport_all: Vec<String>,
    pub decls: Vec<LocalDecl>,
    /// `Class → method names`, for `this.m()` resolution.
    pub class_methods: Vec<(String, String)>,
    /// Classes that wrote `implements` / `extends`, held for resolution:
    /// (class key, clause name as written, edge type, line).
    pub clauses: Vec<(String, String, String, u64)>,
    /// Call sites too dynamic to name at all.
    pub opaque: usize,
    /// The classes this file declares, by key.
    pub classes: Vec<String>,
    /// Type bindings the source states (annotations, `new`, factories).
    pub hints: Vec<Hint>,
    /// The type positions this file's declarations write down — only where
    /// an annotation was written, which is all a parse-only reader has.
    #[serde(default)]
    pub type_refs: Vec<TypeRef>,
    /// `(caller, bare name, line)` — functions passed as call arguments.
    /// `(caller, line, bare name, scheduled)` — a function handed as a
    /// value. `setTimeout(tick)` schedules `tick` itself, which is a
    /// concurrency fact about a name this body never calls.
    pub fn_refs: Vec<(String, u64, String, bool)>,
    /// `(caller, callee bare name, arg index, callback param names)` —
    /// inline callbacks whose params the callee's annotation may type.
    pub callback_uses: Vec<(String, String, usize, Vec<String>)>,
    /// `(fn key, arg index, declared callback-arg types)` from signatures
    /// whose params are function-typed (`cb: (x: Foo) => void`).
    pub callback_sigs: Vec<(String, usize, Vec<String>)>,
    /// `(callable key, declared return as written)` when it names a plain
    /// class — `Promise<T>` unwraps to `T`, which is what an async factory
    /// hands the awaiter.
    pub returns: Vec<(String, String)>,
    /// The routes this file's calls state, each with the evidence for which
    /// side of the convention it is on. Split in [`assemble`].
    #[serde(default)]
    pub routes: Vec<RouteSite>,
}

/// One route a call states, and what the call did with it.
///
/// `app.get('/x', h)` and `axios.get('/x')` are the same shape — a verb in
/// the callee's name, a path in an argument — and what tells a declaration
/// from a request is whether the path was handed to a **handler**: a
/// registration hands it to someone, a client call does not.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteSite {
    /// The route as written, on the function the call sits in.
    pub route: route::RouteRef,
    pub handler: Option<Handler>,
}

/// The handler a declaration hands its path to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Handler {
    /// The symbol the route sits on is itself the handler: a method under
    /// its own decorator, or an arrow written into the registration call. A
    /// config object never is, which is what keeps `axios.get('/x', cfg)` a
    /// request.
    InPlace,
    /// Named by a bare identifier, resolved in [`assemble`]: a declaration
    /// only when the name turns out to be an in-tree callable, which is what
    /// keeps `axios.get('/x', config)` a request too.
    Named(String),
}

/// Parse one chunk of paths into per-file facts.
///
/// Pure per file: nothing here looks across files, which is what makes
/// chunks safe to parse concurrently. Resolution — the cross-file half —
/// happens in [`assemble`], once, over every chunk's facts together.
pub fn parse_chunk(files: &dyn Files, paths: &[String], include_source: bool) -> Vec<FileFacts> {
    let mut manifests = ManifestTable::new(files);
    paths
        .iter()
        .map(|p| match files.read(p) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => parse_file(p, &manifests.package_for(p), &text, include_source),
                Err(_) => failed(p),
            },
            Err(_) => failed(p),
        })
        .collect()
}

/// Parse one pushed document. With no tree around it there is no manifest to
/// read, so the file's own stem serves as the package.
pub fn parse_document(name: &str, bytes: &[u8], include_source: bool) -> Vec<FileFacts> {
    let Ok(text) = String::from_utf8(bytes.to_vec()) else {
        return vec![failed(name)];
    };
    let package = Package {
        name: stem(name).to_string(),
        root: String::new(),
    };
    vec![parse_file(name, &package, &text, include_source)]
}

fn failed(path: &str) -> FileFacts {
    FileFacts {
        file: path.to_string(),
        failed: true,
        ..Default::default()
    }
}

#[derive(Clone)]
struct Package {
    name: String,
    /// The directory holding its `package.json`, `""` for the tree root.
    root: String,
}

/// Resolves a file to its package by finding the nearest `package.json`
/// above it — the declared `name` is what decides, the way the Rust parser
/// prefers the manifest over the directory. No manifest → the host's label.
struct ManifestTable<'a> {
    files: &'a dyn Files,
    cache: BTreeMap<String, Package>,
}

impl<'a> ManifestTable<'a> {
    fn new(files: &'a dyn Files) -> Self {
        Self {
            files,
            cache: BTreeMap::new(),
        }
    }

    fn package_for(&mut self, path: &str) -> Package {
        let dir = parent_dir(path);
        if let Some(hit) = self.cache.get(&dir) {
            return hit.clone();
        }
        let resolved = self.resolve(&dir);
        self.cache.insert(dir, resolved.clone());
        resolved
    }

    fn resolve(&self, dir: &str) -> Package {
        let mut at = dir.to_string();
        loop {
            let manifest = if at.is_empty() {
                "package.json".to_string()
            } else {
                format!("{at}/package.json")
            };
            if let Ok(bytes) = self.files.read(&manifest)
                && let Ok(v) = serde_json::from_slice::<Value>(&bytes)
                && let Some(name) = v.get("name").and_then(Value::as_str)
                && !name.is_empty()
            {
                return Package {
                    name: name.to_string(),
                    root: at,
                };
            }
            if at.is_empty() {
                break;
            }
            at = parent_dir(&at);
        }
        Package {
            name: self
                .files
                .label()
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| "package".into()),
            root: String::new(),
        }
    }
}

fn parent_dir(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, _)) => dir.to_string(),
        None => String::new(),
    }
}

fn stem(path: &str) -> &str {
    let base = path.rsplit('/').next().unwrap_or(path);
    base.rsplit_once('.').map(|(s, _)| s).unwrap_or(base)
}

/// The extensions this plugin claims, in resolution-probe order.
pub const EXTENSIONS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

/// `pkg/src/util.ts` under package `pkg` rooted at `""` → `pkg/src/util`;
/// `index` collapses to its directory, and a root `index.ts` names the
/// package itself.
fn module_id(package: &Package, file: &str) -> String {
    let rel = file
        .strip_prefix(&package.root)
        .map(|r| r.trim_start_matches('/'))
        .unwrap_or(file);
    let mut rel = match rel.rsplit_once('.') {
        Some((s, ext)) if EXTENSIONS.contains(&ext) => s,
        _ => rel,
    };
    rel = rel.strip_suffix("/index").unwrap_or(rel);
    // `index` names the package; so does a file named like it — which is
    // what a lone pushed document is.
    if rel == "index" || rel.is_empty() || rel == package.name {
        package.name.clone()
    } else {
        format!("{}/{rel}", package.name)
    }
}

/// Decorators are on for every dialect: a Nest or Angular file is ordinary
/// TypeScript that does not parse at all without them, and a file that parsed
/// before parses the same way with them.
fn syntax_for(path: &str) -> Syntax {
    match path.rsplit('.').next().unwrap_or("") {
        "ts" | "mts" | "cts" => Syntax::Typescript(TsSyntax {
            decorators: true,
            ..Default::default()
        }),
        "tsx" => Syntax::Typescript(TsSyntax {
            tsx: true,
            decorators: true,
            ..Default::default()
        }),
        "jsx" => Syntax::Es(EsSyntax {
            jsx: true,
            decorators: true,
            ..Default::default()
        }),
        _ => Syntax::Es(EsSyntax {
            decorators: true,
            ..Default::default()
        }),
    }
}

fn parse_file(path: &str, package: &Package, text: &str, include_source: bool) -> FileFacts {
    let cm: Lrc<SourceMap> = Default::default();
    let fm = cm.new_source_file(
        Rc::new(FileName::Custom(path.to_string())),
        text.to_string(),
    );
    let comments = SingleThreadedComments::default();
    let lexer = Lexer::new(
        syntax_for(path),
        ast::EsVersion::latest(),
        StringInput::from(&*fm),
        Some(&comments),
    );
    let mut parser = Parser::new_from(lexer);
    let Ok(program) = parser.parse_program() else {
        return failed(path);
    };
    // Recoverable errors still yield a tree; a file that only half-parses is
    // still worth its facts, and the error list is the parser's own business.

    let module = module_id(package, path);
    let mut facts = FileFacts {
        file: path.to_string(),
        module_id: module.clone(),
        package: package.name.clone(),
        ..Default::default()
    };

    let mut w = Walker {
        facts: &mut facts,
        cm,
        comments: &comments,
        module,
        include_source,
    };
    match &program {
        ast::Program::Module(m) => w.module_items(&m.body),
        ast::Program::Script(s) => {
            for stmt in &s.body {
                w.stmt(stmt, false, None);
            }
            for stmt in &s.body {
                w.top_level_route(stmt);
            }
        }
    }

    // The module node, after the walk so its imports are known. Like the
    // Rust parser's file-level module: `path` says which file, so no
    // file/line of its own.
    let mut props = Props::new();
    props.insert("path".into(), Value::String(path.to_string()));
    if !facts.imports.is_empty() {
        let joined = facts
            .imports
            .iter()
            .map(|(s, _)| s.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        props.insert("imports".into(), Value::String(joined));
    }
    facts.nodes.insert(
        0,
        Node {
            key: facts.module_id.clone(),
            label: "Module".into(),
            extra_labels: Vec::new(),
            props,
        },
    );

    // Everything else in this walk came from this file.
    for n in facts.nodes.iter_mut().skip(1) {
        n.props
            .insert("file".into(), Value::String(path.to_string()));
    }

    // What the runners collect by default. `describe`/`it` would be the
    // other candidate and are worse evidence: jest and vitest inject them as
    // globals, so they arrive as unresolved calls and a file that merely
    // *defines* a function called `it` would read the same. The name is what
    // is actually written down — and it is a default a jest/vitest config can
    // move, which is why this is `strong` and not `definitive`.
    if let Some(why) = test_path(path) {
        for n in facts.nodes.iter_mut() {
            set_test_flag(&mut n.props, "filename", &why, "strong");
        }
    }
    facts
}

/// `foo.test.ts`, `foo.spec.tsx`, or anything under a `__tests__` directory —
/// the three shapes the JS test runners look for out of the box.
fn test_path(path: &str) -> Option<String> {
    let norm = path.replace('\\', "/");
    if norm.split('/').any(|seg| seg == "__tests__") {
        return Some(
            "test code: under a `__tests__` directory, which the test runners collect by default"
                .to_string(),
        );
    }
    let base = norm.rsplit('/').next().unwrap_or(norm.as_str());
    let stem = base.rsplit_once('.').map(|(s, _)| s).unwrap_or(base);
    (stem.ends_with(".test") || stem.ends_with(".spec"))
        .then(|| format!("test code: `{base}` is a name the test runners collect by default"))
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

struct Walker<'a> {
    facts: &'a mut FileFacts,
    cm: Lrc<SourceMap>,
    comments: &'a SingleThreadedComments,
    module: String,
    include_source: bool,
}

impl Walker<'_> {
    fn module_items(&mut self, items: &[ast::ModuleItem]) {
        for item in items {
            match item {
                ast::ModuleItem::ModuleDecl(md) => self.module_decl(md),
                ast::ModuleItem::Stmt(stmt) => self.stmt(stmt, false, None),
            }
        }
        self.top_level_routes(items);
    }

    fn module_decl(&mut self, md: &ast::ModuleDecl) {
        match md {
            ast::ModuleDecl::Import(imp) => self.import(imp),
            ast::ModuleDecl::ExportDecl(e) => {
                if let ast::Decl::Var(_) = &e.decl {
                    // Var decls read their own doc; pass the outer span so
                    // `export const` docs attach.
                }
                self.decl(&e.decl, true, Some(e.span));
            }
            ast::ModuleDecl::ExportNamed(named) => self.export_named(named),
            ast::ModuleDecl::ExportDefaultDecl(d) => self.export_default_decl(d),
            ast::ModuleDecl::ExportDefaultExpr(e) => {
                // `export default <expr>` — a value with no declared name.
                let key = format!("{}.default", self.module);
                let mut props = self.common_props(String::new(), e.span, "default");
                props.insert("visibility".into(), Value::String("exported".into()));
                props.insert("end_line".into(), Value::from(self.end_line(e.span)));
                self.push_node(key.clone(), "Const", props, self.line(e.span));
                self.facts.exports.push(Export {
                    name: "default".into(),
                    key,
                });
            }
            ast::ModuleDecl::ExportAll(all) => {
                let spec = all.src.value.to_atom_lossy().to_string();
                self.facts.imports.push((spec.clone(), self.line(all.span)));
                self.facts.reexport_all.push(spec);
            }
            // TS import-equals / export-assignment are legacy module syntax;
            // namespaces are walked nowhere in v1 — one comment, not a guess.
            _ => {}
        }
    }

    fn import(&mut self, imp: &ast::ImportDecl) {
        let spec = imp.src.value.to_atom_lossy().to_string();
        self.facts.imports.push((spec.clone(), self.line(imp.span)));
        for s in &imp.specifiers {
            let binding = match s {
                ast::ImportSpecifier::Named(n) => ImportBinding {
                    local: n.local.sym.to_string(),
                    imported: n
                        .imported
                        .as_ref()
                        .map(export_name)
                        .unwrap_or_else(|| n.local.sym.to_string()),
                    specifier: spec.clone(),
                },
                ast::ImportSpecifier::Default(d) => ImportBinding {
                    local: d.local.sym.to_string(),
                    imported: "default".into(),
                    specifier: spec.clone(),
                },
                ast::ImportSpecifier::Namespace(ns) => ImportBinding {
                    local: ns.local.sym.to_string(),
                    imported: "*".into(),
                    specifier: spec.clone(),
                },
            };
            self.facts.bindings.push(binding);
        }
    }

    fn export_named(&mut self, named: &ast::NamedExport) {
        for s in &named.specifiers {
            let ast::ExportSpecifier::Named(n) = s else {
                continue;
            };
            let orig = export_name_of_export(&n.orig);
            let name = n
                .exported
                .as_ref()
                .map(export_name_of_export)
                .unwrap_or_else(|| orig.clone());
            match &named.src {
                Some(src) => {
                    let spec = src.value.to_atom_lossy().to_string();
                    if !self.facts.imports.iter().any(|(s, _)| *s == spec) {
                        self.facts
                            .imports
                            .push((spec.clone(), self.line(named.span)));
                    }
                    self.facts.reexports.push(ReExport {
                        name,
                        original: orig,
                        specifier: spec,
                    });
                }
                None => {
                    // `export { a as b }` — republish a local declaration.
                    self.facts.exports.push(Export {
                        name,
                        key: format!("{}.{orig}", self.module),
                    });
                    if let Some(d) = self
                        .facts
                        .nodes
                        .iter_mut()
                        .find(|d| d.key == format!("{}.{orig}", self.module))
                    {
                        d.props
                            .insert("visibility".into(), Value::String("exported".into()));
                    }
                }
            }
        }
    }

    fn export_default_decl(&mut self, d: &ast::ExportDefaultDecl) {
        match &d.decl {
            ast::DefaultDecl::Fn(f) => {
                let name = f
                    .ident
                    .as_ref()
                    .map(|i| i.sym.to_string())
                    .unwrap_or_else(|| "default".into());
                self.function(&name, &f.function, true, d.span, "default");
            }
            ast::DefaultDecl::Class(c) => {
                let name = c
                    .ident
                    .as_ref()
                    .map(|i| i.sym.to_string())
                    .unwrap_or_else(|| "default".into());
                self.class(&name, &c.class, true, d.span, "default");
            }
            ast::DefaultDecl::TsInterfaceDecl(i) => {
                self.interface(i, true, d.span);
            }
        }
    }

    fn stmt(&mut self, stmt: &ast::Stmt, exported: bool, outer: Option<Span>) {
        match stmt {
            ast::Stmt::Decl(decl) => self.decl(decl, exported, outer),
            // CommonJS lives in top-level expression statements:
            // `module.exports = …`, `exports.foo = …`, a side-effect
            // `require('./x')`. Syntax, not inference — the module system
            // may be older than ESM, but it is written down all the same.
            ast::Stmt::Expr(es) => self.top_level_expr(&es.expr),
            _ => {}
        }
    }

    fn top_level_expr(&mut self, expr: &ast::Expr) {
        if let Some((spec, _)) = require_spec(expr) {
            self.note_import(&spec, self.line(expr.span()));
            return;
        }
        let ast::Expr::Assign(a) = expr else { return };
        if a.op != ast::AssignOp::Assign {
            return;
        }
        let ast::AssignTarget::Simple(ast::SimpleAssignTarget::Member(target)) = &a.left else {
            return;
        };

        // `module.exports = …`
        if is_module_exports(target) {
            match &*a.right {
                ast::Expr::Object(obj) => {
                    for prop in &obj.props {
                        let ast::PropOrSpread::Prop(prop) = prop else {
                            continue;
                        };
                        match &**prop {
                            ast::Prop::Shorthand(i) => {
                                self.cjs_export(&i.sym, &i.sym);
                            }
                            ast::Prop::KeyValue(kv) => {
                                let Some(name) = prop_name(&kv.key) else {
                                    continue;
                                };
                                if let ast::Expr::Ident(v) = &*kv.value {
                                    self.cjs_export(&name, &v.sym);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                ast::Expr::Ident(i) => {
                    // `module.exports = thing` — the module *is* that thing;
                    // a `require` of it binds to `default`.
                    self.facts.exports.push(Export {
                        name: "default".into(),
                        key: format!("{}.{}", self.module, i.sym),
                    });
                }
                _ => {}
            }
            return;
        }

        // `module.exports.foo = …` / `exports.foo = …`
        let is_exports_member = match &*target.obj {
            ast::Expr::Ident(i) => i.sym == *"exports",
            ast::Expr::Member(inner) => is_module_exports(inner),
            _ => false,
        };
        if is_exports_member && let Some(name) = target.prop.as_ident().map(|p| p.sym.to_string()) {
            match &*a.right {
                ast::Expr::Ident(v) => self.cjs_export(&name, &v.sym),
                ast::Expr::Arrow(arrow) => {
                    let key = format!("{}.{name}", self.module);
                    let sig = self.fn_signature(&name, arrow.span, Some(arrow.body.span()));
                    let mut props = self.common_props(sig, expr.span(), &name);
                    props.insert("visibility".into(), Value::String("exported".into()));
                    let line = self.line(arrow.span);
                    props.insert("end_line".into(), Value::from(self.end_line(arrow.span)));
                    props.insert("end_line".into(), Value::from(self.end_line(a.span)));
                    self.push_contained(key.clone(), "Function", props, line);
                    self.local(&name, &key, true, true);
                    self.facts.exports.push(Export {
                        name,
                        key: key.clone(),
                    });
                    match &*arrow.body {
                        ast::BlockStmtOrExpr::BlockStmt(b) => self.collect_calls(&key, b, None),
                        ast::BlockStmtOrExpr::Expr(e) => self.collect_calls_expr(&key, e, None),
                    }
                }
                ast::Expr::Fn(f) => {
                    self.function(&name, &f.function, true, expr.span(), &name);
                    self.facts.exports.push(Export {
                        name: name.clone(),
                        key: format!("{}.{name}", self.module),
                    });
                }
                _ => {}
            }
        }
    }

    /// Register a CommonJS export of a local declaration, marking it
    /// exported the way `export { a }` does.
    fn cjs_export(&mut self, name: &str, local: &str) {
        let key = format!("{}.{local}", self.module);
        self.facts.exports.push(Export {
            name: name.to_string(),
            key: key.clone(),
        });
        if let Some(d) = self.facts.nodes.iter_mut().find(|d| d.key == key) {
            d.props
                .insert("visibility".into(), Value::String("exported".into()));
        }
    }

    fn note_import(&mut self, spec: &str, line: u64) {
        if !self.facts.imports.iter().any(|(s, _)| s == spec) {
            self.facts.imports.push((spec.to_string(), line));
        }
    }

    fn decl(&mut self, decl: &ast::Decl, exported: bool, outer: Option<Span>) {
        match decl {
            ast::Decl::Fn(f) => {
                let name = f.ident.sym.to_string();
                self.function(
                    &name,
                    &f.function,
                    exported,
                    outer.unwrap_or(f.span()),
                    &name,
                );
            }
            ast::Decl::Class(c) => {
                let name = c.ident.sym.to_string();
                self.class(&name, &c.class, exported, outer.unwrap_or(c.span()), &name);
            }
            ast::Decl::Var(v) => self.var(v, exported, outer),
            ast::Decl::TsInterface(i) => self.interface(i, exported, outer.unwrap_or(i.span)),
            ast::Decl::TsTypeAlias(t) => {
                let name = t.id.sym.to_string();
                let key = format!("{}.{name}", self.module);
                let mut props = self.common_props(
                    self.snippet(t.type_ann.span()),
                    outer.unwrap_or(t.span),
                    &name,
                );
                if exported {
                    props.insert("visibility".into(), Value::String("exported".into()));
                }
                let line = self.line(t.id.span);
                props.insert("end_line".into(), Value::from(self.end_line(t.span)));
                self.push_contained(key.clone(), "TypeAlias", props, line);
                self.local(&name, &key, false, exported);
            }
            ast::Decl::TsEnum(e) => self.ts_enum(e, exported, outer),
            // `namespace X {}` is legacy module syntax; its members are not
            // walked in v1 — a deliberate gap, not an accident.
            ast::Decl::TsModule(_) => {}
            ast::Decl::Using(_) => {}
        }
    }

    /// A written type usable by receiver typing: a plain identifier or a
    /// one-level qualified name (`ns.Foo`). Generics and unions are a
    /// checker's business — except `Promise<T>`, whose `T` is what an
    /// async factory hands the awaiter.
    /// Every named type an annotation mentions, outermost first:
    /// `Map<string, Job[]>` yields `Map`, `string`, `Job`.
    ///
    /// Where [`Walker::type_written`] takes only the head and gives up on
    /// generics — a receiver's methods are the head's — a *dependency* does
    /// not: `jobs: Job[]` depends on `Job`, and that is the fact `USES_TYPE`
    /// records.
    fn type_names(t: &ast::TsType, out: &mut Vec<String>) {
        match t {
            ast::TsType::TsTypeRef(r) => {
                let name = match &r.type_name {
                    ast::TsEntityName::Ident(i) => Some(i.sym.to_string()),
                    ast::TsEntityName::TsQualifiedName(q) => match &q.left {
                        ast::TsEntityName::Ident(l) => Some(format!("{}.{}", l.sym, q.right.sym)),
                        _ => None,
                    },
                };
                if let Some(name) = name {
                    out.push(name);
                }
                if let Some(args) = &r.type_params {
                    for p in &args.params {
                        Self::type_names(p, out);
                    }
                }
            }
            ast::TsType::TsArrayType(a) => Self::type_names(&a.elem_type, out),
            ast::TsType::TsOptionalType(o) => Self::type_names(&o.type_ann, out),
            ast::TsType::TsRestType(r) => Self::type_names(&r.type_ann, out),
            ast::TsType::TsParenthesizedType(p) => Self::type_names(&p.type_ann, out),
            ast::TsType::TsTypeOperator(o) => Self::type_names(&o.type_ann, out),
            ast::TsType::TsTupleType(t) => {
                for e in &t.elem_types {
                    Self::type_names(&e.ty, out);
                }
            }
            ast::TsType::TsUnionOrIntersectionType(u) => match u {
                ast::TsUnionOrIntersectionType::TsUnionType(u) => {
                    for t in &u.types {
                        Self::type_names(t, out);
                    }
                }
                ast::TsUnionOrIntersectionType::TsIntersectionType(i) => {
                    for t in &i.types {
                        Self::type_names(t, out);
                    }
                }
            },
            _ => {}
        }
    }

    /// Record every named type an annotation mentions as one type position of
    /// `owner`.
    fn type_refs(&mut self, owner: &str, role: &str, name: &str, t: &ast::TsType) {
        let mut names = Vec::new();
        Self::type_names(t, &mut names);
        for written in names {
            self.facts.type_refs.push(TypeRef {
                owner: owner.to_string(),
                role: role.to_string(),
                name: name.to_string(),
                written,
            });
        }
    }

    fn type_written(t: &ast::TsType) -> Option<String> {
        let ast::TsType::TsTypeRef(r) = t else {
            return None;
        };
        let name = match &r.type_name {
            ast::TsEntityName::Ident(i) => i.sym.to_string(),
            ast::TsEntityName::TsQualifiedName(q) => {
                let ast::TsEntityName::Ident(l) = &q.left else {
                    return None;
                };
                format!("{}.{}", l.sym, q.right.sym)
            }
        };
        match &r.type_params {
            Some(args) if name == "Promise" && args.params.len() == 1 => {
                Self::type_written(&args.params[0])
            }
            Some(_) => None,
            None => Some(name),
        }
    }

    /// An inline callback argument's parameter names — plain idents only.
    fn callback_params(e: &ast::Expr) -> Option<Vec<String>> {
        let pats: Vec<&ast::Pat> = match e {
            ast::Expr::Arrow(a) => a.params.iter().collect(),
            ast::Expr::Fn(f) => f.function.params.iter().map(|p| &p.pat).collect(),
            _ => return None,
        };
        let names = pats
            .iter()
            .map(|p| match p {
                ast::Pat::Ident(b) => Some(b.id.sym.to_string()),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?;
        (!names.is_empty()).then_some(names)
    }

    /// The callback-argument types a signature declares, per parameter
    /// position — a param annotated with a function type states them.
    fn callback_sig(&self, params: &[ast::Param]) -> Vec<(usize, Vec<String>)> {
        let mut out = Vec::new();
        for (i, param) in params.iter().enumerate() {
            let ast::Pat::Ident(b) = &param.pat else {
                continue;
            };
            let Some(ann) = &b.type_ann else { continue };
            let ast::TsType::TsFnOrConstructorType(ast::TsFnOrConstructorType::TsFnType(f)) =
                &*ann.type_ann
            else {
                continue;
            };
            let args = f
                .params
                .iter()
                .map(|p| match p {
                    ast::TsFnParam::Ident(b) => b
                        .type_ann
                        .as_ref()
                        .and_then(|a| Self::type_written(&a.type_ann)),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>();
            if let Some(args) = args
                && !args.is_empty()
            {
                out.push((i, args));
            }
        }
        out
    }

    fn hint(&mut self, caller: &str, name: String, written: String, constructed: bool) {
        if !self
            .facts
            .hints
            .iter()
            .any(|h| h.caller == caller && h.name == name)
        {
            self.facts.hints.push(Hint {
                caller: caller.to_string(),
                name,
                written,
                constructed,
            });
        }
    }

    /// Receiver-typing inputs of one callable: annotated parameters, the
    /// declared return, and the body's stated bindings (`const x: T`,
    /// `const x = new T()`, `const x = make()`).
    fn fn_hints(&mut self, caller: &str, f: &ast::Function) {
        for param in &f.params {
            if let ast::Pat::Ident(b) = &param.pat
                && let Some(ann) = &b.type_ann
            {
                if let Some(written) = Self::type_written(&ann.type_ann) {
                    self.hint(caller, b.id.sym.to_string(), written, false);
                }
                let pname = b.id.sym.to_string();
                self.type_refs(caller, "param", &pname, &ann.type_ann);
            }
        }
        if let Some(ret) = &f.return_type {
            if let Some(written) = Self::type_written(&ret.type_ann) {
                self.facts.returns.push((caller.to_string(), written));
            }
            self.type_refs(caller, "return", "", &ret.type_ann);
        }
        for (idx, args) in self.callback_sig(&f.params) {
            self.facts
                .callback_sigs
                .push((caller.to_string(), idx, args));
        }
        if let Some(body) = &f.body {
            self.body_hints(caller, body);
        }
    }

    fn body_hints(&mut self, caller: &str, body: &ast::BlockStmt) {
        struct Vars<'a, 'b, 'c> {
            walker: &'a mut Walker<'c>,
            caller: &'b str,
        }
        impl Visit for Vars<'_, '_, '_> {
            fn visit_var_decl(&mut self, node: &ast::VarDecl) {
                for d in &node.decls {
                    let ast::Pat::Ident(b) = &d.name else {
                        continue;
                    };
                    let name = b.id.sym.to_string();
                    if let Some(ann) = &b.type_ann {
                        if let Some(written) = Walker::type_written(&ann.type_ann) {
                            self.walker.hint(self.caller, name, written, false);
                        }
                        continue;
                    }
                    match d.init.as_deref() {
                        Some(ast::Expr::New(n)) => {
                            if let ast::Expr::Ident(i) = &*n.callee {
                                self.walker
                                    .hint(self.caller, name, i.sym.to_string(), false);
                            }
                        }
                        Some(ast::Expr::Await(a)) => {
                            if let ast::Expr::Call(c) = &*a.arg
                                && let ast::Callee::Expr(e) = &c.callee
                                && let ast::Expr::Ident(i) = &**e
                            {
                                self.walker.hint(self.caller, name, i.sym.to_string(), true);
                            }
                        }
                        Some(ast::Expr::Call(c)) => {
                            if let ast::Callee::Expr(e) = &c.callee
                                && let ast::Expr::Ident(i) = &**e
                            {
                                self.walker.hint(self.caller, name, i.sym.to_string(), true);
                            }
                        }
                        _ => {}
                    }
                }
                node.visit_children_with(self);
            }
        }
        body.visit_with(&mut Vars {
            walker: self,
            caller,
        });
    }

    fn function(
        &mut self,
        name: &str,
        f: &ast::Function,
        exported: bool,
        doc_span: Span,
        export_as: &str,
    ) {
        let key = format!("{}.{name}", self.module);
        let sig = self.fn_signature(name, f.span(), f.body.as_ref().map(|b| b.span));
        let mut props = self.common_props(sig, doc_span, name);
        if exported {
            props.insert("visibility".into(), Value::String("exported".into()));
        }
        if f.is_async {
            props.insert("is_async".into(), Value::Bool(true));
        }
        if self.include_source {
            self.add_source(&mut props, f.span());
        }
        let line = self.line(f.span());
        // Overload signatures share the name; the implementation (or the
        // first signature, which carries the docs) wins, silently — they are
        // one logical function, not a collision.
        if self.facts.nodes.iter().any(|n| n.key == key) {
            return;
        }
        props.insert("end_line".into(), Value::from(self.end_line(f.span())));
        self.push_contained(key.clone(), "Function", props, line);
        self.local(name, &key, true, exported);
        if export_as == "default" && name != "default" {
            self.facts.exports.push(Export {
                name: "default".into(),
                key: key.clone(),
            });
        }
        if let Some(body) = &f.body {
            self.collect_calls(&key, body, None);
        }
        self.fn_hints(&key, f);
    }

    fn var(&mut self, v: &ast::VarDecl, exported: bool, outer: Option<Span>) {
        let label = match v.kind {
            ast::VarDeclKind::Const => "Const",
            _ => "Var",
        };
        for d in &v.decls {
            // `const x = require('./y')` is an import wearing a call's
            // syntax — bindings, not a value node, exactly as an ESM import.
            if let Some(init) = d.init.as_deref()
                && let Some((spec, member)) = require_spec(init)
            {
                self.note_import(&spec, self.line(init.span()));
                match &d.name {
                    ast::Pat::Ident(id) => {
                        self.facts.bindings.push(ImportBinding {
                            local: id.id.sym.to_string(),
                            imported: member.unwrap_or_else(|| "*".into()),
                            specifier: spec,
                        });
                    }
                    ast::Pat::Object(obj) => {
                        for prop in &obj.props {
                            match prop {
                                ast::ObjectPatProp::Assign(short) => {
                                    self.facts.bindings.push(ImportBinding {
                                        local: short.key.sym.to_string(),
                                        imported: short.key.sym.to_string(),
                                        specifier: spec.clone(),
                                    });
                                }
                                ast::ObjectPatProp::KeyValue(kv) => {
                                    if let (Some(k), ast::Pat::Ident(v)) =
                                        (prop_name_of_key(&kv.key), &*kv.value)
                                    {
                                        self.facts.bindings.push(ImportBinding {
                                            local: v.id.sym.to_string(),
                                            imported: k,
                                            specifier: spec.clone(),
                                        });
                                    }
                                }
                                ast::ObjectPatProp::Rest(_) => {}
                            }
                        }
                    }
                    _ => {}
                }
                continue;
            }

            let ast::Pat::Ident(ident) = &d.name else {
                // Destructuring declares several names from one value; whose
                // value is whose is a data-flow question, not a parse.
                continue;
            };
            let name = ident.id.sym.to_string();
            let key = format!("{}.{name}", self.module);
            let doc_span = outer.unwrap_or(v.span);

            // `const f = (…) => …` *is* the function — the TS idiom for most
            // of them — so it is labelled one, signature and calls included.
            match d.init.as_deref() {
                Some(ast::Expr::Arrow(a)) => {
                    let sig = self.fn_signature(&name, a.span, Some(a.body.span()));
                    let mut props = self.common_props(sig, doc_span, &name);
                    if exported {
                        props.insert("visibility".into(), Value::String("exported".into()));
                    }
                    if a.is_async {
                        props.insert("is_async".into(), Value::Bool(true));
                    }
                    if self.include_source {
                        self.add_source(&mut props, a.span);
                    }
                    let line = self.line(ident.id.span);
                    self.push_contained(key.clone(), "Function", props, line);
                    self.local(&name, &key, true, exported);
                    match &*a.body {
                        ast::BlockStmtOrExpr::BlockStmt(b) => self.collect_calls(&key, b, None),
                        ast::BlockStmtOrExpr::Expr(e) => self.collect_calls_expr(&key, e, None),
                    }
                    continue;
                }
                Some(ast::Expr::Fn(f)) => {
                    self.function(&name, &f.function, exported, doc_span, &name);
                    continue;
                }
                _ => {}
            }

            let mut props = self.common_props(
                ident
                    .type_ann
                    .as_ref()
                    .map(|t| self.snippet(t.type_ann.span()))
                    .unwrap_or_default(),
                doc_span,
                &name,
            );
            if let Some(init) = &d.init {
                // As written, never evaluated — `256 * 1024` folded wrongly
                // is worse than the expression that produced it.
                props.insert("value".into(), Value::String(self.snippet(init.span())));
            }
            if exported {
                props.insert("visibility".into(), Value::String("exported".into()));
            }
            let line = self.line(ident.id.span);
            props.insert("end_line".into(), Value::from(self.end_line(d.span)));
            self.push_contained(key.clone(), label, props, line);
            self.local(&name, &key, true, exported);
        }
    }

    fn class(
        &mut self,
        name: &str,
        c: &ast::Class,
        exported: bool,
        doc_span: Span,
        export_as: &str,
    ) {
        let key = format!("{}.{name}", self.module);
        let mut props = self.common_props(String::new(), doc_span, name);
        if exported {
            props.insert("visibility".into(), Value::String("exported".into()));
        }

        self.facts.classes.push(key.clone());
        // Fields as the Rust parser writes them: `name: type` in declaration
        // order, a list rather than a map.
        let mut fields: Vec<Value> = Vec::new();
        for m in &c.body {
            if let ast::ClassMember::ClassProp(p) = m
                && let Some(n) = prop_name(&p.key)
            {
                let entry = match &p.type_ann {
                    Some(t) => format!("{n}: {}", self.snippet(t.type_ann.span())),
                    None => n.clone(),
                };
                // A declared property's class types `this.n.m()` chains.
                if let Some(t) = &p.type_ann {
                    self.type_refs(&key, "field", &n, &t.type_ann);
                }
                if let Some(t) = &p.type_ann
                    && let Some(written) = Self::type_written(&t.type_ann)
                {
                    self.hint(&key, n.clone(), written, false);
                } else if let Some(ast::Expr::New(nw)) = p.value.as_deref()
                    && let ast::Expr::Ident(i) = &*nw.callee
                {
                    self.hint(&key, n.clone(), i.sym.to_string(), false);
                }
                fields.push(Value::String(entry));
            }
        }
        if !fields.is_empty() {
            props.insert(
                "fields".into(),
                serde_json::json!({
                    "$desc": "the fields it declares, each with its type as written",
                    "$value": Value::Array(fields),
                }),
            );
        }
        if self.include_source {
            self.add_source(&mut props, c.span);
        }
        let line = self.line(c.span);
        props.insert("end_line".into(), Value::from(self.end_line(c.span)));
        self.push_contained(key.clone(), "Class", props, line);
        self.local(name, &key, true, exported);
        if export_as == "default" && name != "default" {
            self.facts.exports.push(Export {
                name: "default".into(),
                key: key.clone(),
            });
        }

        // `implements` and `extends` are syntax here, not inference — held
        // whole until every file is known.
        if let Some(sc) = &c.super_class
            && let Some(written) = self.clause_name(sc)
        {
            self.facts
                .clauses
                .push((key.clone(), written, "EXTENDS".into(), line));
        }
        for i in &c.implements {
            if let Some(written) = self.expr_with_args_name(i) {
                self.facts
                    .clauses
                    .push((key.clone(), written, "IMPLEMENTS".into(), line));
            }
        }

        for m in &c.body {
            match m {
                ast::ClassMember::Method(method) => {
                    let Some(mname) = prop_name(&method.key) else {
                        continue;
                    };
                    self.class_method(
                        &key,
                        name,
                        &mname,
                        &method.function,
                        method.accessibility,
                        method.span,
                    );
                }
                ast::ClassMember::Constructor(ctor) => {
                    let mkey = format!("{key}.constructor");
                    let sig = self.fn_signature(
                        "constructor",
                        ctor.span,
                        ctor.body.as_ref().map(|b| b.span),
                    );
                    let mut props = self.common_props(sig, ctor.span, "constructor");
                    if self.include_source {
                        self.add_source(&mut props, ctor.span);
                    }
                    let line = self.line(ctor.span);
                    props.insert("end_line".into(), Value::from(self.end_line(ctor.span)));
                    self.push_node(mkey.clone(), "Method", props, line);
                    self.facts
                        .edges
                        .push(edge_at(&key, &mkey, "CONTAINS", line));
                    self.facts
                        .class_methods
                        .push((name.to_string(), "constructor".into()));
                    if let Some(body) = &ctor.body {
                        self.collect_calls(&mkey, body, Some((name, &key)));
                    }
                }
                _ => {}
            }
        }
        self.class_routes(&key, c);
    }

    fn class_method(
        &mut self,
        class_key: &str,
        class_name: &str,
        mname: &str,
        f: &ast::Function,
        accessibility: Option<ast::Accessibility>,
        span: Span,
    ) {
        let mkey = format!("{class_key}.{mname}");
        let sig = self.fn_signature(mname, f.span(), f.body.as_ref().map(|b| b.span));
        let mut props = self.common_props(sig, span, mname);
        if let Some(acc) = accessibility {
            let vis = match acc {
                ast::Accessibility::Public => "public",
                ast::Accessibility::Protected => "protected",
                ast::Accessibility::Private => "private",
            };
            props.insert("visibility".into(), Value::String(vis.into()));
        }
        if f.is_async {
            props.insert("is_async".into(), Value::Bool(true));
        }
        if self.include_source {
            self.add_source(&mut props, span);
        }
        let line = self.line(span);
        self.push_node(mkey.clone(), "Method", props, line);
        self.facts
            .edges
            .push(edge_at(class_key, &mkey, "CONTAINS", line));
        self.facts
            .class_methods
            .push((class_name.to_string(), mname.to_string()));
        if let Some(body) = &f.body {
            self.collect_calls(&mkey, body, Some((class_name, class_key)));
        }
        self.fn_hints(&mkey, f);
    }

    fn interface(&mut self, i: &ast::TsInterfaceDecl, exported: bool, doc_span: Span) {
        let name = i.id.sym.to_string();
        let key = format!("{}.{name}", self.module);
        let mut props = self.common_props(String::new(), doc_span, &name);
        if exported {
            props.insert("visibility".into(), Value::String("exported".into()));
        }

        let mut fields: Vec<Value> = Vec::new();
        for m in &i.body.body {
            match m {
                ast::TsTypeElement::TsPropertySignature(p) => {
                    if let ast::Expr::Ident(id) = &*p.key {
                        let entry = match &p.type_ann {
                            Some(t) => format!("{}: {}", id.sym, self.snippet(t.type_ann.span())),
                            None => id.sym.to_string(),
                        };
                        fields.push(Value::String(entry));
                    }
                }
                ast::TsTypeElement::TsMethodSignature(m) => {
                    let ast::Expr::Ident(id) = &*m.key else {
                        continue;
                    };
                    // Each demanded method is its own node — the Trait and Go
                    // interface convention. No visibility: as public as the
                    // interface.
                    let mkey = format!("{key}.{}", id.sym);
                    let sig = format!("{}{}", id.sym, self.snippet_after(m.span, id.span.hi));
                    let mut mprops = Props::new();
                    mprops.insert("signature".into(), Value::String(sig));
                    if let Some(doc) = self.doc_of(m.span) {
                        mprops.insert("doc_comment".into(), Value::String(doc));
                    }
                    let line = self.line(m.span);
                    mprops.insert("end_line".into(), Value::from(self.end_line(m.span)));
                    self.push_node(mkey.clone(), "Method", mprops, line);
                    self.facts
                        .edges
                        .push(edge_at(&key, &mkey, "HAS_METHOD", line));
                }
                _ => {}
            }
        }
        if !fields.is_empty() {
            props.insert(
                "fields".into(),
                serde_json::json!({
                    "$desc": "the properties it demands, each with its type as written",
                    "$value": Value::Array(fields),
                }),
            );
        }
        let line = self.line(i.id.span);
        props.insert("end_line".into(), Value::from(self.end_line(i.span)));
        // Insert before the method nodes so CONTAINS order reads naturally.
        self.push_contained(key.clone(), "Interface", props, line);
        self.local(&name, &key, false, exported);

        for e in &i.extends {
            if let Some(written) = self.expr_with_args_name(e) {
                self.facts
                    .clauses
                    .push((key.clone(), written, "EXTENDS".into(), line));
            }
        }
    }

    fn ts_enum(&mut self, e: &ast::TsEnumDecl, exported: bool, outer: Option<Span>) {
        let name = e.id.sym.to_string();
        let key = format!("{}.{name}", self.module);
        let mut props = self.common_props(String::new(), outer.unwrap_or(e.span), &name);
        if exported {
            props.insert("visibility".into(), Value::String("exported".into()));
        }
        let variants: Vec<Value> = e
            .members
            .iter()
            .map(|m| {
                let vname = match &m.id {
                    ast::TsEnumMemberId::Ident(i) => i.sym.to_string(),
                    ast::TsEnumMemberId::Str(s) => s.value.to_atom_lossy().to_string(),
                };
                Value::String(match &m.init {
                    Some(init) => format!("{vname} = {}", self.snippet(init.span())),
                    None => vname,
                })
            })
            .collect();
        if !variants.is_empty() {
            props.insert(
                "variants".into(),
                serde_json::json!({
                    "$desc": "the enum's variants, each with its value as written",
                    "$value": Value::Array(variants),
                }),
            );
        }
        let line = self.line(e.id.span);
        props.insert("end_line".into(), Value::from(self.end_line(e.span)));
        self.push_contained(key.clone(), "Enum", props, line);
        self.local(&name, &key, true, exported);
    }

    // ---- collection helpers ----------------------------------------------

    fn collect_calls(&mut self, caller: &str, body: &ast::BlockStmt, class: Option<(&str, &str)>) {
        let mut v = CallCollector {
            walker: self,
            caller: caller.to_string(),
            in_class: class.map(|(n, _)| n.to_string()),
            shapes: std::collections::BTreeMap::new(),
            routed: BTreeSet::new(),
        };
        body.visit_with(&mut v);
    }

    fn collect_calls_expr(&mut self, caller: &str, expr: &ast::Expr, class: Option<(&str, &str)>) {
        let mut v = CallCollector {
            walker: self,
            caller: caller.to_string(),
            in_class: class.map(|(n, _)| n.to_string()),
            shapes: std::collections::BTreeMap::new(),
            routed: BTreeSet::new(),
        };
        expr.visit_with(&mut v);
    }

    fn local(&mut self, name: &str, key: &str, callable: bool, _exported: bool) {
        self.facts.decls.push(LocalDecl {
            name: name.to_string(),
            key: key.to_string(),
            callable,
        });
    }

    fn push_contained(&mut self, key: String, label: &str, props: Props, line: u64) {
        self.push_node(key.clone(), label, props, line);
        self.facts
            .edges
            .push(edge_at(&self.module.clone(), &key, "CONTAINS", line));
    }

    fn push_node(&mut self, key: String, label: &str, mut props: Props, line: u64) {
        props.insert("line".into(), Value::from(line));
        self.facts.nodes.push(Node {
            key,
            label: label.into(),
            extra_labels: Vec::new(),
            props,
        });
    }

    fn common_props(&self, signature: String, doc_span: Span, _name: &str) -> Props {
        let mut props = Props::new();
        if !signature.is_empty() {
            props.insert("signature".into(), Value::String(signature));
        }
        if let Some(doc) = self.doc_of(doc_span) {
            props.insert("doc_comment".into(), Value::String(doc));
        }
        props
    }

    /// `function name(params): ret` as written — the source sliced from the
    /// declaration's start to its body, so nothing is re-printed.
    fn fn_signature(&self, name: &str, span: Span, body: Option<Span>) -> String {
        let hi = body.map(|b| b.lo).unwrap_or(span.hi);
        let head = self
            .cm
            .span_to_snippet(Span::new(span.lo, hi))
            .unwrap_or_default();
        let head = head
            .trim_end()
            .trim_end_matches("=>")
            .trim_end()
            .to_string();
        if head.is_empty() {
            name.to_string()
        } else {
            head
        }
    }

    fn snippet(&self, span: Span) -> String {
        self.cm.span_to_snippet(span).unwrap_or_default()
    }

    fn snippet_after(&self, span: Span, from: BytePos) -> String {
        self.cm
            .span_to_snippet(Span::new(from, span.hi))
            .unwrap_or_default()
    }

    fn add_source(&self, props: &mut Props, span: Span) {
        props.insert(
            "_code".into(),
            serde_json::json!({
                "$desc": "source as written, for retrieval — not indexed or embedded",
                "$value": Value::String(self.snippet(span)),
            }),
        );
    }

    /// The nearest leading doc comment: a `/** … */` block, or nothing — a
    /// stray `//` line above a declaration is not documentation.
    fn doc_of(&self, span: Span) -> Option<String> {
        let list = self.comments.with_leading(span.lo, |cs| cs.to_vec());
        let doc: &Comment = list
            .iter()
            .rev()
            .find(|c| c.kind == CommentKind::Block && c.text.starts_with('*'))?;
        let text = doc
            .text
            .trim_start_matches('*')
            .lines()
            .map(|l| l.trim().trim_start_matches('*').trim())
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string();
        (!text.is_empty()).then_some(text)
    }

    /// 1-based, like every editor's gutter.
    fn line(&self, span: Span) -> u64 {
        self.cm.lookup_char_pos(span.lo).line as u64
    }

    /// Where a declaration stops. With [`Walker::line`] it says how big a
    /// thing is without opening it, and lets `snippet` read exactly the
    /// declaration instead of guessing a fixed number of lines after its
    /// first.
    fn end_line(&self, span: Span) -> u64 {
        self.cm.lookup_char_pos(span.hi).line as u64
    }

    fn clause_name(&self, e: &ast::Expr) -> Option<String> {
        match e {
            ast::Expr::Ident(i) => Some(i.sym.to_string()),
            ast::Expr::Member(m) => {
                let ast::Expr::Ident(obj) = &*m.obj else {
                    return None;
                };
                let prop = m.prop.as_ident()?;
                Some(format!("{}.{}", obj.sym, prop.sym))
            }
            _ => None,
        }
    }

    fn expr_with_args_name(&self, e: &ast::TsExprWithTypeArgs) -> Option<String> {
        self.clause_name(&e.expr)
    }
}

fn export_name(n: &ast::ModuleExportName) -> String {
    match n {
        ast::ModuleExportName::Ident(i) => i.sym.to_string(),
        ast::ModuleExportName::Str(s) => s.value.to_atom_lossy().to_string(),
    }
}

fn export_name_of_export(n: &ast::ModuleExportName) -> String {
    export_name(n)
}

/// `require('spec')` or `require('spec').member` — the CommonJS import.
fn require_spec(e: &ast::Expr) -> Option<(String, Option<String>)> {
    match e {
        ast::Expr::Call(call) => {
            let ast::Callee::Expr(callee) = &call.callee else {
                return None;
            };
            let ast::Expr::Ident(i) = &**callee else {
                return None;
            };
            if i.sym != *"require" {
                return None;
            }
            let arg = call.args.first()?;
            let ast::Expr::Lit(ast::Lit::Str(s)) = &*arg.expr else {
                return None;
            };
            Some((s.value.to_atom_lossy().to_string(), None))
        }
        ast::Expr::Member(m) => {
            let (spec, None) = require_spec(&m.obj)? else {
                return None;
            };
            let prop = m.prop.as_ident()?;
            Some((spec, Some(prop.sym.to_string())))
        }
        _ => None,
    }
}

fn is_module_exports(m: &ast::MemberExpr) -> bool {
    matches!(&*m.obj, ast::Expr::Ident(i) if i.sym == *"module")
        && m.prop.as_ident().is_some_and(|p| p.sym == *"exports")
}

fn prop_name_of_key(key: &ast::PropName) -> Option<String> {
    prop_name(key)
}

fn prop_name(key: &ast::PropName) -> Option<String> {
    match key {
        ast::PropName::Ident(i) => Some(i.sym.to_string()),
        ast::PropName::Str(s) => Some(s.value.to_atom_lossy().to_string()),
        _ => None,
    }
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

/// Records every call a body makes. A bare name, one `qualifier.name`
/// selector, `this.name`, or `new Name` is held for resolution; a rendered
/// JSX component is a function called; anything deeper is counted opaque.
struct CallCollector<'a, 'b> {
    walker: &'a mut Walker<'b>,
    caller: String,
    in_class: Option<String>,
    /// The shape recorded for a call, by where its span starts.
    ///
    /// The visit gives no parents, and the concurrency of a call is entirely
    /// a fact about its parent: `await f()`, `Promise.all([f()])` and a bare
    /// `f()` differ nowhere inside the `f()`. Each parent stamps its
    /// children on the way down, and the call reads its own stamp when the
    /// walk arrives.
    shapes: std::collections::BTreeMap<u32, Shape>,
    /// The call spans a route chain already read, by where each starts.
    ///
    /// A chain is read whole at its outermost call, because only together do
    /// `router.route('/x')` and `.get(h)` state one route; the walk reaches
    /// each inner link again on its own, and this is what stops it being
    /// read twice.
    routed: BTreeSet<u32>,
}

/// Whether a callee starts what it is handed beside its caller.
///
/// `all`/`race` are matched only under `Promise`, because a method called
/// `all` on anything else schedules nothing.
/// The path an expression states, if it states one: a string literal, or a
/// template whose holes are left as `${}` for the convention to reduce.
///
/// A template is the common way a path carries an id — `` `/orders/${id}` ``
/// — and keeping the hole rather than dropping it is what lets the result
/// compare equal to a declaration's `/orders/{id}`.
fn path_text(expr: &ast::Expr) -> Option<String> {
    match expr {
        ast::Expr::Lit(ast::Lit::Str(s)) => {
            let text = s.value.to_atom_lossy().to_string();
            route::looks_like_path(&text).then_some(text)
        }
        ast::Expr::Tpl(tpl) => {
            let mut out = String::new();
            for (i, quasi) in tpl.quasis.iter().enumerate() {
                out.push_str(&quasi.raw);
                if i < tpl.exprs.len() {
                    out.push_str("${}");
                }
            }
            route::looks_like_path(&out).then_some(out)
        }
        ast::Expr::Paren(p) => path_text(&p.expr),
        ast::Expr::TsAs(a) => path_text(&a.expr),
        _ => None,
    }
}

fn schedules(callee: &ast::Expr) -> bool {
    match callee {
        ast::Expr::Ident(i) => matches!(
            &*i.sym,
            "setTimeout" | "setInterval" | "setImmediate" | "queueMicrotask"
        ),
        ast::Expr::Member(m) => {
            let Some(prop) = m.prop.as_ident().map(|p| p.sym.to_string()) else {
                return false;
            };
            match &*m.obj {
                ast::Expr::Ident(o) if o.sym == *"Promise" => {
                    matches!(&*prop, "all" | "allSettled" | "race" | "any")
                }
                ast::Expr::Ident(o) if o.sym == *"process" => prop == "nextTick",
                _ => false,
            }
        }
        _ => false,
    }
}

impl CallCollector<'_, '_> {
    /// Stamp a call expression, looking through the wrappers that carry one
    /// without changing what it is: an array of promises handed to
    /// `Promise.all`, a `void f()` that floats one deliberately.
    fn stamp(&mut self, e: &ast::Expr, shape: Shape) {
        match e {
            ast::Expr::Call(c) => {
                self.shapes.insert(c.span.lo.0, shape);
            }
            ast::Expr::Array(a) => {
                for item in a.elems.iter().flatten() {
                    self.stamp(&item.expr, shape);
                }
            }
            ast::Expr::Unary(u) if u.op == ast::UnaryOp::Void => self.stamp(&u.arg, shape),
            ast::Expr::Paren(p) => self.stamp(&p.expr, shape),
            _ => {}
        }
    }

    fn push(&mut self, kind: CallKind, name: String, span: Span) {
        let line = self.walker.line(span);
        let shape = self.shapes.remove(&span.lo.0);
        self.walker.facts.calls.push(Call {
            shape,
            caller: self.caller.clone(),
            kind,
            name,
            line,
        });
    }

    /// The routes the chain this call belongs to states, read once at its
    /// outermost link.
    fn route_ref(&mut self, node: &ast::CallExpr) {
        if !self.routed.insert(node.span.lo.0) {
            return;
        }
        let caller = self.caller.clone();
        let consumed = self.walker.route_chain(&caller, node);
        self.routed.extend(consumed);
    }
}

/// Every string literal an expression holds directly: the argument itself, or
/// the elements of an array of them — `@Get(['a', 'b'])` declares two.
fn str_args(expr: &ast::Expr) -> Vec<String> {
    match expr {
        ast::Expr::Lit(ast::Lit::Str(s)) => vec![s.value.to_atom_lossy().to_string()],
        ast::Expr::Array(a) => a
            .elems
            .iter()
            .flatten()
            .flat_map(|e| str_args(&e.expr))
            .collect(),
        ast::Expr::Paren(p) => str_args(&p.expr),
        ast::Expr::TsAs(a) => str_args(&a.expr),
        _ => Vec::new(),
    }
}

/// A decorator's own name: `@Get(…)`, `@Nest.Get(…)`, `@Get`.
fn decorator_name(expr: &ast::Expr) -> Option<String> {
    match expr {
        ast::Expr::Call(c) => match &c.callee {
            ast::Callee::Expr(e) => decorator_name(e),
            _ => None,
        },
        ast::Expr::Ident(i) => Some(i.sym.to_string()),
        ast::Expr::Member(m) => m.prop.as_ident().map(|p| p.sym.to_string()),
        _ => None,
    }
}

/// What one decorator states: the verb its name carries, and the paths its
/// string arguments hold.
///
/// A decorator that names a verb is a route's, so its string argument is a
/// path whether or not it was written with a leading slash — `@Get(':id')` is
/// how Nest writes one. `lenient` says the same of a decorator that names no
/// verb, which only a class's mount decorator is allowed. Otherwise a literal
/// must be path-shaped to state anything, and `@Roles('admin')` states
/// nothing.
fn decorator_route(expr: &ast::Expr, lenient: bool) -> (Option<route::Verb>, Vec<String>) {
    let verb = decorator_name(expr)
        .as_deref()
        .and_then(route::Verb::from_name);
    let ast::Expr::Call(call) = expr else {
        return (verb, Vec::new());
    };
    let loose = lenient || verb.is_some();
    let paths = call
        .args
        .iter()
        .flat_map(|a| str_args(&a.expr))
        .filter(|t| {
            route::looks_like_path(t)
                || (loose && !t.is_empty() && !t.contains(char::is_whitespace))
        })
        .collect();
    (verb, paths)
}

/// Is this the decorator a class is mounted by? The one name read rather
/// than inferred, because a mount path is the only thing a framework writes
/// with no slash and no verb to recognise it by: `@Controller('cats')`.
fn mounts(expr: &ast::Expr) -> bool {
    decorator_name(expr).is_some_and(|n| n.to_ascii_lowercase().ends_with("controller"))
}

/// What one method's decorators declare, before the class prefix applies.
#[derive(Default)]
struct Decorated {
    verbs: Vec<route::Verb>,
    paths: BTreeSet<String>,
    line: u64,
}

/// What one call states about a route: the verb its callee names, the path an
/// argument holds, and the handler it hands that path to.
struct Stated {
    verb: Option<route::Verb>,
    written: Option<String>,
    handler: Option<Handler>,
}

impl Walker<'_> {
    /// Every route a call chain states, read at its outermost call.
    ///
    /// A chain may split the path from the verb: `router.route('/x')` states
    /// the path and does nothing with it, and each `.get(h)` after it states
    /// a verb and a handler and no path. A link that states a path but
    /// neither a verb nor a handler has handed it onward, which is exactly
    /// what tells that chain from `axios.get('/x').then(cb)` — where the
    /// first link is the request and `.then` is nobody's registration.
    ///
    /// Returns the spans of the inner links, so a walk that reaches them
    /// again does not read them twice.
    fn route_chain(&mut self, caller: &str, node: &ast::CallExpr) -> Vec<u32> {
        let mut links = vec![node];
        loop {
            let last = links[links.len() - 1];
            let ast::Callee::Expr(callee) = &last.callee else {
                break;
            };
            let ast::Expr::Member(m) = &**callee else {
                break;
            };
            let ast::Expr::Call(inner) = &*m.obj else {
                break;
            };
            links.push(inner);
        }
        let consumed: Vec<u32> = links[1..].iter().map(|c| c.span.lo.0).collect();
        links.reverse();

        let mut base: Option<String> = None;
        let count = links.len();
        for (i, link) in links.iter().enumerate() {
            let stated = self.stated(link);
            let outermost = i + 1 == count;
            if !outermost
                && stated.written.is_some()
                && stated.verb.is_none()
                && stated.handler.is_none()
            {
                base = stated.written;
                continue;
            }
            let Some(written) = stated.written.or_else(|| base.clone()) else {
                continue;
            };
            let line = self.line(link.span);
            self.facts.routes.push(RouteSite {
                route: route::RouteRef {
                    symbol: caller.to_string(),
                    verb: stated.verb,
                    written,
                    line,
                },
                handler: stated.handler,
            });
        }
        consumed
    }

    /// What one call states, read from its callee's name and its arguments.
    ///
    /// Recognising the *argument* rather than a list of blessed frameworks is
    /// what makes a project-local wrapper visible: `request('/bridge/check')`
    /// names no library this reader could have been taught. The cost is that
    /// a router push or a file path can slip through; it will match nothing
    /// on the other side, and the account says how many did.
    fn stated(&self, node: &ast::CallExpr) -> Stated {
        let ast::Callee::Expr(callee) = &node.callee else {
            return Stated {
                verb: None,
                written: None,
                handler: None,
            };
        };
        // `axios.get(…)`, `app.post(…)`, `fetch(…)`: the verb is in the name
        // when the framework puts it there.
        let mut verb = match &**callee {
            ast::Expr::Member(m) => m
                .prop
                .as_ident()
                .and_then(|p| route::Verb::from_name(&p.sym)),
            ast::Expr::Ident(i) => route::Verb::from_name(&i.sym),
            _ => None,
        };
        let mut written = None;
        // The last handler-ish argument wins: Express reads `get(p, auth, h)`
        // as middleware then handler, and `h` is what serves the path.
        let mut handler = None;
        for arg in &node.args {
            match &*arg.expr {
                ast::Expr::Arrow(_) | ast::Expr::Fn(_) => handler = Some(Handler::InPlace),
                ast::Expr::Ident(i) => handler = Some(Handler::Named(i.sym.to_string())),
                // A config object states both: `{ url: '/x', method: 'post' }`.
                ast::Expr::Object(obj) => {
                    for prop in &obj.props {
                        let ast::PropOrSpread::Prop(p) = prop else {
                            continue;
                        };
                        let ast::Prop::KeyValue(kv) = &**p else {
                            continue;
                        };
                        let Some(name) = prop_name(&kv.key) else {
                            continue;
                        };
                        match name.as_str() {
                            "url" | "uri" => {
                                written = written.or_else(|| path_text(&kv.value));
                            }
                            "method" => {
                                if let ast::Expr::Lit(ast::Lit::Str(s)) = &*kv.value {
                                    let m = s.value.to_atom_lossy().to_string();
                                    verb = verb.or_else(|| route::Verb::from_name(&m));
                                }
                            }
                            _ => {}
                        }
                    }
                }
                other => written = written.or_else(|| path_text(other)),
            }
        }
        Stated {
            verb,
            written,
            handler,
        }
    }

    /// The routes a decorated class declares: the prefix its own decorator
    /// carries, and the verb and path on each method's.
    ///
    /// Read from the arguments the way every other shape is, so Nest's
    /// `@Controller('cats')` + `@Get(':id')`, a JAX-RS-style `@Path('/cats')`
    /// and anything else written this way are one rule rather than three
    /// tables. The verbs and paths of one method are crossed, never taken a
    /// decorator at a time: `@Get()` and `@Path(':id')` on one method are one
    /// route, and reading them apart would claim two.
    fn class_routes(&mut self, class_key: &str, c: &ast::Class) {
        let prefix = c
            .decorators
            .iter()
            .find_map(|d| {
                let (_, paths) = decorator_route(&d.expr, mounts(&d.expr));
                paths.into_iter().next()
            })
            .unwrap_or_default();
        for m in &c.body {
            let ast::ClassMember::Method(method) = m else {
                continue;
            };
            let Some(mname) = prop_name(&method.key) else {
                continue;
            };
            let mut d = Decorated::default();
            for dec in &method.function.decorators {
                let (verb, paths) = decorator_route(&dec.expr, false);
                if verb.is_none() && paths.is_empty() {
                    continue;
                }
                if let Some(v) = verb
                    && !d.verbs.contains(&v)
                {
                    d.verbs.push(v);
                }
                d.paths.extend(paths);
                let line = self.line(dec.span);
                d.line = if d.line == 0 { line } else { d.line.min(line) };
            }
            if d.verbs.is_empty() && d.paths.is_empty() {
                continue;
            }
            // No path of its own means the class's: `@Get()` under
            // `@Controller('cats')` serves `/cats`.
            let own: Vec<&str> = if d.paths.is_empty() {
                vec![""]
            } else {
                d.paths.iter().map(String::as_str).collect()
            };
            let verbs: Vec<Option<route::Verb>> = if d.verbs.is_empty() {
                vec![None]
            } else {
                d.verbs.iter().copied().map(Some).collect()
            };
            let symbol = format!("{class_key}.{mname}");
            for path in own {
                let written = if path.is_empty() {
                    prefix.clone()
                } else {
                    route::join(&prefix, path)
                };
                // Neither the method nor its class states a path: nothing is
                // known, and a guess would be worse than the silence.
                if written.is_empty() {
                    continue;
                }
                for verb in &verbs {
                    self.facts.routes.push(RouteSite {
                        route: route::RouteRef {
                            symbol: symbol.clone(),
                            verb: *verb,
                            written: written.clone(),
                            line: d.line,
                        },
                        // A decorator hands the path to the method it is
                        // written on, and that method is the handler.
                        handler: Some(Handler::InPlace),
                    });
                }
            }
        }
    }

    /// Routes registered in the module's own body — `app.get('/orders', h)`
    /// at the top level, which is where a small Express or Koa app writes
    /// them.
    ///
    /// A narrow walk on purpose: expression statements only, and no descent
    /// into a handler, whose body `collect_calls` already covers. The module
    /// is the symbol such a route belongs to when its handler is written in
    /// place and has no name of its own.
    fn top_level_routes(&mut self, items: &[ast::ModuleItem]) {
        for item in items {
            if let ast::ModuleItem::Stmt(stmt) = item {
                self.top_level_route(stmt);
            }
        }
    }

    /// One top-level statement's route chain, when it is a bare call.
    ///
    /// A file with neither an import nor an export parses as a script rather
    /// than a module, and a single-file Express app is exactly that — so both
    /// program shapes come through here.
    fn top_level_route(&mut self, stmt: &ast::Stmt) {
        let ast::Stmt::Expr(e) = stmt else {
            return;
        };
        let mut expr = &*e.expr;
        while let ast::Expr::Await(a) = expr {
            expr = &a.arg;
        }
        if let ast::Expr::Call(c) = expr {
            let module = self.module.clone();
            self.route_chain(&module, c);
        }
    }
}

impl CallCollector<'_, '_> {
    fn arg_refs(&mut self, args: &[ast::ExprOrSpread], line_span: Span, scheduled: bool) {
        for a in args {
            if a.spread.is_none()
                && let ast::Expr::Ident(i) = &*a.expr
            {
                let line = self.walker.line(line_span);
                let caller = self.caller.clone();
                self.walker
                    .facts
                    .fn_refs
                    .push((caller, line, i.sym.to_string(), scheduled));
            }
        }
    }

    fn callback_uses(&mut self, callee: &ast::Expr, args: &[ast::ExprOrSpread]) {
        let ast::Expr::Ident(f) = callee else { return };
        for (i, a) in args.iter().enumerate() {
            if a.spread.is_none()
                && let Some(params) = Walker::callback_params(&a.expr)
            {
                let caller = self.caller.clone();
                self.walker
                    .facts
                    .callback_uses
                    .push((caller, f.sym.to_string(), i, params));
            }
        }
    }
}

impl Visit for CallCollector<'_, '_> {
    /// A call standing alone as a statement throws its result away. For a
    /// promise that means nothing here awaits it — the `await` that would
    /// have is simply absent. Stamped for every callee and kept at assemble
    /// only for the ones that are async, because `console.log(x)` is a
    /// statement too and there is nothing there to await.
    fn visit_expr_stmt(&mut self, node: &ast::ExprStmt) {
        self.stamp(&node.expr, Shape::Unawaited);
        node.visit_children_with(self);
    }

    /// `await f()` waits for the promise here, so it is the ordinary case
    /// and clears any `unawaited` the statement above stamped.
    fn visit_await_expr(&mut self, node: &ast::AwaitExpr) {
        if let ast::Expr::Call(c) = &*node.arg {
            self.shapes.remove(&c.span.lo.0);
        }
        node.visit_children_with(self);
    }

    fn visit_call_expr(&mut self, node: &ast::CallExpr) {
        // A scheduler starts everything it is handed beside its caller.
        if let ast::Callee::Expr(callee) = &node.callee
            && schedules(callee)
        {
            let args: Vec<ast::ExprOrSpread> = node.args.clone();
            for a in &args {
                self.stamp(&a.expr, Shape::Scheduled);
            }
        }
        self.route_ref(node);
        if let ast::Callee::Expr(expr) = &node.callee {
            match &**expr {
                ast::Expr::Ident(i) => {
                    // A lazy `require('./x')` inside a body is still an
                    // import; anything else named `require` is noise either
                    // way.
                    if i.sym == *"require" {
                        if let Some((spec, _)) = require_spec(&ast::Expr::Call(node.clone())) {
                            let line = self.walker.line(node.span);
                            self.walker.note_import(&spec, line);
                        }
                    } else {
                        self.push(CallKind::Plain, i.sym.to_string(), node.span);
                    }
                }
                ast::Expr::Member(m) => {
                    let prop = m.prop.as_ident().map(|p| p.sym.to_string());
                    match (&*m.obj, prop) {
                        (ast::Expr::Ident(obj), Some(name)) => {
                            self.push(CallKind::Qualified(obj.sym.to_string()), name, node.span);
                        }
                        (ast::Expr::This(_), Some(name)) => {
                            if self.in_class.is_some() {
                                self.push(
                                    CallKind::This(self.in_class.clone().unwrap()),
                                    name,
                                    node.span,
                                );
                            } else {
                                self.walker.facts.opaque += 1;
                            }
                        }
                        // One field hop: `this.attr.m()` when the class
                        // states `attr`, `o.field.m()` when both hops are
                        // stated. Deeper chains stay opaque.
                        (ast::Expr::Member(inner), Some(name)) => {
                            let field = inner.prop.as_ident().map(|p| p.sym.to_string());
                            match (&*inner.obj, field, &self.in_class) {
                                (ast::Expr::This(_), Some(attr), Some(class)) => self.push(
                                    CallKind::SelfAttr {
                                        class: class.clone(),
                                        attr,
                                    },
                                    name,
                                    node.span,
                                ),
                                (ast::Expr::Ident(obj), Some(field), _) => self.push(
                                    CallKind::FieldChain {
                                        obj: obj.sym.to_string(),
                                        field,
                                    },
                                    name,
                                    node.span,
                                ),
                                _ => self.walker.facts.opaque += 1,
                            }
                        }
                        _ => self.walker.facts.opaque += 1,
                    }
                }
                // `super.m()` — the base chain's method, never the own class.
                ast::Expr::SuperProp(sp) => {
                    if let (ast::SuperProp::Ident(i), Some(class)) = (&sp.prop, &self.in_class) {
                        self.push(CallKind::Super(class.clone()), i.sym.to_string(), node.span);
                    } else {
                        self.walker.facts.opaque += 1;
                    }
                }
                _ => self.walker.facts.opaque += 1,
            }
        }
        self.arg_refs(
            &node.args,
            node.span,
            matches!(&node.callee, ast::Callee::Expr(c) if schedules(c)),
        );
        if let ast::Callee::Expr(callee) = &node.callee {
            self.callback_uses(callee, &node.args);
        }
        node.visit_children_with(self);
    }

    fn visit_new_expr(&mut self, node: &ast::NewExpr) {
        if let ast::Expr::Ident(i) = &*node.callee {
            self.push(CallKind::New, i.sym.to_string(), node.span);
        } else {
            self.walker.facts.opaque += 1;
        }
        node.visit_children_with(self);
    }

    fn visit_jsx_opening_element(&mut self, node: &ast::JSXOpeningElement) {
        // `<Foo />` renders by calling `Foo`; a lowercase name is an HTML
        // tag, not a component.
        if let ast::JSXElementName::Ident(i) = &node.name
            && i.sym.chars().next().is_some_and(char::is_uppercase)
        {
            self.push(CallKind::Plain, i.sym.to_string(), node.span);
        }
        node.visit_children_with(self);
    }

    // A nested function or class declares its own scope but its calls still
    // belong to the declaration a reader would look at — the enclosing one.
}

#[cfg(test)]
mod tests;
