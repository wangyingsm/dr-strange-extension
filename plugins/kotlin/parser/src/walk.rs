//! The per-file walk: one Kotlin source into the facts it states, and
//! nothing it does not.

use crate::{
    Call, CallKind, Clause, FileFacts, Hint, INJECT_ANNOTATIONS, Import, Injection, Node, Props,
    STEREOTYPES, TypeDecl, TypeRef, edge_at,
};
use dr_strange_ext::route;
use serde_json::Value;
use tree_sitter::Node as TsNode;

/// The type a member walk sits in.
struct TypeCtx<'a> {
    key: &'a str,
    /// Its simple name, which a secondary constructor's node repeats.
    name: &'a str,
    /// Its label — an interface demands its functions where a class
    /// contains them.
    label: &'a str,
    /// Spring injects this type's only constructor: it is a stereotyped bean
    /// and declares exactly one. In Kotlin that is the primary constructor,
    /// and it is how nearly every Kotlin bean is written.
    inject_ctor: bool,
}

/// An annotation as written on a declaration.
struct Anno {
    written: String,
    args: Option<String>,
    line: u64,
}

/// Every node kind that stands for a type in a type position.
const TYPE_KINDS: &[&str] = &[
    "user_type",
    "nullable_type",
    "function_type",
    "parenthesized_type",
    "definitely_non_nullable_type",
    "dynamic_type",
];

/// The annotations that say nothing about structure: compiler directives and
/// JVM interop bookkeeping.
const NOISE_ANNOTATIONS: &[&str] = &[
    "Override",
    "Deprecated",
    "Suppress",
    "SuppressWarnings",
    "OptIn",
    "JvmStatic",
    "JvmField",
    "JvmName",
    "JvmOverloads",
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

    /// The file: its package first, since every key hangs off it, then its
    /// imports, then every top-level declaration.
    pub fn source_file(&mut self, root: TsNode) {
        let children = named_children(root);
        for node in &children {
            if node.kind() == "package_header"
                && let Some(name) = named_children(*node)
                    .into_iter()
                    .find(|c| c.kind() == "qualified_identifier")
            {
                self.facts.package = self.text(name);
            }
        }
        for node in &children {
            if node.kind() == "import" {
                self.import(*node);
            }
        }
        let package = self.facts.package.clone();
        for node in &children {
            if let Some(key) = self.declaration(*node, None, &package) {
                self.facts.top_types.push(key);
            }
        }
    }

    /// One `import`. Kotlin binds the last segment, or the alias where one is
    /// written; a lowercase last segment imports a member rather than a type.
    fn import(&mut self, node: TsNode) {
        let children = named_children(node);
        let Some(target) = children
            .iter()
            .find(|c| c.kind() == "qualified_identifier")
            .map(|c| self.text(*c))
        else {
            return;
        };
        let line = self.line(node);
        if self.node_text_contains(node, "*") {
            self.facts.wildcards.push(target.clone());
            self.facts.import_lines.push((format!("{target}.*"), line));
            return;
        }
        self.facts.import_lines.push((target.clone(), line));
        let last = target.rsplit('.').next().unwrap_or(&target).to_string();
        // `as` renames what the file calls it; the target is unchanged.
        let alias = children
            .iter()
            .rev()
            .find(|c| c.kind() == "identifier")
            .map(|c| self.text(*c))
            .filter(|a| *a != last);
        let is_member = last.chars().next().is_some_and(char::is_lowercase);
        self.facts.imports.push(Import {
            local: alias.unwrap_or(last),
            target,
            is_member,
        });
    }

    /// One top-level or nested declaration. Returns its key where it minted
    /// one.
    fn declaration(&mut self, node: TsNode, outer: Option<&str>, parent: &str) -> Option<String> {
        match node.kind() {
            "class_declaration" | "object_declaration" | "companion_object" => {
                self.type_decl(node, outer, parent)
            }
            "type_alias" => self.type_alias(node, parent),
            "function_declaration" if outer.is_none() => {
                self.top_function(node, parent);
                None
            }
            "property_declaration" if outer.is_none() => {
                self.top_property(node, parent);
                None
            }
            _ => None,
        }
    }

    /// A type declaration: `class`, `interface`, `enum class`, `annotation
    /// class`, `object`, `companion object`.
    fn type_decl(&mut self, node: TsNode, outer: Option<&str>, parent: &str) -> Option<String> {
        let label = self.type_label(node);
        let name = self.type_name_of(node, label);
        let key = match outer {
            Some(outer_key) => format!("{outer_key}.{name}"),
            None if parent.is_empty() => name.clone(),
            None => format!("{parent}.{name}"),
        };
        let line = node
            .child_by_field_name("name")
            .map(|n| self.line(n))
            .unwrap_or_else(|| self.line(node));

        let mut props = self.decl_props(node, line);
        if let Some(mods) = self.declared_modifiers(node) {
            props.insert(
                "modifiers".into(),
                serde_json::json!({
                    "$desc": "the declaration modifiers as written",
                    "$value": mods,
                }),
            );
        }

        let mut decl = TypeDecl {
            key: key.clone(),
            methods: Vec::new(),
            extends: Vec::new(),
            implements: Vec::new(),
            kind: label.to_string(),
            companion: None,
        };
        self.supertypes(node, &key, label, line, &mut decl);
        self.annotations(node, &key);

        let inject_ctor = self.injects_constructor(node);
        let mut fields = self.primary_constructor(node, &key, inject_ctor);
        let variants = self.enum_entries(node);
        if let Some(body) = self.body_of(node) {
            let ctx = TypeCtx {
                key: &key,
                name: &name,
                label,
                inject_ctor,
            };
            self.members(body, &ctx, &mut decl, &mut fields);
        }
        described(
            &mut props,
            "fields",
            "the properties it declares, each with its type as written",
            fields,
        );
        described(
            &mut props,
            "variants",
            "the enum's entries, as written",
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

    /// A `typealias`: a name of its own for a type written elsewhere, which
    /// is a dependency on that type and nothing more.
    fn type_alias(&mut self, node: TsNode, parent: &str) -> Option<String> {
        let name_node = node.child_by_field_name("type")?;
        let name = self.text(name_node);
        let key = if parent.is_empty() {
            name.clone()
        } else {
            format!("{parent}.{name}")
        };
        let line = self.line(name_node);
        let mut props = self.decl_props(node, line);
        if let Some(aliased) = named_children(node)
            .into_iter()
            .skip_while(|c| c.id() != name_node.id())
            .find(|c| is_type_node(*c))
        {
            props.insert(
                "signature".into(),
                Value::String(format!("{name} = {}", self.text(aliased))),
            );
            self.type_refs(&key, "alias", &name, aliased);
        }
        self.facts.nodes.push(Node {
            key: key.clone(),
            label: "TypeAlias".into(),
            extra_labels: Vec::new(),
            props,
        });
        self.facts
            .edges
            .push(edge_at(parent, &key, "CONTAINS", line));
        self.annotations(node, &key);
        Some(key)
    }

    /// Which kind of type a declaration is. Kotlin writes the distinction in
    /// keywords rather than in the node kind: `interface` and `object` are
    /// their own things, `enum` and `annotation` are class modifiers.
    fn type_label(&self, node: TsNode) -> &'static str {
        if node.kind() == "object_declaration" || node.kind() == "companion_object" {
            return "Object";
        }
        if self.node_text_contains(node, "interface") {
            return "Interface";
        }
        let mods = self.modifier_words(node);
        if mods.iter().any(|m| m == "enum") {
            return "Enum";
        }
        if mods.iter().any(|m| m == "annotation") {
            return "Annotation";
        }
        "Class"
    }

    /// A declaration's name. An unnamed `companion object` is `Companion`,
    /// which is the name Kotlin itself gives it and the one a call through
    /// the outer type reaches it by.
    fn type_name_of(&self, node: TsNode, label: &str) -> String {
        match node.child_by_field_name("name") {
            Some(n) => self.text(n),
            None if node.kind() == "companion_object" => "Companion".to_string(),
            None => format!("<anonymous {label}>"),
        }
    }

    /// The supertypes a declaration writes. Kotlin's only syntactic
    /// distinction is the constructor invocation: a superclass must be
    /// constructed, an interface cannot be — so `: Base()` extends and
    /// `: Runner` implements, which [`crate::assemble`] corrects from the
    /// target's own declaration where the tree holds it.
    fn supertypes(&mut self, node: TsNode, key: &str, label: &str, line: u64, decl: &mut TypeDecl) {
        let Some(list) = named_children(node)
            .into_iter()
            .find(|c| c.kind() == "delegation_specifiers")
        else {
            return;
        };
        for spec in named_children(list) {
            let invoked = named_children(spec)
                .iter()
                .any(|c| c.kind() == "constructor_invocation");
            let Some(written) = self.first_type_name(spec) else {
                continue;
            };
            let ty = if invoked && label != "Interface" {
                decl.extends.push(written.clone());
                "EXTENDS"
            } else {
                decl.implements.push(written.clone());
                "IMPLEMENTS"
            };
            let mut clause = Clause::new(key, written, ty, line);
            clause.invoked = invoked;
            self.facts.clauses.push(clause);
        }
    }

    /// A primary constructor's parameters. The ones written `val` or `var`
    /// are the type's properties — the shape nearly every Kotlin class uses
    /// — so they count as both.
    fn primary_constructor(&mut self, node: TsNode, key: &str, injects: bool) -> Vec<String> {
        let mut fields = Vec::new();
        let Some(params) = named_children(node)
            .into_iter()
            .find(|c| c.kind() == "primary_constructor")
            .and_then(|pc| {
                named_children(pc)
                    .into_iter()
                    .find(|c| c.kind() == "class_parameters")
            })
        else {
            return fields;
        };
        let ctor_key = format!("{key}.{}", key.rsplit('.').next().unwrap_or(key));
        for p in named_children(params) {
            if p.kind() != "class_parameter" {
                continue;
            }
            let (Some(name), Some(ty)) = (self.decl_name(p), self.type_node(p)) else {
                continue;
            };
            let on = self.annos(p);
            if self.node_text_contains(p, "val") || self.node_text_contains(p, "var") {
                if let Some(written) = self.type_name(ty) {
                    self.hint(key, name.clone(), written);
                }
                self.type_refs(key, "property", &name, ty);
                fields.push(format!("{name}: {}", self.text(ty)));
                self.property_node(p, key, &name, ty);
            } else {
                self.type_refs(&ctor_key, "param", &name, ty);
            }
            // The primary constructor is the one Spring injects in Kotlin,
            // and a `val` parameter is the property it fills.
            if injects {
                let kind = if Self::is_injected(&on) {
                    "annotation"
                } else {
                    "constructor"
                };
                self.injection(key, &name, ty, kind, &on);
            }
        }
        fields
    }

    /// An enum's entries, as written.
    fn enum_entries(&mut self, node: TsNode) -> Vec<String> {
        self.body_of(node)
            .map(|body| {
                named_children(body)
                    .into_iter()
                    .filter(|m| m.kind() == "enum_entry")
                    .filter_map(|m| self.decl_name(m))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Does Spring inject this type's constructor unannotated? It does when
    /// the type is a stereotyped bean with exactly one constructor, primary
    /// or secondary.
    fn injects_constructor(&self, node: TsNode) -> bool {
        let stereotyped = self
            .annos(node)
            .iter()
            .any(|a| STEREOTYPES.contains(&simple(&a.written)));
        let primary = named_children(node)
            .iter()
            .any(|c| c.kind() == "primary_constructor");
        let secondary = self
            .body_of(node)
            .map(|body| {
                named_children(body)
                    .iter()
                    .filter(|m| m.kind() == "secondary_constructor")
                    .count()
            })
            .unwrap_or(0);
        stereotyped && secondary + usize::from(primary) == 1
    }

    /// Every member of a type body, descending into the subtrees the
    /// grammar's error recovery leaves behind (`fun interface`, and a named
    /// companion object holding one member, are both parsed wrongly and both
    /// recover with the declaration intact).
    fn members(
        &mut self,
        body: TsNode,
        ctx: &TypeCtx<'_>,
        decl: &mut TypeDecl,
        fields: &mut Vec<String>,
    ) {
        for member in named_children(body) {
            match member.kind() {
                "function_declaration" => self.function(member, ctx, decl),
                "secondary_constructor" => self.constructor(member, ctx, decl),
                "anonymous_initializer" => self.initializer(member, ctx),
                "property_declaration" => {
                    if let Some(entry) = self.member_property(member, ctx) {
                        fields.push(entry);
                    }
                }
                "companion_object" => {
                    if let Some(k) = self.type_decl(member, Some(ctx.key), ctx.key) {
                        decl.companion = Some(k);
                    }
                }
                "class_declaration" | "object_declaration" | "type_alias" => {
                    self.declaration(member, Some(ctx.key), ctx.key);
                }
                _ => {}
            }
        }
    }
}

/// A node's named children, with the children of an `ERROR` node spliced in
/// where the grammar failed to recover: the subtree under it is still the
/// declaration the source wrote, and dropping it would lose real members.
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

/// Is this the annotation group the grammar detached from the declaration
/// after it?
///
/// `@RestController` above a class is the class's modifier — but with a
/// second declaration later in the file the grammar re-reads the two bare
/// annotations as an expression of their own, and leaves the class without
/// them. A bare expression is legal in neither place this appears, so a node
/// holding nothing but annotations is that and nothing else.
fn detached_annotations(node: TsNode) -> bool {
    if node.kind() != "annotated_expression" {
        return false;
    }
    let kids = named_children(node);
    kids.iter().any(|c| c.kind() == "annotation")
        && kids.iter().all(|c| {
            matches!(
                c.kind(),
                "annotation" | "annotated_expression" | "parenthesized_expression"
            )
        })
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

/// What the type-position helpers recognise as a type.
fn is_type_node(node: TsNode) -> bool {
    TYPE_KINDS.contains(&node.kind())
}

/// Declarations: functions, constructors, initialisers, properties.
impl Walker<'_> {
    /// One function of a type. Overloads share a name and a key: one node,
    /// the first (with the docs) wins, and the member surface keeps a single
    /// entry too.
    fn function(&mut self, node: TsNode, ctx: &TypeCtx<'_>, decl: &mut TypeDecl) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let name = self.text(name_node);
        let key = format!("{}.{name}", ctx.key);
        let line = self.line(name_node);
        let props = self.function_props(node, &name, line, &key);

        if !self.facts.nodes.iter().any(|n| n.key == key) {
            self.facts.nodes.push(Node {
                key: key.clone(),
                label: "Method".into(),
                extra_labels: Vec::new(),
                props,
            });
            // An interface demands its functions (HAS_METHOD, the family's
            // trait convention); a class contains its own.
            let ty = if ctx.label == "Interface" {
                "HAS_METHOD"
            } else {
                "CONTAINS"
            };
            self.facts.edges.push(edge_at(ctx.key, &key, ty, line));
            decl.methods.push(name);
        }
        self.annotations(node, &key);
        self.parameters(node, &key, ctx.key, false);
        self.function_body(node, &key, ctx.key);
    }

    /// A top-level function: the package contains it, since Kotlin's unit of
    /// declaration is not the type.
    fn top_function(&mut self, node: TsNode, package: &str) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let name = self.text(name_node);
        let key = if package.is_empty() {
            name.clone()
        } else {
            format!("{package}.{name}")
        };
        let line = self.line(name_node);
        let props = self.function_props(node, &name, line, &key);
        if !self.facts.nodes.iter().any(|n| n.key == key) {
            self.facts.nodes.push(Node {
                key: key.clone(),
                label: "Function".into(),
                extra_labels: Vec::new(),
                props,
            });
            self.facts
                .edges
                .push(edge_at(package, &key, "CONTAINS", line));
            self.facts.top_functions.push((name, key.clone()));
        }
        self.annotations(node, &key);
        self.parameters(node, &key, package, false);
        self.function_body(node, &key, package);
        self.facts.top_types.push(key);
    }

    /// What a function node says about itself, and what its declared return
    /// and extension receiver say about types.
    fn function_props(&mut self, node: TsNode, name: &str, line: u64, key: &str) -> Props {
        let params = named_children(node)
            .into_iter()
            .find(|c| c.kind() == "function_value_parameters")
            .map(|p| self.text(p))
            .unwrap_or_else(|| "()".to_string());
        let ret = self.return_type(node);
        let mut props = self.decl_props(node, line);
        props.insert(
            "signature".into(),
            Value::String(match &ret {
                Some(t) => format!("{name}{params}: {}", self.text(*t)),
                None => format!("{name}{params}"),
            }),
        );
        if let Some(mods) = self.declared_modifiers(node) {
            described(
                &mut props,
                "modifiers",
                "the declaration modifiers as written",
                mods,
            );
        }
        if let Some(t) = ret
            && let Some(written) = self.type_name(t)
        {
            self.facts.returns.push((key.to_string(), written));
            self.type_refs(key, "return", "", t);
        }
        // An extension receiver is written before the name, and is the type
        // the function extends.
        if let Some(recv) = self.extension_receiver(node) {
            let written = self.text(recv);
            props.insert("receiver".into(), Value::String(written.clone()));
            self.type_refs(key, "receiver", "", recv);
            // Dispatched on the receiver's static type, so the call site can
            // be resolved from the name alone — see `FileFacts::extensions`.
            self.facts
                .extensions
                .push((written, name.to_string(), key.to_string()));
        }
        props
    }

    /// A secondary constructor, under the type's own simple name — the key
    /// Java's constructors take, so a reader finds it in the same place.
    fn constructor(&mut self, node: TsNode, ctx: &TypeCtx<'_>, decl: &mut TypeDecl) {
        let key = format!("{}.{}", ctx.key, ctx.name);
        let line = self.line(node);
        let params = named_children(node)
            .into_iter()
            .find(|c| c.kind() == "function_value_parameters")
            .map(|p| self.text(p))
            .unwrap_or_else(|| "()".to_string());
        let mut props = self.decl_props(node, line);
        props.insert(
            "signature".into(),
            Value::String(format!("constructor{params}")),
        );
        if !self.facts.nodes.iter().any(|n| n.key == key) {
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
        self.annotations(node, &key);
        self.parameters(node, &key, ctx.key, ctx.inject_ctor);
        self.function_body(node, &key, ctx.key);
    }

    /// An `init` block: the code a constructor runs, under a name of its own
    /// so the calls in it belong to something a reader can ask about.
    fn initializer(&mut self, node: TsNode, ctx: &TypeCtx<'_>) {
        let key = format!("{}.init", ctx.key);
        let line = self.line(node);
        if !self.facts.nodes.iter().any(|n| n.key == key) {
            let mut props = self.decl_props(node, line);
            props.insert("signature".into(), Value::String("init".into()));
            self.facts.nodes.push(Node {
                key: key.clone(),
                label: "Method".into(),
                extra_labels: Vec::new(),
                props,
            });
            self.facts
                .edges
                .push(edge_at(ctx.key, &key, "CONTAINS", line));
        }
        if let Some(block) = named_children(node)
            .into_iter()
            .find(|c| c.kind() == "block")
        {
            self.collect_calls(&key, ctx.key, block);
        }
    }

    /// A function's parameters: each one's declared type binds its name, and
    /// in a constructor Spring injects, each one is an injection point.
    fn parameters(&mut self, node: TsNode, key: &str, owner: &str, injects: bool) {
        let Some(params) = named_children(node)
            .into_iter()
            .find(|c| c.kind() == "function_value_parameters")
        else {
            return;
        };
        for p in named_children(params) {
            if p.kind() != "parameter" {
                continue;
            }
            let (Some(name), Some(ty)) = (self.decl_name(p), self.type_node(p)) else {
                continue;
            };
            if let Some(written) = self.type_name(ty) {
                self.hint(key, name.clone(), written);
            }
            self.type_refs(key, "param", &name, ty);
            if injects {
                let on = self.param_annos(p);
                let kind = if Self::is_injected(&on) {
                    "annotation"
                } else {
                    "constructor"
                };
                self.injection(owner, &name, ty, kind, &on);
            }
        }
    }

    /// A member property. Kotlin's property is the unit a reader navigates
    /// to — it carries a type, and often a getter with code in it — so it
    /// gets a node as well as its line in the type's `fields`.
    fn member_property(&mut self, node: TsNode, ctx: &TypeCtx<'_>) -> Option<String> {
        let decl = named_children(node)
            .into_iter()
            .find(|c| c.kind() == "variable_declaration")?;
        let name = self.decl_name(decl)?;
        let ty = self.type_node(decl)?;
        if let Some(written) = self.type_name(ty) {
            self.hint(ctx.key, name.clone(), written);
        }
        self.type_refs(ctx.key, "property", &name, ty);
        let on = self.annos(node);
        if Self::is_injected(&on) {
            self.injection(ctx.key, &name, ty, "annotation", &on);
        }
        self.property_node(node, ctx.key, &name, ty);
        Some(format!("{name}: {}", self.text(ty)))
    }

    /// The node a property gets, and the calls its initialiser, delegate and
    /// accessors make.
    fn property_node(&mut self, node: TsNode, owner: &str, name: &str, ty: TsNode) {
        let key = format!("{owner}.{name}");
        let line = self.line(node);
        if !self.facts.nodes.iter().any(|n| n.key == key) {
            let mut props = self.decl_props(node, line);
            props.insert(
                "signature".into(),
                Value::String(format!("{name}: {}", self.text(ty))),
            );
            if let Some(mods) = self.declared_modifiers(node) {
                described(
                    &mut props,
                    "modifiers",
                    "the declaration modifiers as written",
                    mods,
                );
            }
            self.facts.nodes.push(Node {
                key: key.clone(),
                label: "Property".into(),
                extra_labels: Vec::new(),
                props,
            });
            self.facts
                .edges
                .push(edge_at(owner, &key, "CONTAINS", line));
        }
        self.annotations(node, &key);
        // Its initialiser, its delegate and its accessors all hold code, and
        // that code is the property's.
        for part in named_children(node) {
            if matches!(part.kind(), "variable_declaration" | "modifiers") || is_type_node(part) {
                continue;
            }
            self.collect_calls(&key, owner, part);
        }
    }

    /// A top-level property: `val` is a constant of its package, `var` a
    /// variable of it.
    fn top_property(&mut self, node: TsNode, package: &str) {
        let Some(decl) = named_children(node)
            .into_iter()
            .find(|c| c.kind() == "variable_declaration")
        else {
            return;
        };
        let Some(name) = self.decl_name(decl) else {
            return;
        };
        let key = if package.is_empty() {
            name.clone()
        } else {
            format!("{package}.{name}")
        };
        let line = self.line(node);
        let label = if self.node_text_contains(node, "var") {
            "Var"
        } else {
            "Const"
        };
        if !self.facts.nodes.iter().any(|n| n.key == key) {
            let mut props = self.decl_props(node, line);
            if let Some(ty) = self.type_node(decl) {
                props.insert(
                    "signature".into(),
                    Value::String(format!("{name}: {}", self.text(ty))),
                );
                self.hint(
                    package,
                    name.clone(),
                    self.type_name(ty).unwrap_or_default(),
                );
                self.type_refs(&key, "type", &name, ty);
            }
            self.facts.nodes.push(Node {
                key: key.clone(),
                label: label.into(),
                extra_labels: Vec::new(),
                props,
            });
            self.facts
                .edges
                .push(edge_at(package, &key, "CONTAINS", line));
        }
        self.annotations(node, &key);
        self.facts.top_types.push(key);
    }

    /// A function's body, block or expression alike — `= expr` is a body,
    /// and the calls in it are the function's.
    fn function_body(&mut self, node: TsNode, key: &str, in_type: &str) {
        if let Some(body) = named_children(node)
            .into_iter()
            .find(|c| c.kind() == "function_body")
        {
            self.collect_calls(key, in_type, body);
        }
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

/// Annotations, and the injection they are evidence of.
impl Walker<'_> {
    /// Annotations resolve like any other type reference. Compiler
    /// directives and JVM interop markers say nothing about structure and
    /// stay out of the graph.
    fn annotations(&mut self, node: TsNode, target_key: &str) {
        for a in self.annos(node) {
            if NOISE_ANNOTATIONS.contains(&simple(&a.written)) {
                continue;
            }
            let mut clause = Clause::new(target_key, a.written, "ANNOTATED_BY", a.line);
            clause.args = a.args;
            self.facts.clauses.push(clause);
        }
    }

    /// The annotations written on a declaration, with their arguments as
    /// written, parentheses stripped. Verbatim: an argument's meaning is the
    /// framework's, not a parser's.
    ///
    /// Both places they can land: the declaration's own modifiers, and the
    /// expression the grammar sometimes detaches them into (see
    /// [`detached_annotations`]).
    fn annos(&self, node: TsNode) -> Vec<Anno> {
        let mut out = Vec::new();
        if let Some(group) = node
            .prev_named_sibling()
            .filter(|s| detached_annotations(*s))
        {
            self.detached_annos(group, &mut out);
        }
        if let Some(modifiers) = named_children(node)
            .into_iter()
            .find(|c| c.kind() == "modifiers")
        {
            out.extend(self.annos_in(modifiers));
        }
        out
    }

    /// The annotations in a group the grammar detached, arguments and all:
    /// it nests one annotation per level and leaves each argument list
    /// beside its annotation rather than inside it.
    fn detached_annos(&self, group: TsNode, out: &mut Vec<Anno>) {
        let kids = named_children(group);
        for (i, k) in kids.iter().enumerate() {
            match k.kind() {
                "annotation" => {
                    let Some(mut a) = self.anno_of(*k) else {
                        continue;
                    };
                    if a.args.is_none()
                        && let Some(args) = kids
                            .get(i + 1)
                            .filter(|n| n.kind() == "parenthesized_expression")
                    {
                        a.args = self.unwrapped(*args);
                    }
                    out.push(a);
                }
                "annotated_expression" => self.detached_annos(*k, out),
                _ => {}
            }
        }
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

    /// A function parameter's annotations sit in a `parameter_modifiers`
    /// node *beside* it rather than inside it, which is the one place this
    /// grammar puts a declaration's modifiers somewhere else.
    fn param_annos(&self, p: TsNode) -> Vec<Anno> {
        p.prev_named_sibling()
            .filter(|s| s.kind() == "parameter_modifiers")
            .map(|m| self.annos_in(m))
            .unwrap_or_default()
    }

    fn annos_in(&self, modifiers: TsNode) -> Vec<Anno> {
        named_children(modifiers)
            .into_iter()
            .filter(|m| m.kind() == "annotation")
            .filter_map(|m| self.anno_of(m))
            .collect()
    }

    /// One annotation: `@Foo` writes a type, `@Foo(x)` a constructor
    /// invocation, and a use-site target (`@field:Foo`) changes neither.
    fn anno_of(&self, m: TsNode) -> Option<Anno> {
        let inner = named_children(m)
            .into_iter()
            .find(|c| matches!(c.kind(), "constructor_invocation" | "user_type"))?;
        let written = self.first_type_name(inner)?;
        let args = named_children(inner)
            .into_iter()
            .find(|c| c.kind() == "value_arguments")
            .and_then(|n| self.unwrapped(n));
        Some(Anno {
            written,
            args,
            line: self.line(m),
        })
    }

    /// Records one injection point. `kind` is the evidence the caller holds —
    /// the annotation it read, or the constructor it knows Spring injects —
    /// and `on` is what was annotated, for the bean name a `@Qualifier`
    /// states.
    fn injection(&mut self, owner: &str, name: &str, ty: TsNode, kind: &str, on: &[Anno]) {
        let qualifier = on
            .iter()
            .find(|a| matches!(simple(&a.written), "Qualifier" | "Resource"))
            .and_then(|a| a.args.clone());
        self.facts.injections.push(Injection {
            owner: owner.to_string(),
            name: name.to_string(),
            written: self.type_name(ty).unwrap_or_default(),
            args: self.generic_args(ty),
            kind: kind.to_string(),
            qualifier,
        });
    }

    /// Is one of these the annotation Spring reads as "inject this"?
    fn is_injected(on: &[Anno]) -> bool {
        on.iter()
            .any(|a| INJECT_ANNOTATIONS.contains(&simple(&a.written)))
    }

    /// A generic type's arguments as written — `List<Handler>` subscripts
    /// `Handler`, which is the bean a collection injection point asks for.
    fn generic_args(&self, node: TsNode) -> Vec<String> {
        named_children(node)
            .into_iter()
            .find(|c| c.kind() == "type_arguments")
            .map(|a| {
                named_children(a)
                    .into_iter()
                    .filter_map(|t| self.first_type_name(t))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Call sites, and the types a body states along the way.
impl Walker<'_> {
    /// Records every call a body makes, and every local whose type it
    /// states.
    fn collect_calls(&mut self, caller: &str, in_type: &str, body: TsNode) {
        let mut stack = vec![body];
        while let Some(node) = stack.pop() {
            match node.kind() {
                "call_expression" => {
                    self.route_ref(caller, node);
                    let line = self.line(node);
                    if let Some(kind) = self.call_kind(node) {
                        self.facts.calls.push(Call {
                            caller: caller.to_string(),
                            in_type: in_type.to_string(),
                            kind,
                            line,
                        });
                    }
                }
                "property_declaration" => self.local_hint(caller, node),
                "for_statement" => self.loop_hint(caller, node),
                _ => {}
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                stack.push(child);
            }
        }
    }

    /// What one call site is. Kotlin spells construction as a call, so a
    /// capitalized callee is a constructor and nothing else needs saying.
    fn call_kind(&mut self, node: TsNode) -> Option<CallKind> {
        let callee = named_children(node).into_iter().next()?;
        match callee.kind() {
            "identifier" => {
                let name = self.text(callee);
                Some(if starts_upper(&name) {
                    CallKind::New(name)
                } else {
                    CallKind::Own(name)
                })
            }
            "navigation_expression" => self.nav_kind(callee),
            _ => {
                self.facts.opaque += 1;
                None
            }
        }
    }

    /// A call through a navigation chain: `x.m()`, `This.m()`, `a.b.C.m()`,
    /// `this.p.m()`, `Foo.get().m()`.
    fn nav_kind(&mut self, nav: TsNode) -> Option<CallKind> {
        let kids = named_children(nav);
        let (Some(recv), Some(sel)) = (kids.first().copied(), kids.get(1).copied()) else {
            self.facts.opaque += 1;
            return None;
        };
        if sel.kind() != "identifier" {
            self.facts.opaque += 1;
            return None;
        }
        let method = self.text(sel);
        // `com.acme.Box()`, `Outer.Inner()`: a capitalized selector under a
        // call is a constructor, qualified or nested.
        if starts_upper(&method) {
            return Some(CallKind::New(self.text(nav)));
        }
        if recv.kind() == "super_expression" {
            return Some(CallKind::Super(method));
        }
        if recv.kind() == "call_expression" {
            return self.chain_kind(recv, method);
        }
        let Some(parts) = self.dotted(recv) else {
            self.facts.opaque += 1;
            return None;
        };
        self.receiver_kind(&parts, method)
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

    /// `Foo.create().bar()` — the factory's declared return types the chain.
    fn chain_kind(&mut self, recv: TsNode, method: String) -> Option<CallKind> {
        let inner = named_children(recv).into_iter().next();
        let parts = inner
            .filter(|c| c.kind() == "navigation_expression")
            .and_then(|c| {
                let kids = named_children(c);
                let first = self.dotted(*kids.first()?)?;
                Some((first, self.text(*kids.get(1)?)))
            });
        match parts {
            Some((head, first))
                if head.last().is_some_and(|h| starts_upper(h)) && !starts_upper(&first) =>
            {
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
            "this_expression" => Some(vec![String::new()]),
            "navigation_expression" => {
                let kids = named_children(node);
                let sel = *kids.get(1)?;
                if sel.kind() != "identifier" {
                    return None;
                }
                let mut out = self.dotted(*kids.first()?)?;
                out.push(self.text(sel));
                Some(out)
            }
            _ => None,
        }
    }

    /// A local's type: the one it declares, else the one a constructor call
    /// on the right names, which is what `val x = Helper()` states.
    fn local_hint(&mut self, caller: &str, node: TsNode) {
        let kids = named_children(node);
        let Some(decl) = kids.iter().find(|c| c.kind() == "variable_declaration") else {
            return;
        };
        let Some(name) = self.decl_name(*decl) else {
            return;
        };
        let written = self
            .type_node(*decl)
            .and_then(|t| self.type_name(t))
            .or_else(|| {
                kids.iter()
                    .find(|c| c.kind() == "call_expression")
                    .and_then(|v| self.constructed_type(*v))
            });
        if let Some(written) = written {
            self.hint(caller, name, written);
        }
    }

    /// `for (h: Handler in handlers)` states `h`'s type as plainly as a
    /// local declaration does; `for (h in handlers)` states nothing.
    fn loop_hint(&mut self, caller: &str, node: TsNode) {
        let Some(decl) = named_children(node)
            .into_iter()
            .find(|c| c.kind() == "variable_declaration")
        else {
            return;
        };
        if let (Some(name), Some(written)) = (
            self.decl_name(decl),
            self.type_node(decl).and_then(|t| self.type_name(t)),
        ) {
            self.hint(caller, name, written);
        }
    }

    /// The type a call constructs, where its callee names one.
    fn constructed_type(&self, call: TsNode) -> Option<String> {
        let callee = named_children(call).into_iter().next()?;
        if !matches!(callee.kind(), "identifier" | "navigation_expression") {
            return None;
        }
        let name = self.text(callee);
        starts_upper(simple(&name)).then_some(name)
    }

    /// A call whose argument is shaped like a path is asking for a route —
    /// whatever the function is named.
    ///
    /// Recognising the *argument* rather than a list of blessed clients is
    /// what makes a project-local wrapper visible: `callBridge("/bridge")`
    /// names no library this reader could have been taught. The cost is that
    /// a resource path can slip through; it will match no declaration, and
    /// the account says how many did.
    fn route_ref(&mut self, caller: &str, node: TsNode) {
        let kids = named_children(node);
        let Some(callee) = kids.first().copied() else {
            return;
        };
        let mut verb = self
            .callee_name(callee)
            .and_then(|n| route::Verb::from_name(&n));
        let Some(args) = kids.into_iter().find(|c| c.kind() == "value_arguments") else {
            return;
        };
        let mut written = None;
        for arg in named_children(args) {
            if written.is_none() {
                written = self.path_text(arg);
            }
            // `exchange(url, HttpMethod.POST, …)` states the verb as an enum
            // entry, which is the one shape a function's name hides.
            if verb.is_none() {
                verb = self.enum_verb(arg);
            }
        }
        let Some(written) = written else { return };
        self.facts.requests.push(route::RouteRef {
            symbol: caller.to_string(),
            verb,
            written,
            line: self.line(node),
        });
    }

    /// What a call's callee is called, whatever it is reached through.
    fn callee_name(&self, callee: TsNode) -> Option<String> {
        match callee.kind() {
            "identifier" => Some(self.text(callee)),
            "navigation_expression" => named_children(callee)
                .get(1)
                .filter(|s| s.kind() == "identifier")
                .map(|s| self.text(*s)),
            _ => None,
        }
    }

    /// The verb an argument names as an enum entry.
    fn enum_verb(&self, arg: TsNode) -> Option<route::Verb> {
        let nav = named_children(arg)
            .into_iter()
            .find(|c| c.kind() == "navigation_expression")?;
        let parts = self.dotted(nav)?;
        let [holder, entry] = parts.as_slice() else {
            return None;
        };
        matches!(holder.as_str(), "HttpMethod" | "RequestMethod")
            .then(|| route::Verb::from_name(entry))
            .flatten()
    }

    /// The path a literal argument states, if it states one. A template is
    /// not a literal and states nothing a parser is certain of.
    fn path_text(&self, arg: TsNode) -> Option<String> {
        let lit = named_children(arg)
            .into_iter()
            .find(|c| c.kind() == "string_literal")?;
        let parts = named_children(lit);
        let [content] = parts.as_slice() else {
            return None;
        };
        if content.kind() != "string_content" {
            return None;
        }
        let text = self.text(*content);
        route::looks_like_path(&text).then_some(text)
    }
}

/// Types, properties and text.
impl Walker<'_> {
    /// The type a declaration writes down, where it writes one.
    fn type_node<'t>(&self, node: TsNode<'t>) -> Option<TsNode<'t>> {
        named_children(node).into_iter().find(|c| is_type_node(*c))
    }

    /// A function's declared return: the type written after its parameters.
    fn return_type<'t>(&self, node: TsNode<'t>) -> Option<TsNode<'t>> {
        let kids = named_children(node);
        let after = kids
            .iter()
            .position(|c| c.kind() == "function_value_parameters")?;
        kids.into_iter().skip(after + 1).find(|c| is_type_node(*c))
    }

    /// An extension function's receiver: the type written before its name.
    fn extension_receiver<'t>(&self, node: TsNode<'t>) -> Option<TsNode<'t>> {
        let kids = named_children(node);
        let name = kids.iter().position(|c| c.kind() == "identifier")?;
        kids.into_iter().take(name).find(|c| is_type_node(*c))
    }

    /// The first type a subtree names — a supertype through its constructor
    /// invocation, an annotation through its own.
    fn first_type_name(&self, node: TsNode) -> Option<String> {
        if let Some(name) = self.type_name(node) {
            return Some(name);
        }
        named_children(node)
            .into_iter()
            .filter(|c| c.kind() != "value_arguments")
            .find_map(|c| self.first_type_name(c))
    }

    /// The name a type expression writes, generics stripped down to what
    /// they subscript: `Map<K, V>` names `Map`, `com.acme.Box<T>` names
    /// `com.acme.Box`, `Helper?` names `Helper`.
    fn type_name(&self, node: TsNode) -> Option<String> {
        match node.kind() {
            "user_type" => {
                let head: Vec<String> = named_children(node)
                    .into_iter()
                    .filter(|c| c.kind() == "identifier")
                    .map(|c| self.text(c))
                    .collect();
                (!head.is_empty()).then(|| head.join("."))
            }
            "nullable_type" | "parenthesized_type" | "definitely_non_nullable_type" => {
                self.type_node(node).and_then(|t| self.type_name(t))
            }
            _ => None,
        }
    }

    /// Every named type a type expression mentions, outermost first:
    /// `Map<String, List<Job>>` yields `Map`, `String`, `List`, `Job`.
    ///
    /// Where [`Walker::type_name`] stops at the head — a receiver's members
    /// are the head's, not its arguments' — a *dependency* does not: a
    /// property of type `List<Job>` depends on `Job`, and that is the fact
    /// `USES_TYPE` records.
    fn type_names(&self, node: TsNode, out: &mut Vec<String>) {
        if node.kind() == "user_type" {
            if let Some(head) = self.type_name(node) {
                out.push(head);
            }
            for a in named_children(node)
                .into_iter()
                .filter(|c| c.kind() == "type_arguments")
            {
                for t in named_children(a) {
                    self.type_names(t, out);
                }
            }
            return;
        }
        for c in named_children(node) {
            self.type_names(c, out);
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
        if let Some(doc) = self.kdoc_before(node) {
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
        let modifiers = named_children(node)
            .into_iter()
            .find(|c| c.kind() == "modifiers")?;
        let out: Vec<String> = named_children(modifiers)
            .into_iter()
            .filter(|m| m.kind().ends_with("_modifier") && m.kind() != "visibility_modifier")
            .map(|m| self.text(m))
            .collect();
        (!out.is_empty()).then_some(out)
    }

    /// Every modifier word a declaration writes, for the keywords that
    /// decide what kind of declaration it is.
    fn modifier_words(&self, node: TsNode) -> Vec<String> {
        named_children(node)
            .into_iter()
            .find(|c| c.kind() == "modifiers")
            .map(|m| {
                self.text(m)
                    .split_whitespace()
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Kotlin's default visibility is public and unwritten; this reports
    /// only what the source states.
    fn visibility(&self, node: TsNode) -> Option<String> {
        let modifiers = named_children(node)
            .into_iter()
            .find(|c| c.kind() == "modifiers")?;
        named_children(modifiers)
            .into_iter()
            .find(|m| m.kind() == "visibility_modifier")
            .map(|m| self.text(m))
    }

    /// The KDoc immediately above a declaration — `/** … */`, cleaned of its
    /// asterisk gutter. A `//` line is not documentation.
    fn kdoc_before(&self, node: TsNode) -> Option<String> {
        let mut prev = node.prev_sibling()?;
        if detached_annotations(prev) {
            prev = prev.prev_sibling()?;
        }
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

    /// The name a declaration writes: its `name` field, else its first bare
    /// identifier — which is where a parameter and a variable put theirs.
    fn decl_name(&self, node: TsNode) -> Option<String> {
        if let Some(n) = node.child_by_field_name("name") {
            return Some(self.text(n));
        }
        named_children(node)
            .into_iter()
            .find(|c| c.kind() == "identifier")
            .map(|c| self.text(c))
    }

    /// A type's body, whichever of the two the grammar gives it.
    fn body_of<'t>(&self, node: TsNode<'t>) -> Option<TsNode<'t>> {
        named_children(node)
            .into_iter()
            .find(|c| matches!(c.kind(), "class_body" | "enum_class_body"))
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

/// Does this name read as a type? Kotlin's conventions are the only evidence
/// a syntax tree holds, and every codebase follows them.
fn starts_upper(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}
