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

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
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
    /// `(caller, bare name, line)` — functions passed as call arguments.
    pub fn_refs: Vec<(String, u64, String)>,
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

fn syntax_for(path: &str) -> Syntax {
    match path.rsplit('.').next().unwrap_or("") {
        "ts" | "mts" | "cts" => Syntax::Typescript(TsSyntax::default()),
        "tsx" => Syntax::Typescript(TsSyntax {
            tsx: true,
            ..Default::default()
        }),
        "jsx" => Syntax::Es(EsSyntax {
            jsx: true,
            ..Default::default()
        }),
        _ => Syntax::Es(EsSyntax::default()),
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
    facts
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
                && let Some(written) = Self::type_written(&ann.type_ann)
            {
                self.hint(caller, b.id.sym.to_string(), written, false);
            }
        }
        if let Some(ret) = &f.return_type
            && let Some(written) = Self::type_written(&ret.type_ann)
        {
            self.facts.returns.push((caller.to_string(), written));
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
        self.push_contained(key.clone(), "Enum", props, line);
        self.local(&name, &key, true, exported);
    }

    // ---- collection helpers ----------------------------------------------

    fn collect_calls(&mut self, caller: &str, body: &ast::BlockStmt, class: Option<(&str, &str)>) {
        let mut v = CallCollector {
            walker: self,
            caller: caller.to_string(),
            in_class: class.map(|(n, _)| n.to_string()),
        };
        body.visit_with(&mut v);
    }

    fn collect_calls_expr(&mut self, caller: &str, expr: &ast::Expr, class: Option<(&str, &str)>) {
        let mut v = CallCollector {
            walker: self,
            caller: caller.to_string(),
            in_class: class.map(|(n, _)| n.to_string()),
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
}

impl CallCollector<'_, '_> {
    fn push(&mut self, kind: CallKind, name: String, span: Span) {
        let line = self.walker.line(span);
        self.walker.facts.calls.push(Call {
            caller: self.caller.clone(),
            kind,
            name,
            line,
        });
    }
}

impl CallCollector<'_, '_> {
    fn arg_refs(&mut self, args: &[ast::ExprOrSpread], line_span: Span) {
        for a in args {
            if a.spread.is_none()
                && let ast::Expr::Ident(i) = &*a.expr
            {
                let line = self.walker.line(line_span);
                let caller = self.caller.clone();
                self.walker
                    .facts
                    .fn_refs
                    .push((caller, line, i.sym.to_string()));
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
    fn visit_call_expr(&mut self, node: &ast::CallExpr) {
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
        self.arg_refs(&node.args, node.span);
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
