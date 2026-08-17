//! Python into facts — nodes and edges a parser is certain of, leaving
//! nothing for a model to guess at.
//!
//! Same discipline as the Rust, Go and TS parsers beside it: parse each
//! file alone (so chunks run concurrently in instances that share nothing),
//! resolve across files once in [`assemble`], and count whatever could not
//! be resolved in the notes rather than dropping it silently. Parse-only —
//! ruff's parser, no inference, which is the point: what a checker would
//! have to infer is exactly what this refuses to guess.
//!
//! Keys are Python's own dotted names. A module's identity comes from the
//! language's rule for it: walking up from the file, every directory with an
//! `__init__.py` is a package, and the first without one is the `sys.path`
//! root — `src/mypkg/core/utils.py` is the module `mypkg.core.utils`, and
//! `__init__.py` names its directory the way `mod.rs` names its parent.
//! `mypkg.core.utils.parse_row` is a declaration;
//! `mypkg.core.utils.Config.load` is a method.

mod resolve;

pub use resolve::{Assembled, assemble};

use ruff_python_ast as ast;
use ruff_source_file::LineIndex;
use ruff_text_size::{Ranged, TextRange};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// What the parser reads through — the plugin contract's host, as one small
/// trait so tests can hand in a plain map.
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

/// One call site, held until every file is known.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Call {
    pub caller: String,
    pub kind: CallKind,
    pub line: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallKind {
    /// `foo()` — a bare name: a local declaration or an imported binding.
    Plain(String),
    /// `a.b.c()` — a dotted chain rooted at a bare name, which only an
    /// import can make resolvable.
    Chain(Vec<String>),
    /// `self.m()` / `cls.m()` inside a class body — lexical, so resolvable.
    This { class: String, method: String },
    /// `super().m()` inside a class body — resolved over the bases only.
    Super { class: String, method: String },
    /// `self.attr.m()` — resolvable when the class states `attr`'s class
    /// (an annotation, or what `__init__` assigns).
    SelfAttr {
        class: String,
        attr: String,
        method: String,
    },
}

/// One imported binding. `target` is the module it reaches, already dotted
/// and absolute — relative imports are resolved at parse, where the current
/// module is known.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportBinding {
    /// The name bound in this file.
    pub local: String,
    /// The absolute dotted module the binding reaches.
    pub target: String,
    /// The name imported from it — empty when the binding is the module
    /// itself (`import a.b as m`).
    pub member: String,
}

/// A local declaration, for call resolution. `value` marks a plain
/// assignment — which yields to an import binding of the same name, because
/// `try: from x import y / except: y = None` is the fallback idiom, and the
/// import is the primary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalDecl {
    pub name: String,
    pub key: String,
    #[serde(default)]
    pub value: bool,
}

/// How a name is bound to a class the source states — an annotation, or a
/// constructor/factory call the file can name. `caller` is a function key
/// for params and locals, a CLASS key for instance attributes (`self.name`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hint {
    pub caller: String,
    pub name: String,
    /// The annotation or callee, as written (possibly dotted).
    pub written: String,
    /// True when `written` was called (`Foo()` / `make()`): the name may be
    /// a class (constructor) or a factory whose return annotation types it.
    pub constructed: bool,
}

/// Everything one file contributes — the opaque partial the component
/// serializes between the two phases.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileFacts {
    pub file: String,
    pub module_id: String,
    pub failed: bool,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub calls: Vec<Call>,
    /// `(caller, dotted string literal, line)` — strings shaped like
    /// qualified names (`"pkg.mod.fn"`), resolved at assemble into
    /// REFERENCES edges when they name a known symbol. mock.patch targets
    /// are the canonical case.
    pub string_refs: Vec<(String, String, u64)>,
    pub bindings: Vec<ImportBinding>,
    /// Import statements as written (absolute dotted targets), with lines.
    pub imports: Vec<(String, u64)>,
    /// `from mod import *` targets, in import order.
    pub stars: Vec<String>,
    pub decls: Vec<LocalDecl>,
    /// `Class → method names`, for `self.m()` resolution.
    pub class_methods: Vec<(String, String)>,
    /// `class C(Base)`: (class key, base as written — possibly dotted, line).
    pub bases: Vec<(String, String, u64)>,
    /// `__all__`, when the module declares one — the written export list.
    pub dunder_all: Option<Vec<String>>,
    /// Names exported by Python's own star-import rule (in `__all__`, or not
    /// underscore-prefixed).
    pub exports: Vec<LocalDecl>,
    /// Call sites too dynamic to name at all.
    pub opaque: usize,
    /// The classes this file declares, by key — what receiver typing may
    /// resolve a written type to.
    pub classes: Vec<String>,
    /// Type bindings the source states (annotations, constructor results).
    pub hints: Vec<Hint>,
    /// `(caller, bare name, line)` — functions (or classes: Python's
    /// class-as-value idiom) passed as call arguments.
    pub fn_refs: Vec<(String, String, u64)>,
    /// `(function key, return annotation as written)` when it is a plain
    /// dotted name — what types `x = make()`.
    pub returns: Vec<(String, String)>,
}

/// Parse one chunk of paths into per-file facts.
pub fn parse_chunk(files: &dyn Files, paths: &[String], include_source: bool) -> Vec<FileFacts> {
    let mut packages = PackageTable::new(files);
    paths
        .iter()
        .map(|p| match files.read(p) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => parse_file(p, &packages.module_for(p), &text, include_source),
                Err(_) => failed(p),
            },
            Err(_) => failed(p),
        })
        .collect()
}

/// Parse one pushed document. With no tree around it, the file's stem is its
/// module.
pub fn parse_document(name: &str, bytes: &[u8], include_source: bool) -> Vec<FileFacts> {
    let Ok(text) = String::from_utf8(bytes.to_vec()) else {
        return vec![failed(name)];
    };
    vec![parse_file(name, &stem(name), &text, include_source)]
}

fn failed(path: &str) -> FileFacts {
    FileFacts {
        file: path.to_string(),
        failed: true,
        ..Default::default()
    }
}

/// Resolves a file to its dotted module id by the language's own rule:
/// every ancestor directory holding an `__init__.py` is a package, and the
/// first without one is the `sys.path` root. Probed through the host,
/// memoized per directory.
struct PackageTable<'a> {
    files: &'a dyn Files,
    /// dir → dotted package prefix ("" at a root).
    cache: BTreeMap<String, String>,
}

impl<'a> PackageTable<'a> {
    fn new(files: &'a dyn Files) -> Self {
        Self {
            files,
            cache: BTreeMap::new(),
        }
    }

    fn module_for(&mut self, path: &str) -> String {
        let dir = parent_dir(path);
        let prefix = self.package_prefix(&dir);
        let base = stem(path);
        if base == "__init__" {
            // `__init__.py` names its directory the way `mod.rs` names its
            // parent; at a root it has no name worth keeping.
            if prefix.is_empty() {
                self.files
                    .label()
                    .filter(|l| !l.is_empty())
                    .unwrap_or_else(|| "module".into())
            } else {
                prefix
            }
        } else if prefix.is_empty() {
            base
        } else {
            format!("{prefix}.{base}")
        }
    }

    fn package_prefix(&mut self, dir: &str) -> String {
        if let Some(hit) = self.cache.get(dir) {
            return hit.clone();
        }
        let marker = if dir.is_empty() {
            "__init__.py".to_string()
        } else {
            format!("{dir}/__init__.py")
        };
        let result = if self.files.read(&marker).is_err() {
            // No marker: this directory is a sys.path root.
            String::new()
        } else {
            let name = dir.rsplit('/').next().unwrap_or(dir).to_string();
            let above = if dir.is_empty() {
                String::new()
            } else {
                self.package_prefix(&parent_dir_of_dir(dir))
            };
            if above.is_empty() {
                name
            } else {
                format!("{above}.{name}")
            }
        };
        self.cache.insert(dir.to_string(), result.clone());
        result
    }
}

fn parent_dir(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, _)) => dir.to_string(),
        None => String::new(),
    }
}

fn parent_dir_of_dir(dir: &str) -> String {
    match dir.rsplit_once('/') {
        Some((up, _)) => up.to_string(),
        None => String::new(),
    }
}

fn stem(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    base.strip_suffix(".py")
        .or_else(|| base.strip_suffix(".pyi"))
        .or_else(|| base.strip_suffix(".pyw"))
        .unwrap_or(base)
        .to_string()
}

/// The extensions this plugin claims.
pub const EXTENSIONS: &[&str] = &["py", "pyi", "pyw"];

/// Is this name a constant by the language's own stated convention (PEP 8:
/// constants are ALL_CAPS)?
fn is_const_name(name: &str) -> bool {
    name.chars().any(|c| c.is_ascii_uppercase()) && !name.chars().any(|c| c.is_ascii_lowercase())
}

fn parse_file(path: &str, module: &str, text: &str, include_source: bool) -> FileFacts {
    let Ok(parsed) = ruff_python_parser::parse_module(text) else {
        return failed(path);
    };
    let lines = LineIndex::from_source_text(text);

    let mut facts = FileFacts {
        file: path.to_string(),
        module_id: module.to_string(),
        ..Default::default()
    };

    let mut w = Walker {
        facts: &mut facts,
        src: text,
        lines,
        module: module.to_string(),
        include_source,
    };
    let body = &parsed.syntax().body;
    w.module_doc(body);
    w.stmts(body);

    // Python's own star-import rule is the export list: `__all__` when the
    // module declares one, otherwise every name not underscore-prefixed.
    let all = facts.dunder_all.clone();
    for d in &facts.decls {
        let exported = match &all {
            Some(list) => list.iter().any(|n| n == &d.name),
            None => !d.name.starts_with('_'),
        };
        if exported {
            facts.exports.push(d.clone());
            if let Some(n) = facts.nodes.iter_mut().find(|n| n.key == d.key) {
                n.props
                    .insert("visibility".into(), Value::String("exported".into()));
            }
        }
    }

    // The module node, first, with its imports known. Like the family's
    // file-level modules: `path` says which file, no line of its own.
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
    if let Some(doc) = facts
        .nodes
        .first()
        .and_then(|n| n.props.get("__module_doc"))
        .cloned()
    {
        props.insert("doc_comment".into(), doc);
        facts.nodes.remove(0);
    }
    facts.nodes.insert(
        0,
        Node {
            key: module.to_string(),
            label: "Module".into(),
            extra_labels: Vec::new(),
            props,
        },
    );
    for n in facts.nodes.iter_mut().skip(1) {
        n.props
            .insert("file".into(), Value::String(path.to_string()));
    }
    facts
}

struct Walker<'a> {
    facts: &'a mut FileFacts,
    src: &'a str,
    lines: LineIndex,
    module: String,
    include_source: bool,
}

impl Walker<'_> {
    /// The module docstring, stashed in a marker node the caller lifts onto
    /// the module node it builds afterwards.
    fn module_doc(&mut self, body: &[ast::Stmt]) {
        if let Some(doc) = docstring(body) {
            let mut props = Props::new();
            props.insert("__module_doc".into(), Value::String(doc));
            self.facts.nodes.push(Node {
                key: "__module_doc".into(),
                label: "_".into(),
                extra_labels: Vec::new(),
                props,
            });
        }
    }

    /// Module-level statements — walking into `if`/`try` blocks because
    /// Python's own import-time execution does: `if TYPE_CHECKING:` imports
    /// and `try: import ujson` fallbacks are ordinary structure here.
    fn stmts(&mut self, body: &[ast::Stmt]) {
        for stmt in body {
            self.stmt(stmt);
        }
    }

    fn stmt(&mut self, stmt: &ast::Stmt) {
        match stmt {
            ast::Stmt::FunctionDef(f) => self.function(f, None),
            ast::Stmt::ClassDef(c) => self.class(c, None),
            ast::Stmt::Import(i) => self.import(i),
            ast::Stmt::ImportFrom(i) => self.import_from(i),
            ast::Stmt::Assign(a) => self.assign(a),
            ast::Stmt::AnnAssign(a) => self.ann_assign(a),
            ast::Stmt::TypeAlias(t) => self.type_alias(t),
            ast::Stmt::If(i) => {
                self.stmts(&i.body);
                for clause in &i.elif_else_clauses {
                    self.stmts(&clause.body);
                }
            }
            ast::Stmt::Try(t) => {
                self.stmts(&t.body);
                for h in &t.handlers {
                    let ast::ExceptHandler::ExceptHandler(h) = h;
                    self.stmts(&h.body);
                }
                self.stmts(&t.orelse);
                self.stmts(&t.finalbody);
            }
            _ => {}
        }
    }

    fn import(&mut self, i: &ast::StmtImport) {
        let line = self.line(i.range());
        for alias in &i.names {
            let target = alias.name.id.to_string();
            self.note_import(&target, line);
            match &alias.asname {
                Some(as_name) => self.facts.bindings.push(ImportBinding {
                    local: as_name.id.to_string(),
                    target,
                    member: String::new(),
                }),
                None => {
                    // `import a.b.c` binds `a`; the chain resolver walks the
                    // rest at the call site.
                    let root = target.split('.').next().unwrap_or(&target).to_string();
                    self.facts.bindings.push(ImportBinding {
                        local: root.clone(),
                        target: root,
                        member: String::new(),
                    });
                }
            }
        }
    }

    fn import_from(&mut self, i: &ast::StmtImportFrom) {
        let line = self.line(i.range());
        // A relative import is relative to the package holding this module —
        // known here, so the stored target is already absolute.
        let base = if i.level == 0 {
            i.module
                .as_ref()
                .map(|m| m.id.to_string())
                .unwrap_or_default()
        } else {
            let mut parts: Vec<&str> = self.module.split('.').collect();
            // The module's own name never counts as a package level…
            if self.facts.file.rsplit('/').next() != Some("__init__.py") {
                parts.pop();
            }
            // …and each further level climbs one package.
            for _ in 1..i.level {
                parts.pop();
            }
            let mut base = parts.join(".");
            if let Some(m) = &i.module {
                if !base.is_empty() {
                    base.push('.');
                }
                base.push_str(m.id.as_ref());
            }
            base
        };
        if base.is_empty() {
            return;
        }
        self.note_import(&base, line);
        for alias in &i.names {
            let name = alias.name.id.to_string();
            if name == "*" {
                self.facts.stars.push(base.clone());
                continue;
            }
            let local = alias
                .asname
                .as_ref()
                .map(|a| a.id.to_string())
                .unwrap_or_else(|| name.clone());
            self.facts.bindings.push(ImportBinding {
                local,
                target: base.clone(),
                member: name,
            });
        }
    }

    fn note_import(&mut self, target: &str, line: u64) {
        if !self.facts.imports.iter().any(|(s, _)| s == target) {
            self.facts.imports.push((target.to_string(), line));
        }
    }

    /// A written type usable by receiver typing: a plain (possibly dotted)
    /// identifier — `Foo`, `pkg.mod.Foo`. Subscripts (`Optional[Foo]`),
    /// unions and strings are a checker's business and yield nothing.
    fn dotted_ident(text: &str) -> Option<String> {
        let t = text.trim();
        let ok = !t.is_empty()
            && t.split('.').all(|seg| {
                !seg.is_empty()
                    && seg
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_alphabetic() || c == '_')
                    && seg.chars().all(|c| c.is_alphanumeric() || c == '_')
            });
        ok.then(|| t.to_string())
    }

    /// The callee of an initializer, as written, when it is a plain call —
    /// `Foo(...)` / `pkg.make(...)`.
    fn call_target(&self, e: &ast::Expr) -> Option<String> {
        if let ast::Expr::Call(c) = e {
            return Self::dotted_ident(&self.snippet(c.func.range()));
        }
        None
    }

    /// Walk a body for the locals whose class the source states: annotated
    /// assignments and constructor/factory initializers. First binding wins.
    fn body_hints(&mut self, caller: &str, body: &[ast::Stmt]) {
        fn walk<'a>(out: &mut Vec<&'a ast::Stmt>, stmts: &'a [ast::Stmt]) {
            for s in stmts {
                out.push(s);
                match s {
                    ast::Stmt::If(i) => {
                        walk(out, &i.body);
                        for c in &i.elif_else_clauses {
                            walk(out, &c.body);
                        }
                    }
                    ast::Stmt::For(f) => {
                        walk(out, &f.body);
                        walk(out, &f.orelse);
                    }
                    ast::Stmt::While(w) => {
                        walk(out, &w.body);
                        walk(out, &w.orelse);
                    }
                    ast::Stmt::With(w) => walk(out, &w.body),
                    ast::Stmt::Try(t) => {
                        walk(out, &t.body);
                        walk(out, &t.orelse);
                        walk(out, &t.finalbody);
                    }
                    _ => {}
                }
            }
        }
        let mut flat = Vec::new();
        walk(&mut flat, body);
        let push = |name: String, written: String, constructed: bool, hints: &mut Vec<Hint>| {
            if !hints.iter().any(|h| h.caller == caller && h.name == name) {
                hints.push(Hint {
                    caller: caller.to_string(),
                    name,
                    written,
                    constructed,
                });
            }
        };
        let mut new_hints: Vec<Hint> = Vec::new();
        for s in flat {
            match s {
                ast::Stmt::Assign(a) => {
                    if let [ast::Expr::Name(n)] = a.targets.as_slice()
                        && let Some(target) = self.call_target(&a.value)
                    {
                        push(n.id.to_string(), target, true, &mut new_hints);
                    }
                }
                ast::Stmt::AnnAssign(a) => {
                    if let ast::Expr::Name(n) = &*a.target
                        && let Some(ann) = Self::dotted_ident(&self.snippet(a.annotation.range()))
                    {
                        push(n.id.to_string(), ann, false, &mut new_hints);
                    }
                }
                _ => {}
            }
        }
        self.facts.hints.extend(new_hints);
    }

    fn function(&mut self, f: &ast::StmtFunctionDef, class: Option<(&str, &str)>) {
        let name = f.name.id.to_string();
        let (label, key, parent) = match class {
            Some((class_name, class_key)) => {
                self.facts
                    .class_methods
                    .push((class_name.to_string(), name.clone()));
                (
                    "Method",
                    format!("{class_key}.{name}"),
                    class_key.to_string(),
                )
            }
            None => (
                "Function",
                format!("{}.{name}", self.module),
                self.module.clone(),
            ),
        };

        let mut sig = format!("def {name}{}", self.snippet(f.parameters.range()));
        if let Some(ret) = &f.returns {
            sig.push_str(" -> ");
            sig.push_str(&self.snippet(ret.range()));
        }
        let mut props = Props::new();
        props.insert("signature".into(), Value::String(sig));
        if let Some(doc) = docstring(&f.body) {
            props.insert("doc_comment".into(), Value::String(doc));
        }
        if f.is_async {
            props.insert("is_async".into(), Value::Bool(true));
        }
        if self.include_source {
            self.add_source(&mut props, f.range());
        }
        let line = self.line(f.name.range());
        // Overload stubs (`@overload`, `if TYPE_CHECKING` twins) share the
        // name; the first carries the docs and wins, silently.
        if self.facts.nodes.iter().any(|n| n.key == key) {
            return;
        }
        props.insert("line".into(), Value::from(line));
        self.facts.nodes.push(Node {
            key: key.clone(),
            label: label.into(),
            extra_labels: Vec::new(),
            props,
        });
        self.facts
            .edges
            .push(edge_at(&parent, &key, "CONTAINS", line));
        if class.is_none() {
            self.facts.decls.push(LocalDecl {
                name: name.clone(),
                key: key.clone(),
                value: false,
            });
        }

        // A decorator is a call written down: `@decorator` applies
        // `decorator(f)`, and `@app.route(…)` names the router.
        for dec in &f.decorator_list {
            self.decorator(&key, dec);
        }

        let in_class = class.map(|(n, _)| n.to_string());
        self.collect_calls(&key, &f.body, in_class.as_deref());

        // Receiver-typing inputs: annotated parameters, the declared return
        // (what types `x = make()`), and the body's own stated bindings.
        for param in f.parameters.iter_non_variadic_params() {
            let pname = param.parameter.name.id.to_string();
            if pname == "self" || pname == "cls" {
                continue;
            }
            if let Some(ann) = &param.parameter.annotation
                && let Some(written) = Self::dotted_ident(&self.snippet(ann.range()))
                && !self
                    .facts
                    .hints
                    .iter()
                    .any(|h| h.caller == key && h.name == pname)
            {
                self.facts.hints.push(Hint {
                    caller: key.clone(),
                    name: pname,
                    written,
                    constructed: false,
                });
            }
        }
        if let Some(ret) = &f.returns
            && let Some(written) = Self::dotted_ident(&self.snippet(ret.range()))
        {
            self.facts.returns.push((key.clone(), written));
        }
        self.body_hints(&key, &f.body);
    }

    fn class(&mut self, c: &ast::StmtClassDef, outer: Option<&str>) {
        let name = c.name.id.to_string();
        let key = match outer {
            Some(outer_key) => format!("{outer_key}.{name}"),
            None => format!("{}.{name}", self.module),
        };
        let parent = outer
            .map(str::to_string)
            .unwrap_or_else(|| self.module.clone());

        let mut props = Props::new();
        if let Some(doc) = docstring(&c.body) {
            props.insert("doc_comment".into(), Value::String(doc));
        }

        // Fields as the family writes them: `name: type` in declaration
        // order — class-level annotations first, then what `__init__`
        // assigns onto `self`, both of which are how Python declares them.
        self.facts.classes.push(key.clone());
        let mut fields: Vec<String> = Vec::new();
        let mut seen_fields: std::collections::BTreeSet<String> = Default::default();
        for stmt in &c.body {
            match stmt {
                ast::Stmt::AnnAssign(a) => {
                    if let ast::Expr::Name(n) = &*a.target {
                        let entry = format!("{}: {}", n.id, self.snippet(a.annotation.range()));
                        if seen_fields.insert(n.id.to_string()) {
                            fields.push(entry);
                        }
                        // A class-level annotation states the attribute's
                        // class for every `self.x.m()` in the body.
                        if let Some(written) =
                            Self::dotted_ident(&self.snippet(a.annotation.range()))
                            && !self
                                .facts
                                .hints
                                .iter()
                                .any(|h| h.caller == key && h.name == n.id.as_str())
                        {
                            self.facts.hints.push(Hint {
                                caller: key.clone(),
                                name: n.id.to_string(),
                                written,
                                constructed: false,
                            });
                        }
                    }
                }
                ast::Stmt::Assign(a) => {
                    for t in &a.targets {
                        if let ast::Expr::Name(n) = t
                            && seen_fields.insert(n.id.to_string())
                        {
                            fields.push(n.id.to_string());
                        }
                    }
                }
                ast::Stmt::FunctionDef(f) if f.name.id == "__init__" => {
                    // What `__init__` states about each attribute's class:
                    // an annotation, a constructor call, or an annotated
                    // parameter assigned through (`self.cfg = cfg`).
                    let mut params: std::collections::BTreeMap<String, String> = Default::default();
                    for param in f.parameters.iter_non_variadic_params() {
                        if let Some(a) = &param.parameter.annotation
                            && let Some(w) = Self::dotted_ident(&self.snippet(a.range()))
                        {
                            params.insert(param.parameter.name.id.to_string(), w);
                        }
                    }
                    for s in &f.body {
                        let (target, ann) = match s {
                            ast::Stmt::Assign(a) => (a.targets.first(), None),
                            ast::Stmt::AnnAssign(a) => {
                                (Some(&*a.target), Some(a.annotation.range()))
                            }
                            _ => continue,
                        };
                        if let Some(ast::Expr::Attribute(at)) = target
                            && let ast::Expr::Name(base) = &*at.value
                            && base.id == "self"
                        {
                            if seen_fields.insert(at.attr.id.to_string()) {
                                fields.push(match ann {
                                    Some(r) => format!("{}: {}", at.attr.id, self.snippet(r)),
                                    None => at.attr.id.to_string(),
                                });
                            }
                            let hint = match (ann, s) {
                                (Some(r), _) => {
                                    Self::dotted_ident(&self.snippet(r)).map(|w| (w, false))
                                }
                                (None, ast::Stmt::Assign(a)) => match &*a.value {
                                    ast::Expr::Name(v) => {
                                        params.get(v.id.as_str()).map(|w| (w.clone(), false))
                                    }
                                    _ => self.call_target(&a.value).map(|w| (w, true)),
                                },
                                _ => None,
                            };
                            if let Some((written, constructed)) = hint
                                && !self
                                    .facts
                                    .hints
                                    .iter()
                                    .any(|h| h.caller == key && h.name == at.attr.id.as_str())
                            {
                                self.facts.hints.push(Hint {
                                    caller: key.clone(),
                                    name: at.attr.id.to_string(),
                                    written,
                                    constructed,
                                });
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if !fields.is_empty() {
            props.insert(
                "fields".into(),
                serde_json::json!({
                    "$desc": "the fields it declares, each with its type as written",
                    "$value": fields,
                }),
            );
        }
        if self.include_source {
            self.add_source(&mut props, c.range());
        }
        let line = self.line(c.name.range());
        props.insert("line".into(), Value::from(line));
        self.facts.nodes.push(Node {
            key: key.clone(),
            label: "Class".into(),
            extra_labels: Vec::new(),
            props,
        });
        self.facts
            .edges
            .push(edge_at(&parent, &key, "CONTAINS", line));
        if outer.is_none() {
            self.facts.decls.push(LocalDecl {
                name: name.clone(),
                key: key.clone(),
                value: false,
            });
        }

        // Bases are syntax: `class Config(BaseModel)` extends BaseModel,
        // written down. A subscripted base extends what it subscripts.
        if let Some(args) = &c.arguments {
            for base in &args.args {
                let expr = match base {
                    ast::Expr::Subscript(s) => &*s.value,
                    other => other,
                };
                if let Some(written) = dotted(expr) {
                    self.facts.bases.push((key.clone(), written, line));
                }
            }
        }
        for dec in &c.decorator_list {
            self.decorator(&key, dec);
        }

        for stmt in &c.body {
            match stmt {
                ast::Stmt::FunctionDef(f) => self.function(f, Some((&name, &key))),
                ast::Stmt::ClassDef(inner) => self.class(inner, Some(&key)),
                _ => {}
            }
        }
    }

    fn assign(&mut self, a: &ast::StmtAssign) {
        // `__all__` is the written export list, not a value node.
        if let Some(ast::Expr::Name(n)) = a.targets.first()
            && n.id == "__all__"
        {
            if let ast::Expr::List(list) = &*a.value {
                let names = list
                    .elts
                    .iter()
                    .filter_map(|e| match e {
                        ast::Expr::StringLiteral(s) => Some(s.value.to_str().to_string()),
                        _ => None,
                    })
                    .collect();
                self.facts.dunder_all = Some(names);
            }
            return;
        }
        for t in &a.targets {
            let ast::Expr::Name(n) = t else { continue };
            self.value_node(&n.id, None, Some(a.value.range()), n.range());
        }
    }

    fn ann_assign(&mut self, a: &ast::StmtAnnAssign) {
        let ast::Expr::Name(n) = &*a.target else {
            return;
        };
        self.value_node(
            &n.id,
            Some(a.annotation.range()),
            a.value.as_ref().map(|v| v.range()),
            n.range(),
        );
    }

    fn value_node(
        &mut self,
        name: &str,
        ann: Option<TextRange>,
        value: Option<TextRange>,
        name_range: TextRange,
    ) {
        let key = format!("{}.{name}", self.module);
        if self.facts.nodes.iter().any(|n| n.key == key) {
            return; // rebinding is Python's business; the first is the fact
        }
        // PEP 8's own rule: constants are ALL_CAPS.
        let label = if is_const_name(name) { "Const" } else { "Var" };
        let mut props = Props::new();
        if let Some(r) = ann {
            props.insert("signature".into(), Value::String(self.snippet(r)));
        }
        if let Some(r) = value {
            props.insert("value".into(), Value::String(self.snippet(r)));
        }
        let line = self.line(name_range);
        props.insert("line".into(), Value::from(line));
        self.facts.nodes.push(Node {
            key: key.clone(),
            label: label.into(),
            extra_labels: Vec::new(),
            props,
        });
        self.facts
            .edges
            .push(edge_at(&self.module.clone(), &key, "CONTAINS", line));
        self.facts.decls.push(LocalDecl {
            name: name.to_string(),
            key,
            value: true,
        });
    }

    fn type_alias(&mut self, t: &ast::StmtTypeAlias) {
        let ast::Expr::Name(n) = &*t.name else { return };
        let key = format!("{}.{}", self.module, n.id);
        let mut props = Props::new();
        props.insert(
            "signature".into(),
            Value::String(self.snippet(t.value.range())),
        );
        let line = self.line(n.range());
        props.insert("line".into(), Value::from(line));
        self.facts.nodes.push(Node {
            key: key.clone(),
            label: "TypeAlias".into(),
            extra_labels: Vec::new(),
            props,
        });
        self.facts
            .edges
            .push(edge_at(&self.module.clone(), &key, "CONTAINS", line));
        self.facts.decls.push(LocalDecl {
            name: n.id.to_string(),
            key,
            value: false,
        });
    }

    fn decorator(&mut self, decl_key: &str, dec: &ast::Decorator) {
        // `@decorator` or `@app.route(…)` — the callable is the expression,
        // or the callee when the decorator itself is a call.
        let expr = match &dec.expression {
            ast::Expr::Call(c) => &*c.func,
            other => other,
        };
        let line = self.line(dec.range());
        match expr {
            ast::Expr::Name(n) => self.facts.calls.push(Call {
                caller: decl_key.to_string(),
                kind: CallKind::Plain(n.id.to_string()),
                line,
            }),
            ast::Expr::Attribute(_) => {
                if let Some(written) = dotted(expr) {
                    self.facts.calls.push(Call {
                        caller: decl_key.to_string(),
                        kind: CallKind::Chain(written.split('.').map(str::to_string).collect()),
                        line,
                    });
                }
            }
            _ => {}
        }
    }

    fn collect_calls(&mut self, caller: &str, body: &[ast::Stmt], class: Option<&str>) {
        let mut v = CallCollector {
            walker: self,
            caller: caller.to_string(),
            class: class.map(str::to_string),
        };
        for stmt in body {
            // Through visit_stmt, not walk_stmt: the body's own top-level
            // statements must hit the visitor too (function-scoped imports
            // live exactly there).
            ruff_python_ast::visitor::Visitor::visit_stmt(&mut v, stmt);
        }
    }

    fn snippet(&self, range: TextRange) -> String {
        self.src[range].to_string()
    }

    fn add_source(&self, props: &mut Props, range: TextRange) {
        props.insert(
            "_code".into(),
            serde_json::json!({
                "$desc": "source as written, for retrieval — not indexed or embedded",
                "$value": self.snippet(range),
            }),
        );
    }

    /// 1-based, like every editor's gutter.
    fn line(&self, range: TextRange) -> u64 {
        self.lines.line_index(range.start()).get() as u64
    }
}

/// The docstring convention is syntax enough: the first statement, when it
/// is a bare string literal.
fn docstring(body: &[ast::Stmt]) -> Option<String> {
    let ast::Stmt::Expr(e) = body.first()? else {
        return None;
    };
    let ast::Expr::StringLiteral(s) = &*e.value else {
        return None;
    };
    let text = s.value.to_str();
    let cleaned = text
        .lines()
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// `a.b.c` as written, when the expression is exactly a dotted name.
fn dotted(e: &ast::Expr) -> Option<String> {
    match e {
        ast::Expr::Name(n) => Some(n.id.to_string()),
        ast::Expr::Attribute(a) => Some(format!("{}.{}", dotted(&a.value)?, a.attr.id)),
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

/// Records every call a body makes. A bare name, a dotted chain rooted at a
/// bare name, or `self.m()` is held for resolution; anything deeper is
/// counted opaque. Nested defs are visited too — their calls belong to the
/// declaration a reader would look at.
struct CallCollector<'a, 'b> {
    walker: &'a mut Walker<'b>,
    caller: String,
    class: Option<String>,
}

impl ruff_python_ast::visitor::Visitor<'_> for CallCollector<'_, '_> {
    fn visit_stmt(&mut self, stmt: &ast::Stmt) {
        // Function-scoped imports (P2): the lazy-import idiom binds names
        // exactly as a top-level import does for resolution's purposes, so
        // they join the same table — recorded here because this visitor is
        // the only walker that enters bodies.
        match stmt {
            ast::Stmt::Import(i) => self.walker.import(i),
            ast::Stmt::ImportFrom(i) => self.walker.import_from(i),
            _ => {}
        }
        ruff_python_ast::visitor::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &ast::Expr) {
        // A string literal shaped like a dotted qualified name is a
        // candidate symbol reference (mock.patch("pkg.mod.fn")); it binds at
        // assemble only if something actually declares that name (P2).
        if let ast::Expr::StringLiteral(lit) = expr {
            let text = lit.value.to_str();
            if text.contains('.')
                && text.split('.').all(|seg| {
                    !seg.is_empty()
                        && seg
                            .chars()
                            .next()
                            .is_some_and(|c| c.is_alphabetic() || c == '_')
                        && seg.chars().all(|c| c.is_alphanumeric() || c == '_')
                })
            {
                let line = self.walker.line(lit.range());
                let caller = self.caller.clone();
                self.walker
                    .facts
                    .string_refs
                    .push((caller, text.to_string(), line));
            }
        }
        if let ast::Expr::Call(call) = expr {
            let line = self.walker.line(call.range());
            for arg in &*call.arguments.args {
                if let ast::Expr::Name(n) = arg {
                    let caller = self.caller.clone();
                    self.walker
                        .facts
                        .fn_refs
                        .push((caller, n.id.to_string(), line));
                }
            }
            match &*call.func {
                ast::Expr::Name(n) => self.walker.facts.calls.push(Call {
                    caller: self.caller.clone(),
                    kind: CallKind::Plain(n.id.to_string()),
                    line,
                }),
                ast::Expr::Attribute(at) => match dotted(&call.func) {
                    Some(written) => {
                        let parts: Vec<String> = written.split('.').map(str::to_string).collect();
                        let kind = match (parts.first().map(String::as_str), &self.class) {
                            (Some("self") | Some("cls"), Some(class)) if parts.len() == 2 => {
                                CallKind::This {
                                    class: class.clone(),
                                    method: parts[1].clone(),
                                }
                            }
                            // `self.attr.m()` — resolvable when the class
                            // states what `attr` is.
                            (Some("self") | Some("cls"), Some(class)) if parts.len() == 3 => {
                                CallKind::SelfAttr {
                                    class: class.clone(),
                                    attr: parts[1].clone(),
                                    method: parts[2].clone(),
                                }
                            }
                            (Some("self") | Some("cls"), _) => {
                                self.walker.facts.opaque += 1;
                                ruff_python_ast::visitor::walk_expr(self, expr);
                                return;
                            }
                            _ => CallKind::Chain(parts),
                        };
                        self.walker.facts.calls.push(Call {
                            caller: self.caller.clone(),
                            kind,
                            line,
                        });
                    }
                    // `super().m()` — a call receiver, so `dotted` refuses
                    // it; the shape itself is lexical and resolvable.
                    None => {
                        if let (ast::Expr::Call(inner), Some(class)) = (&*at.value, &self.class)
                            && let ast::Expr::Name(n) = &*inner.func
                            && n.id == "super"
                        {
                            self.walker.facts.calls.push(Call {
                                caller: self.caller.clone(),
                                kind: CallKind::Super {
                                    class: class.clone(),
                                    method: at.attr.id.to_string(),
                                },
                                line,
                            });
                        } else {
                            self.walker.facts.opaque += 1;
                        }
                    }
                },
                _ => self.walker.facts.opaque += 1,
            }
        }
        ruff_python_ast::visitor::walk_expr(self, expr);
    }
}

#[cfg(test)]
mod tests;
