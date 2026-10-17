//! The per-file walk: one C# source into the facts it states, and nothing
//! it does not.

use crate::{
    Call, CallKind, Clause, FileFacts, Hint, INJECT_ATTRIBUTES, Import, Injection, Node, Props,
    Registration, TypeDecl, TypeRef, edge_at,
};
use dr_strange_ext::route;
use serde_json::Value;
use tree_sitter::Node as TsNode;

/// The type a member walk sits in.
struct TypeCtx<'a> {
    key: &'a str,
    /// Its simple name, which a constructor repeats.
    name: &'a str,
    /// Its label — an interface demands its members where a class contains
    /// them.
    label: &'a str,
}

/// An attribute as written on a declaration.
struct Attr {
    written: String,
    args: Option<String>,
    line: u64,
}

/// The type declarations this plugin reads.
const TYPE_DECLS: &[&str] = &[
    "class_declaration",
    "interface_declaration",
    "struct_declaration",
    "record_declaration",
    "record_struct_declaration",
    "enum_declaration",
];

/// Every node kind that stands for a named type in a type position.
const NAMED_TYPES: &[&str] = &["identifier", "qualified_name", "generic_name"];

/// The attributes that say nothing about structure: compiler directives and
/// debugger bookkeeping.
const NOISE_ATTRIBUTES: &[&str] = &[
    "CompilerGenerated",
    "DebuggerStepThrough",
    "DebuggerDisplay",
    "DebuggerNonUserCode",
    "DebuggerBrowsable",
    "MethodImpl",
    "SuppressMessage",
];

/// Walks one parsed file, writing what it reads into [`FileFacts`].
pub struct Walker<'a> {
    facts: &'a mut FileFacts,
    src: &'a str,
    include_source: bool,
}

impl<'a> Walker<'a> {
    pub fn new(facts: &'a mut FileFacts, src: &'a str, include_source: bool) -> Self {
        Self {
            facts,
            src,
            include_source,
        }
    }

    /// The file: its `using` directives first, since every reference reads
    /// through them, then its namespaces and types.
    pub fn compilation_unit(&mut self, root: TsNode) {
        let children = named_children(root);
        for node in children.iter().filter(|c| c.kind() == "using_directive") {
            self.using_directive(*node);
        }
        self.declarations(&children, None, "");
    }

    /// One `using`. A plain one imports a *namespace*, which is a wildcard;
    /// an alias and a `using static` bind one name each; a `global using` is
    /// every file's.
    fn using_directive(&mut self, node: TsNode) {
        let children = named_children(node);
        let Some(target) = children
            .iter()
            .rev()
            .find(|c| NAMED_TYPES.contains(&c.kind()))
            .map(|c| self.text(*c))
        else {
            return;
        };
        let line = self.line(node);
        self.facts.import_lines.push((target.clone(), line));
        let is_static = self.node_text_contains(node, "static");
        // `using X = A.B.C` names the alias first; a plain or static one
        // writes only the target.
        let alias = node
            .child_by_field_name("name")
            .map(|n| self.text(n))
            .filter(|a| *a != target);
        match (alias, is_static) {
            (Some(local), _) => self.facts.imports.push(Import {
                local,
                target,
                is_static: false,
            }),
            (None, true) => self.facts.imports.push(Import {
                local: String::new(),
                target,
                is_static: true,
            }),
            (None, false) => {
                if self.node_text_contains(node, "global") {
                    self.facts.globals.push(target.clone());
                }
                self.facts.wildcards.push(target);
            }
        }
    }

    /// Every declaration at one level: namespaces, which hold types, and
    /// types, which hold members.
    fn declarations(&mut self, nodes: &[TsNode], outer: Option<&str>, parent: &str) {
        for node in nodes {
            match node.kind() {
                // A file-scoped namespace holds every declaration after it,
                // so it takes them and this level is done.
                "file_scoped_namespace_declaration" => {
                    self.namespace(*node, parent);
                    return;
                }
                "namespace_declaration" => self.namespace(*node, parent),
                kind if TYPE_DECLS.contains(&kind) => {
                    if let Some(key) = self.type_decl(*node, outer, parent)
                        && outer.is_none()
                    {
                        self.facts.top_types.push(key);
                    }
                }
                "delegate_declaration" => {
                    if let Some(key) = self.delegate_decl(*node, parent)
                        && outer.is_none()
                    {
                        self.facts.top_types.push(key);
                    }
                }
                _ => {}
            }
        }
    }

    /// One namespace: a node of its own, contained by the one it nests in,
    /// and the container of everything it declares.
    fn namespace(&mut self, node: TsNode, parent: &str) {
        let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
            return;
        };
        let key = if parent.is_empty() {
            name
        } else {
            format!("{parent}.{name}")
        };
        if self.facts.namespace.is_empty() {
            self.facts.namespace = key.clone();
        }
        if !self.facts.namespaces.contains(&key) {
            self.facts.namespaces.push(key.clone());
        }
        // A file-scoped namespace holds the rest of the file; a braced one
        // holds its own body.
        let inner: Vec<TsNode> = match node.child_by_field_name("body") {
            Some(body) => named_children(body),
            None => node
                .next_named_sibling()
                .map(|_| {
                    let mut out = Vec::new();
                    let mut next = node.next_named_sibling();
                    while let Some(n) = next {
                        out.push(n);
                        next = n.next_named_sibling();
                    }
                    out
                })
                .unwrap_or_default(),
        };
        self.declarations(&inner, None, &key);
    }

    /// One type declaration, at namespace level or nested. Returns its key.
    fn type_decl(&mut self, node: TsNode, outer: Option<&str>, parent: &str) -> Option<String> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.text(name_node);
        let key = match outer {
            Some(outer_key) => format!("{outer_key}.{name}"),
            None if parent.is_empty() => name.clone(),
            None => format!("{parent}.{name}"),
        };
        let label = type_label(node.kind());
        let line = self.line(name_node);
        let mut props = self.decl_props(node, line);
        if let Some(mods) = self.declared_modifiers(node) {
            described(
                &mut props,
                "modifiers",
                "the declaration modifiers as written",
                mods,
            );
        }

        let mut decl = TypeDecl {
            key: key.clone(),
            methods: Vec::new(),
            bases: Vec::new(),
            kind: label.to_string(),
        };
        self.base_list(node, &key, line, &mut decl);
        self.attributes(node, &key);

        let mut fields = self.positional_parameters(node, &key, label);
        let variants = self.enum_members(node);
        if let Some(body) = node.child_by_field_name("body") {
            let ctx = TypeCtx {
                key: &key,
                name: &name,
                label,
            };
            self.members(body, &ctx, &mut decl, &mut fields);
        }
        described(
            &mut props,
            "fields",
            "the fields it declares, each with its type as written",
            fields,
        );
        described(
            &mut props,
            "variants",
            "the enum's members, as written",
            variants,
        );

        self.facts.nodes.push(Node {
            key: key.clone(),
            label: label.into(),
            extra_labels: Vec::new(),
            props,
        });
        self.facts
            .edges
            .push(edge_at(parent, &key, "CONTAINS", line));
        self.facts.types.push(decl);
        Some(key)
    }

    /// A `delegate`: a named signature, and a dependency on every type it
    /// writes.
    fn delegate_decl(&mut self, node: TsNode, parent: &str) -> Option<String> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.text(name_node);
        let key = if parent.is_empty() {
            name.clone()
        } else {
            format!("{parent}.{name}")
        };
        let line = self.line(name_node);
        let mut props = self.decl_props(node, line);
        let params = node
            .child_by_field_name("parameters")
            .map(|p| self.text(p))
            .unwrap_or_else(|| "()".to_string());
        let ret = node
            .child_by_field_name("type")
            .map(|t| format!("{} ", self.text(t)))
            .unwrap_or_default();
        props.insert(
            "signature".into(),
            Value::String(format!("{ret}{name}{params}")),
        );
        self.facts.nodes.push(Node {
            key: key.clone(),
            label: "Delegate".into(),
            extra_labels: Vec::new(),
            props,
        });
        self.facts
            .edges
            .push(edge_at(parent, &key, "CONTAINS", line));
        self.attributes(node, &key);
        self.parameter_types(node, &key);
        if let Some(t) = node.child_by_field_name("type") {
            self.type_refs(&key, "return", "", t);
        }
        Some(key)
    }

    /// The base types a declaration writes. C# puts the base class first and
    /// nowhere else, and interfaces after it — the only distinction its
    /// syntax draws, which [`crate::assemble`] corrects from the target's
    /// own declaration where the tree holds it.
    fn base_list(&mut self, node: TsNode, key: &str, line: u64, decl: &mut TypeDecl) {
        let Some(list) = named_children(node)
            .into_iter()
            .find(|c| c.kind() == "base_list")
        else {
            return;
        };
        for (i, base) in named_children(list).into_iter().enumerate() {
            // A record's base is written with its constructor arguments.
            let target = if base.kind() == "primary_constructor_base_type" {
                self.type_node(base)
            } else {
                Some(base)
            };
            let Some(written) = target.and_then(|t| self.type_name(t)) else {
                continue;
            };
            decl.bases.push(written.clone());
            let ty = if i == 0 && !looks_like_interface(&written) {
                "EXTENDS"
            } else {
                "IMPLEMENTS"
            };
            let mut clause = Clause::new(key, written, ty, line);
            clause.first = i == 0;
            self.facts.clauses.push(clause);
        }
    }

    /// A positional parameter list: a record's are its properties, which is
    /// what `record Point(int X, int Y)` declares; a class's C# 12 primary
    /// constructor only captures them.
    fn positional_parameters(&mut self, node: TsNode, key: &str, label: &str) -> Vec<String> {
        let mut fields = Vec::new();
        let Some(params) = named_children(node)
            .into_iter()
            .find(|c| c.kind() == "parameter_list")
        else {
            return fields;
        };
        for p in named_children(params) {
            if p.kind() != "parameter" {
                continue;
            }
            let (Some(name), Some(ty)) = (
                p.child_by_field_name("name").map(|n| self.text(n)),
                p.child_by_field_name("type"),
            ) else {
                continue;
            };
            if label == "Record" {
                if let Some(written) = self.type_name(ty) {
                    self.hint(key, name.clone(), written);
                }
                self.type_refs(key, "property", &name, ty);
                fields.push(format!("{name}: {}", self.text(ty)));
            } else {
                if let Some(written) = self.type_name(ty) {
                    self.hint(key, name.clone(), written);
                }
                // A C# 12 primary constructor has no field behind it: the
                // parameter is the capture, so the dependency is the type's.
                self.type_refs(key, "param", &name, ty);
            }
            let on = self.attrs(p);
            self.injection(key, &name, ty, &on);
        }
        fields
    }

    /// An enum's members, as written.
    fn enum_members(&mut self, node: TsNode) -> Vec<String> {
        node.child_by_field_name("body")
            .map(|body| {
                named_children(body)
                    .into_iter()
                    .filter(|m| m.kind() == "enum_member_declaration")
                    .filter_map(|m| m.child_by_field_name("name").map(|n| self.text(n)))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Every member of a type body.
    fn members(
        &mut self,
        body: TsNode,
        ctx: &TypeCtx<'_>,
        decl: &mut TypeDecl,
        fields: &mut Vec<String>,
    ) {
        for member in named_children(body) {
            match member.kind() {
                "method_declaration" | "operator_declaration" => self.method(member, ctx, decl),
                "constructor_declaration" => self.constructor(member, ctx, decl),
                "property_declaration" | "indexer_declaration" => self.property(member, ctx),
                "field_declaration" | "event_field_declaration" => {
                    fields.extend(self.field(member, ctx))
                }
                kind if TYPE_DECLS.contains(&kind) => {
                    self.type_decl(member, Some(ctx.key), ctx.key);
                }
                "delegate_declaration" => {
                    self.delegate_decl(member, ctx.key);
                }
                _ => {}
            }
        }
    }
}

/// A node's named children, with the children of an `ERROR` node spliced in
/// where the grammar failed to recover: the subtree under it is still what
/// the source wrote, and dropping it would lose real declarations.
fn named_children(node: TsNode) -> Vec<TsNode> {
    let mut out = Vec::new();
    for child in node.named_children(&mut node.walk()) {
        if child.is_error() {
            out.extend(child.named_children(&mut child.walk()));
        } else {
            out.push(child);
        }
    }
    out
}

/// Which label a type declaration's keyword names.
fn type_label(kind: &str) -> &'static str {
    match kind {
        "interface_declaration" => "Interface",
        "struct_declaration" => "Struct",
        "record_declaration" | "record_struct_declaration" => "Record",
        "enum_declaration" => "Enum",
        _ => "Class",
    }
}

/// Does this name read as an interface? `IRepo` is .NET's own convention,
/// followed by the framework and every codebase on it, and it is the only
/// evidence a syntax tree holds for a base type the tree does not declare.
fn looks_like_interface(written: &str) -> bool {
    let simple_name = written.rsplit('.').next().unwrap_or(written);
    let mut cs = simple_name.chars();
    cs.next() == Some('I') && cs.next().is_some_and(char::is_uppercase)
}

/// The last segment of a qualified name.
fn simple(qualified: &str) -> &str {
    qualified.rsplit('.').next().unwrap_or(qualified)
}

/// A described list property, where there is anything to describe.
fn described(props: &mut Props, key: &str, desc: &str, values: Vec<String>) {
    if values.is_empty() {
        return;
    }
    props.insert(
        key.into(),
        serde_json::json!({ "$desc": desc, "$value": values }),
    );
}

/// Does this name read as a type? .NET's conventions are the only evidence a
/// syntax tree holds, and every codebase follows them — a field is
/// `_helper`, a local is `helper`, a type is `Helper`.
fn starts_upper(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}

/// Members: methods, constructors, properties and fields.
impl Walker<'_> {
    /// One method. Overloads share a name and a key: one node, the first
    /// (with the docs) wins, and the member surface keeps a single entry
    /// too.
    fn method(&mut self, node: TsNode, ctx: &TypeCtx<'_>, decl: &mut TypeDecl) {
        let name = match node.child_by_field_name("name") {
            Some(n) => self.text(n),
            // An operator has no name of its own; what it is, is the symbol
            // it overloads.
            None => format!("operator{}", self.operator_symbol(node)),
        };
        let key = format!("{}.{name}", ctx.key);
        let line = node
            .child_by_field_name("name")
            .map(|n| self.line(n))
            .unwrap_or_else(|| self.line(node));
        if !self.facts.nodes.iter().any(|n| n.key == key) {
            let props = self.member_props(node, &name, line);
            self.facts.nodes.push(Node {
                key: key.clone(),
                label: "Method".into(),
                extra_labels: Vec::new(),
                props,
            });
            // An interface demands its members (HAS_METHOD, the family's
            // trait convention); a class contains its own.
            let ty = if ctx.label == "Interface" {
                "HAS_METHOD"
            } else {
                "CONTAINS"
            };
            self.facts.edges.push(edge_at(ctx.key, &key, ty, line));
            decl.methods.push(name);
        }
        self.attributes(node, &key);
        self.returns(node, &key);
        self.parameter_types(node, &key);
        self.body(node, &key, ctx.key);
    }

    /// A constructor, under the type's own simple name — where Java's and
    /// Kotlin's are, so a reader finds it in the same place.
    fn constructor(&mut self, node: TsNode, ctx: &TypeCtx<'_>, decl: &mut TypeDecl) {
        let key = format!("{}.{}", ctx.key, ctx.name);
        let line = self.line(node);
        if !self.facts.nodes.iter().any(|n| n.key == key) {
            let props = self.member_props(node, ctx.name, line);
            self.facts.nodes.push(Node {
                key: key.clone(),
                label: "Method".into(),
                extra_labels: Vec::new(),
                props,
            });
            self.facts
                .edges
                .push(edge_at(ctx.key, &key, "CONTAINS", line));
            decl.methods.push(ctx.name.to_string());
        }
        self.attributes(node, &key);
        self.parameter_types(node, &key);
        // Constructor injection is the only kind ASP.NET Core does, so every
        // constructor parameter is a candidate; which types the container
        // actually builds is known once every registration is in.
        if let Some(params) = node.child_by_field_name("parameters") {
            for p in named_children(params) {
                let (Some(name), Some(ty)) = (
                    p.child_by_field_name("name").map(|n| self.text(n)),
                    p.child_by_field_name("type"),
                ) else {
                    continue;
                };
                let on = self.attrs(p);
                self.injection(ctx.key, &name, ty, &on);
            }
        }
        self.body(node, &key, ctx.key);
    }

    /// A property. C#'s property is the unit a reader navigates to — it
    /// carries a type, and often an accessor with code in it — so it gets a
    /// node of its own; a field, which carries only a value, gets a line in
    /// its type's `fields`.
    fn property(&mut self, node: TsNode, ctx: &TypeCtx<'_>) {
        let (Some(name), Some(ty)) = (
            node.child_by_field_name("name").map(|n| self.text(n)),
            node.child_by_field_name("type"),
        ) else {
            return;
        };
        let key = format!("{}.{name}", ctx.key);
        let line = self.line(node);
        if let Some(written) = self.type_name(ty) {
            self.hint(ctx.key, name.clone(), written);
        }
        self.type_refs(ctx.key, "property", &name, ty);
        if !self.facts.nodes.iter().any(|n| n.key == key) {
            let mut props = self.decl_props(node, line);
            props.insert(
                "signature".into(),
                Value::String(format!("{} {name}", self.text(ty))),
            );
            self.facts.nodes.push(Node {
                key: key.clone(),
                label: "Property".into(),
                extra_labels: Vec::new(),
                props,
            });
            self.facts
                .edges
                .push(edge_at(ctx.key, &key, "CONTAINS", line));
        }
        self.attributes(node, &key);
        // Its accessors and its initialiser hold code, and that code is the
        // property's.
        for part in named_children(node) {
            if matches!(
                part.kind(),
                "accessor_list" | "arrow_expression_clause" | "object_creation_expression"
            ) || part.kind().ends_with("_expression")
            {
                self.collect_calls(&key, ctx.key, part);
            }
        }
    }

    /// A field or an event field: a line in its type's `fields`, a type
    /// binding for the name, and a dependency on what it is typed by.
    fn field(&mut self, node: TsNode, ctx: &TypeCtx<'_>) -> Vec<String> {
        let mut out = Vec::new();
        let Some(decl) = named_children(node)
            .into_iter()
            .find(|c| c.kind() == "variable_declaration")
        else {
            return out;
        };
        let Some(ty) = decl.child_by_field_name("type") else {
            return out;
        };
        let written = self.type_name(ty);
        for d in named_children(decl) {
            let Some(name) = d
                .child_by_field_name("name")
                .map(|n| self.text(n))
                .filter(|_| d.kind() == "variable_declarator")
            else {
                continue;
            };
            if let Some(w) = &written {
                self.hint(ctx.key, name.clone(), w.clone());
            }
            self.type_refs(ctx.key, "field", &name, ty);
            out.push(format!("{name}: {}", self.text(ty)));
        }
        out
    }

    /// What a member says about itself, and what its declared return says
    /// about types.
    fn member_props(&mut self, node: TsNode, name: &str, line: u64) -> Props {
        let params = node
            .child_by_field_name("parameters")
            .map(|p| self.text(p))
            .unwrap_or_else(|| "()".to_string());
        let ret = node
            .child_by_field_name("returns")
            .map(|t| format!("{} ", self.text(t)))
            .unwrap_or_default();
        let mut props = self.decl_props(node, line);
        props.insert(
            "signature".into(),
            Value::String(format!("{ret}{name}{params}")),
        );
        if let Some(mods) = self.declared_modifiers(node) {
            described(
                &mut props,
                "modifiers",
                "the declaration modifiers as written",
                mods,
            );
        }
        props
    }

    /// The declared return: what a static factory hands its caller, and a
    /// dependency of the member either way.
    fn returns(&mut self, node: TsNode, key: &str) {
        let Some(t) = node.child_by_field_name("returns") else {
            return;
        };
        if let Some(written) = self.type_name(t) {
            self.facts.returns.push((key.to_string(), written));
        }
        self.type_refs(key, "return", "", t);
    }

    /// A member's parameters: each one's declared type binds its name.
    ///
    /// A first parameter marked `this` makes the method an extension, which
    /// C# dispatches on the receiver's *static* type — so the pair of that
    /// type and the name is all a call site needs.
    fn parameter_types(&mut self, node: TsNode, key: &str) {
        let Some(params) = node.child_by_field_name("parameters") else {
            return;
        };
        for (i, p) in named_children(params).into_iter().enumerate() {
            let (Some(name), Some(ty)) = (
                p.child_by_field_name("name").map(|n| self.text(n)),
                p.child_by_field_name("type"),
            ) else {
                continue;
            };
            // The type as written, predefined or not — what extension
            // dispatch matches on, where `type_name` keeps only declarable
            // types and so drops `string`.
            let as_written = self.text(ty).trim().to_string();
            self.facts
                .written_types
                .push((key.to_string(), name.clone(), as_written.clone()));
            if i == 0
                && self.has_this_modifier(p)
                && let Some(simple) = key.rsplit('.').next()
            {
                self.facts
                    .extensions
                    .push((as_written, simple.to_string(), key.to_string()));
            }
            if let Some(written) = self.type_name(ty) {
                self.hint(key, name.clone(), written);
            }
            self.type_refs(key, "param", &name, ty);
        }
    }

    /// Whether a parameter is written `this T value` — the extension marker.
    fn has_this_modifier(&self, param: TsNode) -> bool {
        let mut cursor = param.walk();
        param
            .children(&mut cursor)
            .any(|c| c.kind() == "this" || (c.kind() == "modifier" && self.text(c) == "this"))
    }

    /// A member's body, block or expression alike — `=> expr` is a body, and
    /// the calls in it are the member's.
    fn body(&mut self, node: TsNode, key: &str, in_type: &str) {
        if let Some(body) = node.child_by_field_name("body") {
            self.collect_calls(key, in_type, body);
        }
    }

    /// The symbol an operator overloads, for the name it has instead of one.
    fn operator_symbol(&self, node: TsNode) -> String {
        let mut cursor = node.walk();
        node.children(&mut cursor)
            .find(|c| !c.is_named() && !matches!(self.text(*c).as_str(), "operator" | "(" | ")"))
            .map(|c| self.text(c))
            .unwrap_or_default()
    }

    fn hint(&mut self, caller: &str, name: String, written: String) {
        if written.is_empty()
            || self
                .facts
                .hints
                .iter()
                .any(|h| h.caller == caller && h.name == name)
        {
            return;
        }
        self.facts.hints.push(Hint {
            caller: caller.to_string(),
            name,
            written,
        });
    }
}

/// Attributes, and the injection they are evidence of.
impl Walker<'_> {
    /// Attributes resolve like any other type reference. Compiler
    /// directives and debugger markers say nothing about structure and stay
    /// out of the graph.
    fn attributes(&mut self, node: TsNode, target_key: &str) {
        for a in self.attrs(node) {
            if NOISE_ATTRIBUTES.contains(&bare(&a.written)) {
                continue;
            }
            let mut clause = Clause::new(target_key, a.written, "ANNOTATED_BY", a.line);
            clause.args = a.args;
            self.facts.clauses.push(clause);
        }
    }

    /// The attributes written on a declaration, with their arguments as
    /// written, parentheses stripped. Verbatim: an argument's meaning is the
    /// framework's, not a parser's.
    fn attrs(&self, node: TsNode) -> Vec<Attr> {
        named_children(node)
            .into_iter()
            .filter(|c| c.kind() == "attribute_list")
            .flat_map(|list| named_children(list))
            .filter(|a| a.kind() == "attribute")
            .filter_map(|a| self.attr_of(a))
            .collect()
    }

    fn attr_of(&self, a: TsNode) -> Option<Attr> {
        let written = a.child_by_field_name("name").map(|n| self.text(n))?;
        let args = named_children(a)
            .into_iter()
            .find(|c| c.kind() == "attribute_argument_list")
            .and_then(|n| self.unwrapped(n));
        Some(Attr {
            written,
            args,
            line: self.line(a),
        })
    }

    /// A parenthesised argument list as written, parentheses stripped.
    fn unwrapped(&self, node: TsNode) -> Option<String> {
        let text = self.text(node);
        let inner = text
            .strip_prefix('(')
            .and_then(|t| t.strip_suffix(')'))
            .unwrap_or(&text)
            .trim()
            .to_string();
        (!inner.is_empty()).then_some(inner)
    }

    /// Records one candidate injection point. Every constructor parameter is
    /// one — constructor injection is the only kind ASP.NET Core does — and
    /// which types the container actually builds is known once every
    /// registration is in. `[FromServices]` says so outright.
    fn injection(&mut self, owner: &str, name: &str, ty: TsNode, on: &[Attr]) {
        let attributed = on
            .iter()
            .find(|a| INJECT_ATTRIBUTES.contains(&bare(&a.written)));
        self.facts.injections.push(Injection {
            owner: owner.to_string(),
            name: name.to_string(),
            written: self.type_name(ty).unwrap_or_default(),
            args: self.generic_args(ty),
            kind: if attributed.is_some() {
                "attribute".into()
            } else {
                "constructor".into()
            },
            keyed: attributed.and_then(|a| a.args.clone()),
        });
    }

    /// A generic type's arguments as written — `IEnumerable<IHandler>`
    /// subscripts `IHandler`, which is the service a collection injection
    /// point asks for.
    fn generic_args(&self, node: TsNode) -> Vec<String> {
        if node.kind() != "generic_name" {
            return Vec::new();
        }
        named_children(node)
            .into_iter()
            .find(|c| c.kind() == "type_argument_list")
            .map(|a| {
                named_children(a)
                    .into_iter()
                    .filter_map(|t| self.type_name(t))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Call sites, the services they register, and the types a body states
/// along the way.
impl Walker<'_> {
    /// Records every call a body makes, and every local whose type it
    /// states.
    fn collect_calls(&mut self, caller: &str, in_type: &str, body: TsNode) {
        let mut stack = vec![body];
        while let Some(node) = stack.pop() {
            match node.kind() {
                "invocation_expression" => {
                    self.registration(node);
                    self.route_ref(caller, node);
                    let line = self.line(node);
                    if let Some(kind) = self.call_kind(node) {
                        self.record(caller, in_type, kind, line);
                    }
                }
                "object_creation_expression" => {
                    if let Some(written) = node
                        .child_by_field_name("type")
                        .and_then(|t| self.type_name(t))
                    {
                        let line = self.line(node);
                        self.record(caller, in_type, CallKind::New(written), line);
                    }
                }
                "local_declaration_statement" => self.local_hints(caller, in_type, node),
                "foreach_statement" => self.loop_hint(caller, node),
                _ => {}
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                stack.push(child);
            }
        }
    }

    fn record(&mut self, caller: &str, in_type: &str, kind: CallKind, line: u64) {
        self.facts.calls.push(Call {
            caller: caller.to_string(),
            in_type: in_type.to_string(),
            kind,
            line,
        });
    }

    /// What one call site is.
    fn call_kind(&mut self, node: TsNode) -> Option<CallKind> {
        let callee = node.child_by_field_name("function")?;
        match callee.kind() {
            "identifier" | "generic_name" => Some(CallKind::Own(self.callee_name(callee)?)),
            "member_access_expression" => {
                let recv = callee.child_by_field_name("expression")?;
                let method = callee
                    .child_by_field_name("name")
                    .and_then(|n| self.callee_name(n))?;
                self.access_kind(recv, method)
            }
            // `x?.M()` reaches the same member as `x.M()`; what differs is
            // whether it runs, which is not a fact about the graph.
            "conditional_access_expression" => {
                let recv = callee.child_by_field_name("condition")?;
                let method = named_children(callee)
                    .into_iter()
                    .find(|c| c.kind() == "member_binding_expression")
                    .and_then(|b| b.child_by_field_name("name"))
                    .and_then(|n| self.callee_name(n))?;
                self.access_kind(recv, method)
            }
            _ => {
                self.facts.opaque += 1;
                None
            }
        }
    }

    /// A call through a member access: `x.M()`, `This.M()`, `A.B.C.M()`,
    /// `this.p.M()`, `Foo.Get().M()`.
    fn access_kind(&mut self, recv: TsNode, method: String) -> Option<CallKind> {
        match recv.kind() {
            "this" => Some(CallKind::Own(method)),
            "base" => Some(CallKind::Super(method)),
            "invocation_expression" => self.chain_kind(recv, method),
            "generic_name" => self
                .type_name(recv)
                .map(|type_ref| CallKind::Static { type_ref, method }),
            _ => match self.dotted(recv) {
                Some(parts) => self.receiver_kind(&parts, method),
                None => {
                    self.facts.opaque += 1;
                    None
                }
            },
        }
    }

    /// Which receiver shape a dotted chain is. An empty first segment is
    /// `this`.
    fn receiver_kind(&mut self, parts: &[String], method: String) -> Option<CallKind> {
        match parts {
            [one] if one.is_empty() => Some(CallKind::Own(method)),
            [one] if starts_upper(one) => Some(CallKind::Static {
                type_ref: one.clone(),
                method,
            }),
            [one] => Some(CallKind::Receiver {
                name: one.clone(),
                method,
            }),
            [head, field] if head.is_empty() => Some(CallKind::SelfField {
                field: field.clone(),
                method,
            }),
            [obj, field] if !starts_upper(obj) && !starts_upper(field) => {
                Some(CallKind::FieldChain {
                    obj: obj.clone(),
                    field: field.clone(),
                    method,
                })
            }
            rest if rest.last().is_some_and(|l| starts_upper(l)) => Some(CallKind::Static {
                type_ref: rest.join("."),
                method,
            }),
            _ => {
                self.facts.opaque += 1;
                None
            }
        }
    }

    /// `Foo.Create().Bar()` — the factory's declared return types the chain.
    fn chain_kind(&mut self, recv: TsNode, method: String) -> Option<CallKind> {
        let inner = recv
            .child_by_field_name("function")
            .filter(|f| f.kind() == "member_access_expression");
        let parts = inner.and_then(|f| {
            let head = self.dotted(f.child_by_field_name("expression")?)?;
            let first = self.callee_name(f.child_by_field_name("name")?)?;
            Some((head, first))
        });
        match parts {
            Some((head, first)) if head.last().is_some_and(|h| starts_upper(h)) => {
                Some(CallKind::StaticChain {
                    type_ref: head.join("."),
                    first,
                    method,
                })
            }
            _ => {
                self.facts.opaque += 1;
                None
            }
        }
    }

    /// The dotted name a chain of names writes. `this` reads as an empty
    /// first segment; anything that is not a name at all reads as nothing.
    fn dotted(&self, node: TsNode) -> Option<Vec<String>> {
        match node.kind() {
            "identifier" => Some(vec![self.text(node)]),
            "this" => Some(vec![String::new()]),
            "qualified_name" => Some(self.text(node).split('.').map(str::to_string).collect()),
            "member_access_expression" => {
                let name = self.callee_name(node.child_by_field_name("name")?)?;
                let mut out = self.dotted(node.child_by_field_name("expression")?)?;
                out.push(name);
                Some(out)
            }
            _ => None,
        }
    }

    /// What a callee is called, whether or not it writes type arguments.
    fn callee_name(&self, node: TsNode) -> Option<String> {
        match node.kind() {
            "identifier" => Some(self.text(node)),
            "generic_name" => named_children(node)
                .into_iter()
                .find(|c| c.kind() == "identifier")
                .map(|c| self.text(c)),
            _ => None,
        }
    }

    /// A local's type: the one it declares, else the one `new Helper()`
    /// names where it was declared `var`.
    fn local_hints(&mut self, caller: &str, in_type: &str, node: TsNode) {
        let Some(decl) = named_children(node)
            .into_iter()
            .find(|c| c.kind() == "variable_declaration")
        else {
            return;
        };
        let Some(ty) = decl.child_by_field_name("type") else {
            return;
        };
        let declared = self.type_name(ty);
        for d in named_children(decl)
            .into_iter()
            .filter(|d| d.kind() == "variable_declarator")
        {
            let Some(name) = d.child_by_field_name("name").map(|n| self.text(n)) else {
                continue;
            };
            let value = named_children(d).into_iter().find(|c| {
                matches!(
                    c.kind(),
                    "object_creation_expression" | "implicit_object_creation_expression"
                )
            });
            // `Helper h = new();` constructs what it declares, and says so
            // nowhere else.
            if let (Some(v), Some(written)) = (value, declared.clone())
                && v.kind() == "implicit_object_creation_expression"
            {
                let line = self.line(v);
                self.record(caller, in_type, CallKind::New(written), line);
            }
            let written = declared.clone().or_else(|| {
                value
                    .and_then(|v| v.child_by_field_name("type"))
                    .and_then(|t| self.type_name(t))
            });
            if let Some(written) = written {
                self.hint(caller, name, written);
            }
        }
    }

    /// `foreach (IHandler h in handlers)` states `h`'s type as plainly as a
    /// local declaration does; `var h` states nothing.
    fn loop_hint(&mut self, caller: &str, node: TsNode) {
        let (Some(ty), Some(name)) = (
            node.child_by_field_name("type"),
            node.child_by_field_name("left").map(|n| self.text(n)),
        ) else {
            return;
        };
        if let Some(written) = self.type_name(ty) {
            self.hint(caller, name, written);
        }
    }

    /// One service registration, where a call writes one.
    ///
    /// `services.AddScoped<IRepo, JpaRepo>()` is not configuration a parser
    /// has to guess at: it is a call with both types written down in it, and
    /// reading it is reading the source.
    fn registration(&mut self, node: TsNode) {
        let Some(callee) = node.child_by_field_name("function") else {
            return;
        };
        let generic = match callee.kind() {
            "generic_name" => Some(callee),
            "member_access_expression" => callee
                .child_by_field_name("name")
                .filter(|n| n.kind() == "generic_name"),
            _ => None,
        };
        let Some(generic) = generic else { return };
        let Some(name) = self.callee_name(generic) else {
            return;
        };
        if !registers(&name) {
            return;
        }
        let types: Vec<String> = named_children(generic)
            .into_iter()
            .find(|c| c.kind() == "type_argument_list")
            .map(|l| {
                named_children(l)
                    .into_iter()
                    .filter_map(|t| self.type_name(t))
                    .collect()
            })
            .unwrap_or_default();
        let key = name
            .contains("Keyed")
            .then(|| self.first_literal(node))
            .flatten();
        let (service, implementation) = match types.as_slice() {
            // A self-registration: the container builds the type itself.
            [one] => (one.clone(), one.clone()),
            [service, implementation] => (service.clone(), implementation.clone()),
            _ => return,
        };
        self.facts.registrations.push(Registration {
            service,
            implementation,
            key,
        });
    }

    /// The first string literal a call's arguments hold, as written.
    fn first_literal(&self, node: TsNode) -> Option<String> {
        let args = node.child_by_field_name("arguments")?;
        named_children(args)
            .into_iter()
            .find_map(|arg| self.literal_text(arg))
    }

    /// A call whose argument is shaped like a path is asking for a route —
    /// whatever the callee is named.
    ///
    /// Recognising the *argument* rather than a list of blessed clients is
    /// what makes a project-local wrapper visible: `CallBridge("/bridge")`
    /// names no library this reader could have been taught. The cost is that
    /// a resource path can slip through; it will match no declaration, and
    /// the account says how many did.
    fn route_ref(&mut self, caller: &str, node: TsNode) {
        let verb = node
            .child_by_field_name("function")
            .and_then(|f| self.verb_of(f));
        let Some(written) = self
            .first_literal(node)
            .and_then(|text| as_path(&text, verb.is_some()))
        else {
            return;
        };
        self.facts.requests.push(route::RouteRef {
            symbol: caller.to_string(),
            verb,
            written,
            line: self.line(node),
        });
    }

    /// The verb a callee's own name carries — `GetAsync`, `PostAsJsonAsync`,
    /// `DeleteAsync` — wherever the client puts the verb word.
    fn verb_of(&self, callee: TsNode) -> Option<route::Verb> {
        let name = match callee.kind() {
            "identifier" | "generic_name" => self.callee_name(callee)?,
            "member_access_expression" | "conditional_access_expression" => named_children(callee)
                .into_iter()
                .rev()
                .find_map(|c| match c.kind() {
                    "member_binding_expression" => c
                        .child_by_field_name("name")
                        .and_then(|n| self.callee_name(n)),
                    _ => self.callee_name(c),
                })?,
            _ => return None,
        };
        route::Verb::from_name(&name)
    }

    /// The text a string literal argument holds, quotes stripped. An
    /// interpolated string is a different node and states nothing a parser
    /// is certain of.
    fn literal_text(&self, arg: TsNode) -> Option<String> {
        let lit = if arg.kind() == "string_literal" {
            arg
        } else {
            named_children(arg)
                .into_iter()
                .find(|c| c.kind() == "string_literal")?
        };
        let text = self.text(lit);
        let inner = text
            .trim_start_matches('@')
            .strip_prefix('"')
            .and_then(|t| t.strip_suffix('"'))?;
        (!inner.is_empty()).then(|| inner.to_string())
    }
}

/// Does this call register a service? `AddScoped`, `AddSingleton`,
/// `AddTransient`, and their `Try` and `Keyed` spellings — the four words
/// the container's own API is built from.
fn registers(name: &str) -> bool {
    let rest = name.strip_prefix("Try").unwrap_or(name);
    let Some(rest) = rest.strip_prefix("Add") else {
        return false;
    };
    let rest = rest.strip_prefix("Keyed").unwrap_or(rest);
    matches!(rest, "Singleton" | "Scoped" | "Transient")
}

/// The path a literal states, if it states one.
///
/// ASP.NET writes its routes without a leading slash — `[HttpGet("items")]`,
/// `GetAsync("api/orders")` — so a slashless literal counts where the
/// attribute or the callee has already named a verb or a route. A
/// `"application/json"` beside a `[Produces]` names neither and stays prose.
pub(crate) fn as_path(text: &str, routing: bool) -> Option<String> {
    if route::looks_like_path(text) {
        return Some(text.to_string());
    }
    if !routing || text.contains(char::is_whitespace) {
        return None;
    }
    let slashed = format!("/{text}");
    route::looks_like_path(&slashed).then_some(slashed)
}

/// An attribute's name without the suffix C# lets the source omit:
/// `[ApiController]` and `[ApiControllerAttribute]` are one attribute.
pub(crate) fn bare(written: &str) -> &str {
    let last = simple(written);
    last.strip_suffix("Attribute").unwrap_or(last)
}

/// Types, properties and text.
impl Walker<'_> {
    /// The type a declaration writes down, where it writes one.
    fn type_node<'t>(&self, node: TsNode<'t>) -> Option<TsNode<'t>> {
        node.child_by_field_name("type")
            .or_else(|| named_children(node).into_iter().find(|c| is_type(*c)))
    }

    /// The name a type expression writes, generics stripped down to what
    /// they subscript: `List<T>` names `List`, `Acme.Box<T>` names
    /// `Acme.Box`, `Helper?` and `Helper[]` name `Helper`.
    ///
    /// A predefined type (`int`, `string`, `void`) names nothing to resolve:
    /// it is the language's own, never this tree's.
    fn type_name(&self, node: TsNode) -> Option<String> {
        match node.kind() {
            "identifier" | "qualified_name" => Some(self.text(node)),
            "generic_name" => named_children(node)
                .into_iter()
                .find(|c| c.kind() == "identifier")
                .map(|c| self.text(c)),
            "nullable_type" | "array_type" | "pointer_type" | "ref_type" => {
                self.type_node(node).and_then(|t| self.type_name(t))
            }
            _ => None,
        }
    }

    /// Every named type a type expression mentions, outermost first:
    /// `Dictionary<string, List<Job>>` yields `Dictionary`, `List`, `Job`.
    ///
    /// Where [`Walker::type_name`] stops at the head — a receiver's members
    /// are the head's, not its arguments' — a *dependency* does not: a field
    /// of type `List<Job>` depends on `Job`, and that is the fact
    /// `USES_TYPE` records.
    fn type_names(&self, node: TsNode, out: &mut Vec<String>) {
        match node.kind() {
            // A name is a leaf: `A.B.C` is one type, and its parts are not
            // three more.
            "identifier" | "qualified_name" => out.push(self.text(node)),
            "generic_name" => {
                if let Some(head) = self.type_name(node) {
                    out.push(head);
                }
                for a in named_children(node)
                    .into_iter()
                    .filter(|c| c.kind() == "type_argument_list")
                {
                    for t in named_children(a) {
                        self.type_names(t, out);
                    }
                }
            }
            "predefined_type" | "implicit_type" => {}
            _ => {
                for c in named_children(node) {
                    self.type_names(c, out);
                }
            }
        }
    }

    /// Record every named type `node` mentions as one type position of
    /// `owner`.
    fn type_refs(&mut self, owner: &str, role: &str, name: &str, node: TsNode) {
        let mut names = Vec::new();
        self.type_names(node, &mut names);
        names.sort();
        names.dedup();
        for written in names {
            self.facts.type_refs.push(TypeRef {
                owner: owner.to_string(),
                role: role.to_string(),
                name: name.to_string(),
                written,
            });
        }
    }

    /// What every declaration says about itself: its documentation, its
    /// visibility where it writes one, and where it begins and ends.
    fn decl_props(&self, node: TsNode, line: u64) -> Props {
        let mut props = Props::new();
        if let Some(doc) = self.doc_before(node) {
            props.insert("doc_comment".into(), Value::String(doc));
        }
        if let Some(vis) = self.visibility(node) {
            props.insert("visibility".into(), Value::String(vis));
        }
        props.insert("line".into(), Value::from(line));
        props.insert("end_line".into(), Value::from(self.end_line(node)));
        if self.include_source {
            self.add_source(&mut props, node);
        }
        props
    }

    /// The modifiers a declaration writes, visibility excepted — that has a
    /// property of its own.
    fn declared_modifiers(&self, node: TsNode) -> Option<Vec<String>> {
        let out: Vec<String> = named_children(node)
            .into_iter()
            .filter(|c| c.kind() == "modifier")
            .map(|c| self.text(c))
            .filter(|m| !is_visibility(m))
            .collect();
        (!out.is_empty()).then_some(out)
    }

    /// The access the source writes. C# spells one access with two words
    /// (`protected internal`), and this reports them as written.
    fn visibility(&self, node: TsNode) -> Option<String> {
        let words: Vec<String> = named_children(node)
            .into_iter()
            .filter(|c| c.kind() == "modifier")
            .map(|c| self.text(c))
            .filter(|m| is_visibility(m))
            .collect();
        (!words.is_empty()).then(|| words.join(" "))
    }

    /// The XML documentation immediately above a declaration — the `///`
    /// lines, their markup dropped and their text kept. A `//` line is not
    /// documentation.
    fn doc_before(&self, node: TsNode) -> Option<String> {
        let mut lines: Vec<String> = Vec::new();
        let mut prev = node.prev_sibling();
        while let Some(p) = prev {
            if p.kind() != "comment" {
                break;
            }
            let text = self.text(p);
            let Some(body) = text.strip_prefix("///") else {
                break;
            };
            lines.push(strip_markup(body.trim()));
            prev = p.prev_sibling();
        }
        lines.reverse();
        let cleaned = lines
            .into_iter()
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
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

    /// Where a declaration stops. With [`Walker::line`] it says how big a
    /// thing is without opening it, and lets `snippet` read exactly the
    /// declaration instead of guessing a fixed number of lines after its
    /// first.
    fn end_line(&self, node: TsNode) -> u64 {
        node.end_position().row as u64 + 1
    }
}

/// Does this node stand for a type at all?
fn is_type(node: TsNode) -> bool {
    NAMED_TYPES.contains(&node.kind())
        || matches!(
            node.kind(),
            "nullable_type" | "array_type" | "pointer_type" | "ref_type" | "predefined_type"
        )
}

fn is_visibility(word: &str) -> bool {
    matches!(word, "public" | "private" | "protected" | "internal")
}

/// One documentation line with its XML markup dropped and its text kept.
/// `<summary>` and `</param>` are a doc tool's structure, not prose; the
/// words between them are what a reader wanted.
fn strip_markup(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut depth = 0usize;
    for c in line.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.trim().to_string()
}
