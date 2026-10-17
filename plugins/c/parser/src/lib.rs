//! C into facts — nodes and edges a parser is certain of, leaving nothing
//! for a model to guess at.
//!
//! Same discipline as the five parsers beside it. Keys borrow C++'s own
//! convention for "this file is the scope": every symbol is
//! `{file}::{name}` — `src/wire/msgtx.c::main` — so two files defining the
//! same name (each tool's `main`, a reference implementation per variant)
//! are two facts, never a collision. **Binding** still follows C's linkage
//! rules: a call reaches its own file's definition first, then the one
//! global definition when the tree holds exactly one, and is counted when
//! several files define the name — which definition links is build
//! configuration a parser does not have. A header declares what a source
//! defines: the declaration merges into the definition, which wins the
//! node.
//!
//! The preprocessor is not expanded — a macro body is recorded as written,
//! and `#include "x.h"` is the written import. tree-sitter's C grammar
//! underneath: syntax only, no preprocessing, which is the point.

mod resolve;

pub use resolve::{Assembled, assemble};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tree_sitter::Node as TsNode;

/// What the parser reads through — the plugin contract's host.
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

/// One declaration this file contributes to the global or file-local
/// surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decl {
    pub name: String,
    pub key: String,
    /// A definition (has a body / initializer); a bare declaration yields
    /// to it at assemble.
    pub is_definition: bool,
    #[serde(default)]
    pub is_static: bool,
}

/// One type position a declaration writes down — a struct field, a
/// parameter, a return — resolved in [`assemble`] into a `USES_TYPE` edge
/// when it names a type this tree declares.
///
/// `written` is the bare tag or typedef name: `struct foo *next` names
/// `foo`, `Job j` names `Job`. A primitive names nothing and is never
/// recorded — the same rule that keeps builtins out of the call graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeRef {
    pub owner: String,
    pub role: String,
    pub name: String,
    pub written: String,
}

/// One call site: C calls are bare names — anything reached through a
/// value (function pointers, `ops->read()`) is a compiler's business.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Call {
    pub caller: String,
    pub name: String,
    pub line: u64,
}

/// A function-pointer binding the body states: `fp = target`,
/// `cb_t f = &target`, `vt.init = my_init`. Calls through the bound name
/// resolve to the target — first binding wins; reassignment flow is a
/// checker's business.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hint {
    pub caller: String,
    /// The bound name — an identifier, or `base.field` for a member slot
    /// (`->` and `.` normalized alike).
    pub name: String,
    /// The function identifier assigned.
    pub target: String,
}

/// Everything one file contributes — the opaque partial the component
/// serializes between the two phases.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileFacts {
    pub file: String,
    /// The file's stem, prefix for its statics.
    pub stem: String,
    pub failed: bool,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub decls: Vec<Decl>,
    pub calls: Vec<Call>,
    /// `#include` targets as written, with quotes-vs-angle and line:
    /// (path, is_system, line).
    pub includes: Vec<(String, bool, u64)>,
    pub opaque: usize,
    /// Function-pointer bindings the bodies state.
    pub hints: Vec<Hint>,
    /// The type positions this file's declarations write down.
    #[serde(default)]
    pub type_refs: Vec<TypeRef>,
    /// `(caller, bare name, line)` — names passed as call arguments
    /// (`register_handler(my_cb)`, `&my_cb`); resolved to REFERENCES only
    /// when the name is an in-tree function.
    pub fn_refs: Vec<(String, String, u64)>,
}

/// The extensions this plugin claims. `.h` is claimed too: a C header is C,
/// and its declarations carry the documentation.
pub const EXTENSIONS: &[&str] = &["c", "h"];

/// Parse one chunk of paths into per-file facts.
pub fn parse_chunk(files: &dyn Files, paths: &[String], include_source: bool) -> Vec<FileFacts> {
    paths
        .iter()
        .map(|p| match files.read(p) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => parse_file(p, &text, include_source),
                Err(_) => failed(p),
            },
            Err(_) => failed(p),
        })
        .collect()
}

/// Parse one pushed document.
pub fn parse_document(name: &str, bytes: &[u8], include_source: bool) -> Vec<FileFacts> {
    match String::from_utf8(bytes.to_vec()) {
        Ok(text) => vec![parse_file(name, &text, include_source)],
        Err(_) => vec![failed(name)],
    }
}

fn failed(path: &str) -> FileFacts {
    FileFacts {
        file: path.to_string(),
        failed: true,
        ..Default::default()
    }
}

fn stem_of(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    base.rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(base)
        .to_string()
}

/// The file node key: the path as written, extension included — `util.c`
/// and `util.h` are two files, and C's own convention says so: an include
/// names the header with its extension. Collapsing the pair made the
/// header vanish from the census and turned `util.c → util.h` into a
/// self-edge.
fn file_key(path: &str) -> String {
    path.to_string()
}

/// Headers no translation unit includes unless it is a test.
///
/// C states no test-ness of its own: there is no `#[test]`, no `_test.go`
/// build rule, no `@Test`. A `test_` prefix on a filename is a convention
/// with nothing enforcing it, and this parser does not guess. The one thing
/// actually written down is the framework header the file includes — which
/// says the *file* is built against a test framework, and says nothing about
/// which of its functions is a test. That is why the flag it sets is
/// `circumstantial` where go's and rust's are `definitive`.
const TEST_FRAMEWORK_HEADERS: &[&str] = &[
    "unity.h",
    "cmocka.h",
    "check.h",
    "greatest.h",
    "munit.h",
    "acutest.h",
    "utest.h",
    "minunit.h",
    "tau.h",
    "ctest.h",
];

/// Directories whose every header belongs to one framework.
const TEST_FRAMEWORK_DIRS: &[&str] = &["CUnit/", "criterion/", "gtest/", "cmocka/"];

fn is_test_framework_header(path: &str) -> bool {
    let norm = path.replace('\\', "/");
    if TEST_FRAMEWORK_DIRS.iter().any(|d| norm.starts_with(d)) {
        return true;
    }
    let base = norm.rsplit('/').next().unwrap_or(norm.as_str());
    TEST_FRAMEWORK_HEADERS.contains(&base)
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

fn parse_file(path: &str, text: &str, include_source: bool) -> FileFacts {
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_c::LANGUAGE.into())
        .is_err()
    {
        return failed(path);
    }
    let Some(tree) = parser.parse(text, None) else {
        return failed(path);
    };
    let root = tree.root_node();
    if root.kind() != "translation_unit" {
        return failed(path);
    }

    let mut facts = FileFacts {
        file: path.to_string(),
        stem: stem_of(path),
        ..Default::default()
    };
    let mut w = Walker {
        facts: &mut facts,
        src: text,
        file_key: file_key(path),
        include_source,
    };
    w.unit(root);

    // A tree that errored and yielded nothing did not parse in any useful
    // sense — a C++ header this plugin should not have been fed, say.
    if root.has_error() && facts.decls.is_empty() && facts.includes.is_empty() {
        return failed(path);
    }

    // The file node, like the family's Module — but no `path` prop: the key
    // *is* the path, and a property repeating the key states the same fact
    // twice. `includes` is written at assemble, where resolution can turn
    // each entry into the key of the file it names — which is what lets a
    // reader (and the dashboard) follow it as a link.
    facts.nodes.insert(
        0,
        Node {
            key: file_key(path),
            label: "File".into(),
            extra_labels: Vec::new(),
            props: Props::new(),
        },
    );
    for n in facts.nodes.iter_mut().skip(1) {
        n.props
            .insert("file".into(), Value::String(path.to_string()));
    }

    // A framework header taints its whole translation unit, file node
    // included: the file is the scope C itself uses, and it is the only
    // scope this evidence covers.
    if let Some(header) = facts
        .includes
        .iter()
        .map(|(p, _, _)| p)
        .find(|p| is_test_framework_header(p))
        .cloned()
    {
        let desc = format!("test code: this file includes the test framework header `{header}`");
        for n in facts.nodes.iter_mut() {
            set_test_flag(&mut n.props, "framework-include", &desc, "circumstantial");
        }
    }
    facts
}

struct Walker<'a> {
    facts: &'a mut FileFacts,
    src: &'a str,
    file_key: String,
    include_source: bool,
}

impl Walker<'_> {
    fn unit(&mut self, root: TsNode) {
        let mut cursor = root.walk();
        for node in root.children(&mut cursor) {
            self.top_level(node);
        }
    }

    fn top_level(&mut self, node: TsNode) {
        match node.kind() {
            "preproc_include" => {
                if let Some(pathn) = node.child_by_field_name("path") {
                    let raw = self.text(pathn);
                    let system = raw.starts_with('<');
                    let cleaned = raw
                        .trim_matches(|c| c == '"' || c == '<' || c == '>')
                        .to_string();
                    self.facts.includes.push((cleaned, system, self.line(node)));
                }
            }
            "preproc_def" | "preproc_function_def" => self.macro_def(node),
            "function_definition" => self.function(node, true),
            "declaration" => self.declaration(node),
            "type_definition" => self.typedef(node),
            "struct_specifier" | "union_specifier" | "enum_specifier" => {
                // A bare `struct foo { … };` at top level.
                self.record_type(node);
            }
            // `#ifdef` arms and `extern "C"` blocks hold ordinary top-level
            // declarations — walk in, the way the Python parser walks
            // if/try.
            "preproc_ifdef"
            | "preproc_if"
            | "preproc_else"
            | "preproc_elif"
            | "linkage_specification"
            | "declaration_list" => {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    self.top_level(child);
                }
            }
            "expression_statement" => {}
            _ => {}
        }
    }

    /// `#define NAME body` — an Object-like macro is a Const with its body
    /// as written; a function-like one is a Macro with its parameters.
    fn macro_def(&mut self, node: TsNode) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let name = self.text(name_node);
        // Include guards are bookkeeping, not interface.
        if (name.ends_with("_H") || name.ends_with("_H_") || name.starts_with('_'))
            && node.child_by_field_name("value").is_none()
        {
            return;
        }
        let key = format!("{}::{name}", self.file_key);
        let line = self.line(name_node);
        let mut props = Props::new();
        let label = if node.kind() == "preproc_function_def" {
            if let Some(params) = node.child_by_field_name("parameters") {
                props.insert(
                    "signature".into(),
                    Value::String(format!("{name}{}", self.text(params))),
                );
            }
            "Macro"
        } else {
            if let Some(v) = node.child_by_field_name("value") {
                props.insert(
                    "value".into(),
                    Value::String(self.text(v).trim().to_string()),
                );
            }
            "Const"
        };
        if let Some(doc) = self.doc_before(node) {
            props.insert("doc_comment".into(), Value::String(doc));
        }
        props.insert("line".into(), Value::from(line));
        props.insert("end_line".into(), Value::from(self.end_line(node)));
        self.push_decl(key, label, props, line, true, false, name);
    }

    fn function(&mut self, node: TsNode, is_definition: bool) {
        let Some(declarator) = node.child_by_field_name("declarator") else {
            return;
        };
        let Some((name, name_node)) = self.declarator_name(declarator) else {
            return;
        };
        let is_static = self.has_storage(node, "static");
        let key = format!("{}::{name}", self.file_key);
        let line = self.line(name_node);

        // The signature as written: return type + declarator, minus the body.
        let ret = node
            .child_by_field_name("type")
            .map(|t| self.text(t))
            .unwrap_or_default();
        let sig = format!("{ret} {}", self.text(declarator));

        let mut props = Props::new();
        props.insert("signature".into(), Value::String(sig.trim().to_string()));
        if let Some(doc) = self.doc_before(node) {
            props.insert("doc_comment".into(), Value::String(doc));
        }
        if is_static {
            props.insert("visibility".into(), Value::String("static".into()));
        }
        props.insert("line".into(), Value::from(line));
        props.insert("end_line".into(), Value::from(self.end_line(node)));
        if self.include_source && is_definition {
            self.add_source(&mut props, node);
        }
        self.push_decl(
            key.clone(),
            "Function",
            props,
            line,
            is_definition,
            is_static,
            name,
        );

        // The declared interface: what it is handed and what it hands back.
        if let Some(t) = node.child_by_field_name("type") {
            self.type_ref(&key, "return", "", t);
        }
        if let Some(params) = Self::parameter_list(declarator) {
            let mut cursor = params.walk();
            for p in params.named_children(&mut cursor) {
                if p.kind() != "parameter_declaration" {
                    continue;
                }
                let pname = p
                    .named_children(&mut p.walk())
                    .find_map(|d| self.declarator_name_field(d).map(|(n, _)| n))
                    .unwrap_or_default();
                if let Some(t) = p.child_by_field_name("type") {
                    self.type_ref(&key, "param", &pname, t);
                }
            }
        }

        if let Some(body) = node.child_by_field_name("body") {
            self.collect_calls(&key, body);
        }
    }

    /// The parameter list of a (possibly pointer-wrapped) declarator.
    fn parameter_list(declarator: TsNode) -> Option<TsNode> {
        if declarator.kind() == "function_declarator" {
            return declarator.child_by_field_name("parameters");
        }
        let mut cursor = declarator.walk();
        declarator
            .named_children(&mut cursor)
            .find_map(Self::parameter_list)
    }

    /// A top-level declaration: an extern function prototype, a global
    /// variable, or both at once.
    fn declaration(&mut self, node: TsNode) {
        let is_static = self.has_storage(node, "static");
        let is_extern = self.has_storage(node, "extern");
        let ty = node
            .child_by_field_name("type")
            .map(|t| self.text(t))
            .unwrap_or_default();
        // The type may itself be a struct/enum/union definition.
        if let Some(t) = node.child_by_field_name("type")
            && matches!(
                t.kind(),
                "struct_specifier" | "union_specifier" | "enum_specifier"
            )
        {
            self.record_type(t);
        }

        let mut cursor = node.walk();
        for d in node.named_children(&mut cursor) {
            let (declarator, initializer) = match d.kind() {
                "init_declarator" => (
                    d.child_by_field_name("declarator"),
                    d.child_by_field_name("value"),
                ),
                "function_declarator"
                | "pointer_declarator"
                | "array_declarator"
                | "identifier" => (Some(d), None),
                _ => continue,
            };
            let Some(declarator) = declarator else {
                continue;
            };
            let Some((name, name_node)) = self.declarator_name(declarator) else {
                continue;
            };
            let is_function = self.is_function_declarator(declarator);
            let line = self.line(name_node);
            let key = format!("{}::{name}", self.file_key);

            let mut props = Props::new();
            if is_function {
                props.insert(
                    "signature".into(),
                    Value::String(format!("{ty} {}", self.text(declarator)).trim().to_string()),
                );
            } else {
                props.insert("signature".into(), Value::String(ty.clone()));
                if let Some(init) = initializer {
                    props.insert("value".into(), Value::String(self.text(init)));
                }
            }
            if let Some(doc) = self.doc_before(node) {
                props.insert("doc_comment".into(), Value::String(doc));
            }
            if is_static {
                props.insert("visibility".into(), Value::String("static".into()));
            }
            props.insert("line".into(), Value::from(line));
            props.insert("end_line".into(), Value::from(self.end_line(node)));

            let label = if is_function { "Function" } else { "Var" };
            // A prototype is a declaration; a global with an initializer —
            // or without `extern` — is a definition.
            let is_definition = !is_function && !is_extern;
            self.push_decl(key, label, props, line, is_definition, is_static, name);
        }
    }

    fn typedef(&mut self, node: TsNode) {
        // The underlying type may be a struct/union/enum defined right here.
        if let Some(t) = node.child_by_field_name("type")
            && matches!(
                t.kind(),
                "struct_specifier" | "union_specifier" | "enum_specifier"
            )
        {
            self.record_type(t);
        }
        let ty = node
            .child_by_field_name("type")
            .map(|t| {
                // For an inline struct the whole body is not a signature;
                // name it by its head.
                let mut text = self.text(t);
                if let Some(brace) = text.find('{') {
                    text.truncate(brace);
                }
                text.trim().to_string()
            })
            .unwrap_or_default();
        let mut cursor = node.walk();
        for d in node.named_children(&mut cursor) {
            if !matches!(
                d.kind(),
                "type_identifier"
                    | "pointer_declarator"
                    | "function_declarator"
                    | "array_declarator"
            ) {
                continue;
            }
            let Some((name, name_node)) = self.declarator_name_ty(d) else {
                continue;
            };
            let line = self.line(name_node);
            let mut props = Props::new();
            props.insert("signature".into(), Value::String(ty.clone()));
            if let Some(doc) = self.doc_before(node) {
                props.insert("doc_comment".into(), Value::String(doc));
            }
            props.insert("line".into(), Value::from(line));
            props.insert("end_line".into(), Value::from(self.end_line(node)));
            let key = format!("{}::{name}", self.file_key);
            // An anonymous struct's fields have no tag to belong to, so they
            // belong to the name this typedef gives it. A tagged one is
            // already recorded under the tag by `record_type`.
            if d.kind() == "type_identifier"
                && let Some(t) = node.child_by_field_name("type")
                && matches!(t.kind(), "struct_specifier" | "union_specifier")
                && t.child_by_field_name("name").is_none()
                && let Some(body) = t.child_by_field_name("body")
            {
                self.field_types(&key, body);
            }
            self.push_decl(key, "TypeAlias", props, line, true, false, name);
        }
    }

    /// `struct foo { … }` / `enum bar { … }` — a named type with a body.
    /// The type a type node names, when it names one this tree could
    /// declare: a tag (`struct foo` → `foo`) or a typedef name. A primitive
    /// (`int`, `unsigned long`, `void`) names nothing — nobody's edge, the
    /// same call the call graph makes for builtins.
    fn type_named(&self, node: TsNode) -> Option<String> {
        match node.kind() {
            "type_identifier" => Some(self.text(node)),
            "struct_specifier" | "union_specifier" | "enum_specifier" => {
                node.child_by_field_name("name").map(|n| self.text(n))
            }
            _ => None,
        }
    }

    /// A struct or union body's fields: each `name: type` as written, and a
    /// type reference for each, owned by `owner`.
    ///
    /// Shared with [`Self::typedef`], because `typedef struct { … } Name` has
    /// no tag for [`Self::record_type`] to hang the fields on — which is how
    /// the fields of every anonymous struct went unrecorded, the shape most C
    /// in the wild is written in.
    fn field_types(&mut self, owner: &str, body: TsNode) -> Vec<String> {
        let mut fields: Vec<String> = Vec::new();
        let mut cursor = body.walk();
        for m in body.named_children(&mut cursor) {
            if m.kind() != "field_declaration" {
                continue;
            }
            let fty = m
                .child_by_field_name("type")
                .map(|t| self.text(t))
                .unwrap_or_default();
            let mut fc = m.walk();
            for fd in m.named_children(&mut fc) {
                if matches!(
                    fd.kind(),
                    "field_identifier"
                        | "pointer_declarator"
                        | "array_declarator"
                        | "function_declarator"
                ) && let Some((fname, _)) = self.declarator_name_field(fd)
                {
                    if let Some(t) = m.child_by_field_name("type") {
                        self.type_ref(owner, "field", &fname, t);
                    }
                    fields.push(format!("{fname}: {fty}"));
                }
            }
        }
        fields
    }

    /// Record the type `node` names as one type position of `owner`.
    fn type_ref(&mut self, owner: &str, role: &str, name: &str, node: TsNode) {
        if let Some(written) = self.type_named(node) {
            self.facts.type_refs.push(TypeRef {
                owner: owner.to_string(),
                role: role.to_string(),
                name: name.to_string(),
                written,
            });
        }
    }

    fn record_type(&mut self, node: TsNode) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return; // an anonymous struct is its typedef's business
        };
        let Some(body) = node.child_by_field_name("body") else {
            return; // `struct foo;` forward-declares, defining nothing
        };
        let name = self.text(name_node);
        let line = self.line(name_node);
        let label = match node.kind() {
            "struct_specifier" => "Struct",
            "union_specifier" => "Union",
            _ => "Enum",
        };

        let mut props = Props::new();
        if let Some(doc) = self.doc_before(node) {
            props.insert("doc_comment".into(), Value::String(doc));
        }
        props.insert("line".into(), Value::from(line));
        props.insert("end_line".into(), Value::from(self.end_line(node)));

        if label == "Enum" {
            let mut variants: Vec<String> = Vec::new();
            let mut cursor = body.walk();
            for m in body.named_children(&mut cursor) {
                if m.kind() == "enumerator"
                    && let Some(n) = m.child_by_field_name("name")
                {
                    variants.push(match m.child_by_field_name("value") {
                        Some(v) => format!("{} = {}", self.text(n), self.text(v)),
                        None => self.text(n),
                    });
                }
            }
            if !variants.is_empty() {
                props.insert(
                    "variants".into(),
                    serde_json::json!({
                        "$desc": "the enum's constants, values as written",
                        "$value": variants,
                    }),
                );
            }
        } else {
            let owner = format!("{}::{name}", self.file_key);
            let fields = self.field_types(&owner, body);
            if !fields.is_empty() {
                props.insert(
                    "fields".into(),
                    serde_json::json!({
                        "$desc": "the fields it declares, each with its type as written",
                        "$value": fields,
                    }),
                );
            }
        }
        let key = format!("{}::{name}", self.file_key);
        self.push_decl(key, label, props, line, true, false, name);
    }

    #[allow(clippy::too_many_arguments)]
    fn push_decl(
        &mut self,
        key: String,
        label: &str,
        props: Props,
        line: u64,
        is_definition: bool,
        is_static: bool,
        name: String,
    ) {
        // Within one file, a definition replaces this file's own bare
        // declaration (header pattern is cross-file and settles at
        // assemble; this handles prototype-then-definition in one file).
        if let Some(existing) = self.facts.nodes.iter_mut().find(|n| n.key == key) {
            if is_definition
                && self
                    .facts
                    .decls
                    .iter()
                    .find(|d| d.key == key)
                    .is_some_and(|d| !d.is_definition)
            {
                existing.label = label.to_string();
                existing.props = props;
                if let Some(d) = self.facts.decls.iter_mut().find(|d| d.key == key) {
                    d.is_definition = true;
                }
                // The CONTAINS line moves with the definition.
                if let Some(e) = self
                    .facts
                    .edges
                    .iter_mut()
                    .find(|e| e.dst == key && e.ty == "CONTAINS")
                {
                    e.props.insert("line".into(), Value::from(line));
                }
            }
            return;
        }
        self.facts.nodes.push(Node {
            key: key.clone(),
            label: label.into(),
            extra_labels: Vec::new(),
            props,
        });
        self.facts
            .edges
            .push(edge_at(&self.file_key.clone(), &key, "CONTAINS", line));
        self.facts.decls.push(Decl {
            name,
            key,
            is_definition,
            is_static,
        });
    }

    /// The function identifier an initializer/right-hand side names:
    /// `target`, `&target` — anything else is a value's business.
    fn fn_ref(&self, node: TsNode) -> Option<String> {
        match node.kind() {
            "identifier" => Some(self.text(node)),
            "pointer_expression" | "unary_expression" => {
                let inner = node
                    .child_by_field_name("argument")
                    .or_else(|| node.named_child(0))?;
                (inner.kind() == "identifier").then(|| self.text(inner))
            }
            _ => None,
        }
    }

    /// A callable path: a bare identifier, or `base.field` for a member
    /// slot (`->` and `.` alike). Deeper chains are a value's business.
    fn call_path(&self, node: TsNode) -> Option<String> {
        match node.kind() {
            "identifier" => Some(self.text(node)),
            "field_expression" => {
                let base = node.child_by_field_name("argument")?;
                let field = node.child_by_field_name("field")?;
                (base.kind() == "identifier")
                    .then(|| format!("{}.{}", self.text(base), self.text(field)))
            }
            // `(*fn)(v)` — the deref names the pointer.
            "parenthesized_expression" => {
                let inner = node.named_child(0)?;
                if matches!(inner.kind(), "pointer_expression" | "unary_expression") {
                    let arg = inner
                        .child_by_field_name("argument")
                        .or_else(|| inner.named_child(0))?;
                    return (arg.kind() == "identifier").then(|| self.text(arg));
                }
                None
            }
            _ => None,
        }
    }

    fn hint(&mut self, caller: &str, name: String, target: String) {
        if !self
            .facts
            .hints
            .iter()
            .any(|h| h.caller == caller && h.name == name)
        {
            self.facts.hints.push(Hint {
                caller: caller.to_string(),
                name,
                target,
            });
        }
    }

    fn collect_calls(&mut self, caller: &str, body: TsNode) {
        let mut stack = vec![body];
        while let Some(node) = stack.pop() {
            match node.kind() {
                "call_expression" => {
                    if let Some(f) = node.child_by_field_name("function") {
                        match self.call_path(f) {
                            Some(name) => self.facts.calls.push(Call {
                                caller: caller.to_string(),
                                name,
                                line: self.line(node),
                            }),
                            // `ops->cfg->read()` and friends — a value's
                            // business.
                            None => self.facts.opaque += 1,
                        }
                    }
                    if let Some(args) = node.child_by_field_name("arguments") {
                        for a in args.named_children(&mut args.walk()) {
                            if let Some(name) = self.fn_ref(a) {
                                self.facts.fn_refs.push((
                                    caller.to_string(),
                                    name,
                                    self.line(node),
                                ));
                            }
                        }
                    }
                }
                // `cb_t f = target;` — a declarator init binds the name.
                "declaration" => {
                    for d in node.named_children(&mut node.walk()) {
                        if d.kind() == "init_declarator"
                            && let Some((name, _)) = d
                                .child_by_field_name("declarator")
                                .and_then(|dd| self.declarator_name(dd))
                            && let Some(target) =
                                d.child_by_field_name("value").and_then(|v| self.fn_ref(v))
                        {
                            self.hint(caller, name, target);
                        }
                    }
                }
                // `fp = target;` / `vt.init = my_init;`
                "assignment_expression" => {
                    if let (Some(left), Some(right)) = (
                        node.child_by_field_name("left"),
                        node.child_by_field_name("right"),
                    ) && let Some(name) = self.call_path(left)
                        && let Some(target) = self.fn_ref(right)
                    {
                        self.hint(caller, name, target);
                    }
                }
                _ => {}
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                stack.push(child);
            }
        }
    }

    /// The identifier inside a possibly-nested declarator.
    fn declarator_name<'t>(&self, node: TsNode<'t>) -> Option<(String, TsNode<'t>)> {
        match node.kind() {
            "identifier" => Some((self.text(node), node)),
            "function_declarator"
            | "pointer_declarator"
            | "array_declarator"
            | "parenthesized_declarator"
            | "init_declarator" => {
                let inner = node
                    .child_by_field_name("declarator")
                    .or_else(|| node.named_child(0))?;
                self.declarator_name(inner)
            }
            _ => None,
        }
    }

    fn declarator_name_ty<'t>(&self, node: TsNode<'t>) -> Option<(String, TsNode<'t>)> {
        match node.kind() {
            "type_identifier" => Some((self.text(node), node)),
            _ => {
                let inner = node
                    .child_by_field_name("declarator")
                    .or_else(|| node.named_child(0))?;
                self.declarator_name_ty(inner)
            }
        }
    }

    fn declarator_name_field<'t>(&self, node: TsNode<'t>) -> Option<(String, TsNode<'t>)> {
        match node.kind() {
            "field_identifier" => Some((self.text(node), node)),
            _ => {
                let inner = node
                    .child_by_field_name("declarator")
                    .or_else(|| node.named_child(0))?;
                self.declarator_name_field(inner)
            }
        }
    }

    fn is_function_declarator(&self, node: TsNode) -> bool {
        match node.kind() {
            "function_declarator" => true,
            "pointer_declarator" | "init_declarator" | "parenthesized_declarator" => node
                .child_by_field_name("declarator")
                .or_else(|| node.named_child(0))
                .is_some_and(|d| self.is_function_declarator(d)),
            _ => false,
        }
    }

    fn has_storage(&self, node: TsNode, word: &str) -> bool {
        let mut cursor = node.walk();
        node.children(&mut cursor)
            .any(|c| c.kind() == "storage_class_specifier" && self.text(c) == word)
    }

    /// The comment block immediately above: `/** … */`, `/* … */`, or a run
    /// of `//` lines — C documents in all three, so all three count.
    fn doc_before(&self, node: TsNode) -> Option<String> {
        let mut prev = node.prev_sibling()?;
        if prev.kind() != "comment" {
            return None;
        }
        // A run of // lines reads bottom-up; gather then flip.
        let mut parts = vec![self.text(prev)];
        while let Some(p) = prev.prev_sibling() {
            if p.kind() == "comment"
                && self.text(p).starts_with("//")
                && parts.last().is_some_and(|l| l.starts_with("//"))
            {
                parts.push(self.text(p));
                prev = p;
            } else {
                break;
            }
        }
        parts.reverse();
        let cleaned = parts
            .iter()
            .flat_map(|c| {
                c.trim_start_matches("/**")
                    .trim_start_matches("/*")
                    .trim_end_matches("*/")
                    .lines()
                    .map(|l| {
                        l.trim()
                            .trim_start_matches('*')
                            .trim_start_matches("//")
                            .trim()
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        (!cleaned.is_empty()).then_some(cleaned)
    }

    fn add_source(&self, props: &mut Props, node: TsNode) {
        props.insert(
            "_code".into(),
            serde_json::json!({
                "$desc": "source as written, for retrieval — not indexed or embedded",
                "$value": self.text(node),
            }),
        );
    }

    fn text(&self, node: TsNode) -> String {
        self.src[node.byte_range()].to_string()
    }

    /// 1-based, like every editor's gutter.
    fn line(&self, node: TsNode) -> u64 {
        node.start_position().row as u64 + 1
    }

    /// Where a declaration stops. With [`Walker::line`] it says how big a
    /// thing is without opening it, and lets `snippet` read exactly the
    /// declaration instead of guessing a fixed number of lines after its
    /// first.
    fn end_line(&self, node: TsNode) -> u64 {
        node.end_position().row as u64 + 1
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

#[cfg(test)]
mod tests;
