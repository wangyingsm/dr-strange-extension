//! Java into facts — nodes and edges a parser is certain of, leaving
//! nothing for a model to guess at.
//!
//! Same discipline as the four parsers beside it: parse each file alone,
//! resolve across files once in [`assemble`], count whatever could not be
//! resolved. tree-sitter's Java grammar underneath — syntax only, no
//! symbol tables, which is the point.
//!
//! Keys are Java's own qualified names: the `package` declaration, then the
//! type, then the member — `com.acme.core.Engine.start`; a nested type is
//! `com.acme.core.Engine.Builder`. A file is not a node: in Java the type
//! is the unit, and the package is the container.

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

/// One call site, held until every type is known.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Call {
    pub caller: String,
    /// The type the caller sits in, for own-method and inherited resolution.
    pub in_type: String,
    pub kind: CallKind,
    pub line: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallKind {
    /// `helper()` / `this.helper()` — the class's own surface, or a static
    /// import.
    Own(String),
    /// `Helper.create()` / `com.acme.Helper.create()` — through a type
    /// reference, written down.
    Static { type_ref: String, method: String },
    /// `new Foo(…)` — instantiation is a call to the type.
    New(String),
    /// `super.m()` — the extends chain, where this tree holds it.
    Super(String),
    /// `lg.log()` — a value receiver, typed when the source states its
    /// class (a declared local, a parameter, or a field).
    Receiver { name: String, method: String },
    /// `this.field.m()` — through the field's declared type.
    SelfField { field: String, method: String },
    /// `obj.field.m()` — one field hop on a typed value.
    FieldChain {
        obj: String,
        field: String,
        method: String,
    },
    /// `Foo.getInstance().bar()` — a static factory chain, typed by the
    /// factory's declared return.
    StaticChain {
        type_ref: String,
        first: String,
        method: String,
    },
}

/// How a name is bound to a type the source states — a declared local, a
/// parameter, or a field. `caller` is a method key for locals and params, a
/// TYPE key for fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hint {
    pub caller: String,
    pub name: String,
    /// The type as written (generics stripped to what they subscript).
    pub written: String,
}

/// One name a type reference can bind through: `import a.b.C` binds C;
/// `import static a.b.C.m` binds m.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Import {
    pub local: String,
    pub target: String,
    #[serde(default)]
    pub is_static: bool,
}

/// A declared type: name, kind, and its method surface (for cross-file
/// static and inherited resolution).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeDecl {
    pub key: String,
    pub methods: Vec<String>,
    /// `extends` targets as written, for the inherited-call walk.
    pub extends: Vec<String>,
}

/// Everything one file contributes — the opaque partial the component
/// serializes between the two phases.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileFacts {
    pub file: String,
    /// The `package` declaration, "" for the default package.
    pub package: String,
    pub failed: bool,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub calls: Vec<Call>,
    pub imports: Vec<Import>,
    /// `import a.b.*` packages, in order.
    pub wildcards: Vec<String>,
    /// Import statements as written, with lines — for IMPORTS edges from
    /// this file's top-level types.
    pub import_lines: Vec<(String, u64)>,
    /// Top-level type keys in this file (IMPORTS edges hang off them).
    pub top_types: Vec<String>,
    pub types: Vec<TypeDecl>,
    /// `(src key, name as written, edge type, line)` — extends, implements,
    /// and annotations, all resolved the same way.
    pub clauses: Vec<(String, String, String, u64)>,
    /// The package docstring, from `package-info.java`.
    pub package_doc: Option<String>,
    pub opaque: usize,
    /// Type bindings the source states (params, locals, fields).
    pub hints: Vec<Hint>,
    /// `(method key, declared return as written)` when it names a plain
    /// type — what types a static-factory chain.
    pub returns: Vec<(String, String)>,
}

/// The extensions this plugin claims.
pub const EXTENSIONS: &[&str] = &["java"];

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

fn parse_file(path: &str, text: &str, include_source: bool) -> FileFacts {
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .is_err()
    {
        return failed(path);
    }
    let Some(tree) = parser.parse(text, None) else {
        return failed(path);
    };
    let root = tree.root_node();
    if root.kind() != "program" {
        return failed(path);
    }

    let mut facts = FileFacts {
        file: path.to_string(),
        ..Default::default()
    };
    let mut w = Walker {
        facts: &mut facts,
        src: text,
        include_source,
    };
    w.program(root);
    // tree-sitter always yields a tree; one that errored and produced no
    // facts at all did not parse in any useful sense.
    if root.has_error() && facts.types.is_empty() && facts.package.is_empty() {
        return failed(path);
    }
    for n in facts.nodes.iter_mut() {
        n.props
            .insert("file".into(), Value::String(path.to_string()));
    }
    facts
}

struct Walker<'a> {
    facts: &'a mut FileFacts,
    src: &'a str,
    include_source: bool,
}

impl Walker<'_> {
    fn program(&mut self, root: TsNode) {
        // The package declaration first — every key hangs off it.
        let mut cursor = root.walk();
        let children: Vec<TsNode> = root.children(&mut cursor).collect();
        for node in &children {
            if node.kind() == "package_declaration"
                && let Some(name) = node
                    .named_children(&mut node.walk())
                    .find(|c| c.kind() == "scoped_identifier" || c.kind() == "identifier")
            {
                self.facts.package = self.text(name);
            }
        }
        // `package-info.java` exists to carry the package's javadoc.
        if self.facts.file.ends_with("package-info.java")
            && let Some(pkg_decl) = children.iter().find(|c| c.kind() == "package_declaration")
        {
            self.facts.package_doc = self.javadoc_before(*pkg_decl);
        }

        for node in &children {
            match node.kind() {
                "import_declaration" => self.import(*node),
                "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "record_declaration"
                | "annotation_type_declaration" => {
                    if let Some(key) = self.type_decl(*node, None) {
                        self.facts.top_types.push(key);
                    }
                }
                _ => {}
            }
        }
    }

    fn import(&mut self, node: TsNode) {
        let is_static = self.node_text_contains(node, "static");
        let mut target = String::new();
        let mut wildcard = false;
        for c in node.named_children(&mut node.walk()) {
            match c.kind() {
                "scoped_identifier" | "identifier" => target = self.text(c),
                "asterisk" => wildcard = true,
                _ => {}
            }
        }
        if target.is_empty() {
            return;
        }
        let line = self.line(node);
        if wildcard {
            self.facts.wildcards.push(target.clone());
            self.facts.import_lines.push((format!("{target}.*"), line));
            return;
        }
        self.facts.import_lines.push((target.clone(), line));
        let local = target.rsplit('.').next().unwrap_or(&target).to_string();
        self.facts.imports.push(Import {
            local,
            target,
            is_static,
        });
    }

    /// One type declaration, at top level or nested. Returns its key.
    fn type_decl(&mut self, node: TsNode, outer: Option<&str>) -> Option<String> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.text(name_node);
        let key = match outer {
            Some(outer_key) => format!("{outer_key}.{name}"),
            None if self.facts.package.is_empty() => name.clone(),
            None => format!("{}.{name}", self.facts.package),
        };
        let parent = outer
            .map(str::to_string)
            .unwrap_or_else(|| self.facts.package.clone());

        let label = match node.kind() {
            "class_declaration" => "Class",
            "interface_declaration" => "Interface",
            "enum_declaration" => "Enum",
            "record_declaration" => "Record",
            _ => "Annotation",
        };
        let line = self.line(name_node);

        let mut props = Props::new();
        if let Some(doc) = self.javadoc_before(node) {
            props.insert("doc_comment".into(), Value::String(doc));
        }
        if let Some(vis) = self.visibility(node) {
            props.insert("visibility".into(), Value::String(vis));
        }
        props.insert("line".into(), Value::from(line));
        if self.include_source {
            self.add_source(&mut props, node);
        }

        let mut decl = TypeDecl {
            key: key.clone(),
            methods: Vec::new(),
            extends: Vec::new(),
        };

        // extends / implements are syntax. A generic base extends what it
        // subscripts; `permits` is sealed-class bookkeeping, not ancestry.
        if let Some(sc) = node.child_by_field_name("superclass") {
            for t in sc.named_children(&mut sc.walk()) {
                if let Some(written) = self.type_name(t) {
                    decl.extends.push(written.clone());
                    self.facts
                        .clauses
                        .push((key.clone(), written, "EXTENDS".into(), line));
                }
            }
        }
        for field in ["interfaces", "extends_interfaces"] {
            if let Some(list) = node.child_by_field_name(field) {
                for tl in list.named_children(&mut list.walk()) {
                    for t in tl.named_children(&mut tl.walk()) {
                        if let Some(written) = self.type_name(t) {
                            let ty = if node.kind() == "interface_declaration" {
                                decl.extends.push(written.clone());
                                "EXTENDS"
                            } else {
                                "IMPLEMENTS"
                            };
                            self.facts
                                .clauses
                                .push((key.clone(), written, ty.into(), line));
                        }
                    }
                }
            }
        }
        self.annotations(node, &key);

        // Records declare their fields in the header.
        let mut fields: Vec<String> = Vec::new();
        if node.kind() == "record_declaration"
            && let Some(params) = node.child_by_field_name("parameters")
        {
            for p in params.named_children(&mut params.walk()) {
                if p.kind() == "formal_parameter"
                    && let (Some(t), Some(n)) =
                        (p.child_by_field_name("type"), p.child_by_field_name("name"))
                {
                    fields.push(format!("{}: {}", self.text(n), self.text(t)));
                }
            }
        }

        let mut variants: Vec<String> = Vec::new();
        if let Some(body) = node.child_by_field_name("body") {
            for member in body.named_children(&mut body.walk()) {
                match member.kind() {
                    "field_declaration" | "constant_declaration" => {
                        let ty = member
                            .child_by_field_name("type")
                            .map(|t| self.text(t))
                            .unwrap_or_default();
                        let typed = member
                            .child_by_field_name("type")
                            .and_then(|t| self.type_name(t));
                        for d in member.named_children(&mut member.walk()) {
                            if d.kind() == "variable_declarator"
                                && let Some(n) = d.child_by_field_name("name")
                            {
                                let fname = self.text(n);
                                if let Some(written) = &typed {
                                    self.hint(&key, fname.clone(), written.clone());
                                }
                                fields.push(format!("{fname}: {ty}"));
                            }
                        }
                    }
                    "method_declaration" => {
                        self.method(member, &key, &name, node.kind(), &mut decl);
                    }
                    "constructor_declaration" => {
                        self.method(member, &key, &name, node.kind(), &mut decl);
                    }
                    "enum_constant" => {
                        if let Some(n) = member.child_by_field_name("name") {
                            variants.push(self.text(n));
                        }
                    }
                    "enum_body_declarations" => {
                        for m in member.named_children(&mut member.walk()) {
                            match m.kind() {
                                "method_declaration" | "constructor_declaration" => {
                                    self.method(m, &key, &name, node.kind(), &mut decl)
                                }
                                "field_declaration" => {
                                    let ty = m
                                        .child_by_field_name("type")
                                        .map(|t| self.text(t))
                                        .unwrap_or_default();
                                    let typed = m
                                        .child_by_field_name("type")
                                        .and_then(|t| self.type_name(t));
                                    for d in m.named_children(&mut m.walk()) {
                                        if d.kind() == "variable_declarator"
                                            && let Some(n) = d.child_by_field_name("name")
                                        {
                                            let fname = self.text(n);
                                            if let Some(written) = &typed {
                                                self.hint(&key, fname.clone(), written.clone());
                                            }
                                            fields.push(format!("{fname}: {ty}"));
                                        }
                                    }
                                }
                                "class_declaration"
                                | "interface_declaration"
                                | "enum_declaration"
                                | "record_declaration" => {
                                    self.type_decl(m, Some(&key));
                                }
                                _ => {}
                            }
                        }
                    }
                    "class_declaration"
                    | "interface_declaration"
                    | "enum_declaration"
                    | "record_declaration"
                    | "annotation_type_declaration" => {
                        self.type_decl(member, Some(&key));
                    }
                    _ => {}
                }
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
        if !variants.is_empty() {
            props.insert(
                "variants".into(),
                serde_json::json!({
                    "$desc": "the enum's constants, as written",
                    "$value": variants,
                }),
            );
        }

        self.facts.nodes.push(Node {
            key: key.clone(),
            label: label.into(),
            extra_labels: Vec::new(),
            props,
        });
        self.facts
            .edges
            .push(edge_at(&parent, &key, "CONTAINS", line));
        self.facts.types.push(decl);
        Some(key)
    }

    fn hint(&mut self, caller: &str, name: String, written: String) {
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
            });
        }
    }

    fn method(
        &mut self,
        node: TsNode,
        type_key: &str,
        type_name: &str,
        type_kind: &str,
        decl: &mut TypeDecl,
    ) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let name = self.text(name_node);
        let key = format!("{type_key}.{name}");
        let line = self.line(name_node);

        let params = node
            .child_by_field_name("parameters")
            .map(|p| self.text(p))
            .unwrap_or_default();
        let ret = node
            .child_by_field_name("type")
            .map(|t| format!("{} ", self.text(t)))
            .unwrap_or_default();
        let mut props = Props::new();
        props.insert(
            "signature".into(),
            Value::String(format!("{ret}{name}{params}")),
        );
        if let Some(doc) = self.javadoc_before(node) {
            props.insert("doc_comment".into(), Value::String(doc));
        }
        if let Some(vis) = self.visibility(node) {
            props.insert("visibility".into(), Value::String(vis));
        }
        props.insert("line".into(), Value::from(line));
        if self.include_source {
            self.add_source(&mut props, node);
        }

        // Overloads share a name and a key: one node, first (with the docs)
        // wins; the method surface keeps a single entry too.
        if !self.facts.nodes.iter().any(|n| n.key == key) {
            self.facts.nodes.push(Node {
                key: key.clone(),
                label: "Method".into(),
                extra_labels: Vec::new(),
                props,
            });
            // An interface demands its methods (HAS_METHOD, the family's
            // trait convention); a class contains its own.
            let ty = if type_kind == "interface_declaration"
                || type_kind == "annotation_type_declaration"
            {
                "HAS_METHOD"
            } else {
                "CONTAINS"
            };
            self.facts.edges.push(edge_at(type_key, &key, ty, line));
            decl.methods.push(name.clone());
        }
        self.annotations(node, &key);

        // Receiver-typing inputs: the declared return (what a static
        // factory hands its caller) and the annotated parameters.
        if let Some(t) = node.child_by_field_name("type")
            && let Some(written) = self.type_name(t)
        {
            self.facts.returns.push((key.clone(), written));
        }
        if let Some(params) = node.child_by_field_name("parameters") {
            for pnode in params.named_children(&mut params.walk()) {
                if matches!(pnode.kind(), "formal_parameter" | "spread_parameter")
                    && let (Some(t), Some(n)) = (
                        pnode.child_by_field_name("type"),
                        pnode.child_by_field_name("name"),
                    )
                    && let Some(written) = self.type_name(t)
                {
                    let pname = self.text(n);
                    self.hint(&key, pname, written);
                }
            }
        }

        if let Some(body) = node.child_by_field_name("body") {
            self.collect_calls(&key, type_key, type_name, body);
        }
    }

    /// Annotations resolve like any other type reference. `java.lang`'s own
    /// markers (@Override and friends) say nothing about structure and stay
    /// out of the graph.
    fn annotations(&mut self, node: TsNode, target_key: &str) {
        const NOISE: &[&str] = &[
            "Override",
            "Deprecated",
            "SuppressWarnings",
            "FunctionalInterface",
            "SafeVarargs",
        ];
        let Some(modifiers) = node
            .named_children(&mut node.walk())
            .find(|c| c.kind() == "modifiers")
        else {
            return;
        };
        for m in modifiers.named_children(&mut modifiers.walk()) {
            if m.kind() == "marker_annotation" || m.kind() == "annotation" {
                let Some(name) = m.child_by_field_name("name") else {
                    continue;
                };
                let written = self.text(name);
                if NOISE.contains(&written.as_str()) {
                    continue;
                }
                self.facts.clauses.push((
                    target_key.to_string(),
                    written,
                    "ANNOTATED_BY".into(),
                    self.line(m),
                ));
            }
        }
    }

    /// Records every call a body makes.
    fn collect_calls(&mut self, caller: &str, type_key: &str, type_name: &str, body: TsNode) {
        let mut stack = vec![body];
        while let Some(node) = stack.pop() {
            match node.kind() {
                "method_invocation" => {
                    let line = self.line(node);
                    let name = node
                        .child_by_field_name("name")
                        .map(|n| self.text(n))
                        .unwrap_or_default();
                    if !name.is_empty() {
                        let kind = match node.child_by_field_name("object") {
                            None => Some(CallKind::Own(name)),
                            Some(obj) => match obj.kind() {
                                "this" => Some(CallKind::Own(name)),
                                "super" => Some(CallKind::Super(name)),
                                "identifier" | "scoped_identifier" => {
                                    let written = self.text(obj);
                                    // A capitalized final segment is a type
                                    // reference, written down; a lowercase
                                    // bare identifier is a value the source
                                    // may have typed.
                                    let last = written.rsplit('.').next().unwrap_or(&written);
                                    if last.chars().next().is_some_and(char::is_uppercase) {
                                        Some(CallKind::Static {
                                            type_ref: written,
                                            method: name,
                                        })
                                    } else if obj.kind() == "identifier" {
                                        Some(CallKind::Receiver {
                                            name: written,
                                            method: name,
                                        })
                                    } else {
                                        self.facts.opaque += 1;
                                        None
                                    }
                                }
                                "field_access" => {
                                    let inner = obj.child_by_field_name("object");
                                    let field =
                                        obj.child_by_field_name("field").map(|n| self.text(n));
                                    match (inner.map(|o| o.kind()), field) {
                                        (Some("this"), Some(field)) => Some(CallKind::SelfField {
                                            field,
                                            method: name,
                                        }),
                                        (Some("identifier"), Some(field)) => {
                                            let objname = self.text(inner.unwrap());
                                            if objname
                                                .chars()
                                                .next()
                                                .is_some_and(char::is_uppercase)
                                            {
                                                // `System.out.…` — a static
                                                // field of a type this tree
                                                // may not hold; a checker's
                                                // business.
                                                self.facts.opaque += 1;
                                                None
                                            } else {
                                                Some(CallKind::FieldChain {
                                                    obj: objname,
                                                    field,
                                                    method: name,
                                                })
                                            }
                                        }
                                        _ => {
                                            self.facts.opaque += 1;
                                            None
                                        }
                                    }
                                }
                                // `Foo.getInstance().bar()` — the factory's
                                // declared return types the chain.
                                "method_invocation" => {
                                    let first =
                                        obj.child_by_field_name("name").map(|n| self.text(n));
                                    let base = obj.child_by_field_name("object");
                                    match (base, first) {
                                        (Some(b), Some(first))
                                            if matches!(
                                                b.kind(),
                                                "identifier" | "scoped_identifier"
                                            ) && self
                                                .text(b)
                                                .rsplit('.')
                                                .next()
                                                .unwrap_or_default()
                                                .chars()
                                                .next()
                                                .is_some_and(char::is_uppercase) =>
                                        {
                                            Some(CallKind::StaticChain {
                                                type_ref: self.text(b),
                                                first,
                                                method: name,
                                            })
                                        }
                                        _ => {
                                            self.facts.opaque += 1;
                                            None
                                        }
                                    }
                                }
                                _ => {
                                    self.facts.opaque += 1;
                                    None
                                }
                            },
                        };
                        if let Some(kind) = kind {
                            self.facts.calls.push(Call {
                                caller: caller.to_string(),
                                in_type: type_key.to_string(),
                                kind,
                                line,
                            });
                        }
                    }
                }
                "object_creation_expression" => {
                    if let Some(t) = node.child_by_field_name("type")
                        && let Some(written) = self.type_name(t)
                    {
                        self.facts.calls.push(Call {
                            caller: caller.to_string(),
                            in_type: type_key.to_string(),
                            kind: CallKind::New(written),
                            line: self.line(node),
                        });
                    }
                }
                // A declared local states its type; `var` states it through
                // a `new` initializer.
                "local_variable_declaration" => {
                    let declared = node.child_by_field_name("type").and_then(|t| {
                        if self.text(t) == "var" {
                            None
                        } else {
                            self.type_name(t)
                        }
                    });
                    for d in node.named_children(&mut node.walk()) {
                        if d.kind() != "variable_declarator" {
                            continue;
                        }
                        let Some(n) = d.child_by_field_name("name") else {
                            continue;
                        };
                        let written = declared.clone().or_else(|| {
                            d.child_by_field_name("value").and_then(|v| {
                                (v.kind() == "object_creation_expression")
                                    .then(|| {
                                        v.child_by_field_name("type")
                                            .and_then(|t| self.type_name(t))
                                    })
                                    .flatten()
                            })
                        });
                        if let Some(written) = written {
                            let vname = self.text(n);
                            self.hint(caller, vname, written);
                        }
                    }
                }
                // A local class's calls still belong to the method a reader
                // would look at; its declaration is beside the point here.
                _ => {}
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                stack.push(child);
            }
        }
        let _ = type_name;
    }

    /// The name a type expression writes, generics stripped down to what
    /// they subscript: `Map<K, V>` names `Map`, `com.acme.Box<T>` names
    /// `com.acme.Box`.
    fn type_name(&self, node: TsNode) -> Option<String> {
        match node.kind() {
            "type_identifier" => Some(self.text(node)),
            "scoped_type_identifier" | "scoped_identifier" => Some(self.text(node)),
            "generic_type" => node
                .named_children(&mut node.walk())
                .find(|c| {
                    matches!(
                        c.kind(),
                        "type_identifier" | "scoped_type_identifier" | "scoped_identifier"
                    )
                })
                .map(|c| self.text(c)),
            _ => None,
        }
    }

    fn visibility(&self, node: TsNode) -> Option<String> {
        let modifiers = node
            .named_children(&mut node.walk())
            .find(|c| c.kind() == "modifiers")?;
        let text = self.text(modifiers);
        for vis in ["public", "protected", "private"] {
            if text.split_whitespace().any(|w| w == vis) {
                return Some(vis.to_string());
            }
        }
        None
    }

    /// The javadoc immediately above a declaration — `/** … */`, cleaned of
    /// its asterisk gutter. A `//` line is not documentation.
    fn javadoc_before(&self, node: TsNode) -> Option<String> {
        let prev = node.prev_sibling()?;
        if prev.kind() != "block_comment" {
            return None;
        }
        let text = self.text(prev);
        if !text.starts_with("/**") {
            return None;
        }
        let cleaned = text
            .trim_start_matches("/**")
            .trim_end_matches("*/")
            .lines()
            .map(|l| l.trim().trim_start_matches('*').trim())
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string();
        (!cleaned.is_empty()).then_some(cleaned)
    }

    fn node_text_contains(&self, node: TsNode, word: &str) -> bool {
        let mut cursor = node.walk();
        node.children(&mut cursor).any(|c| self.text(c) == word)
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
