//! HTML and CSS into facts — nodes and edges a parser is certain of.
//!
//! One plugin for the whole page, deliberately: the cross-file facts a
//! frontend plane wants — which stylesheet's `.btn` styles which page —
//! can only be resolved where one assemble sees both sides. Inline
//! `<style>` is parsed with the same CSS grammar, defining in the page's
//! own namespace; inline `<script>` is **shallow-parsed** with the JS
//! grammar — function declarations and the calls between them, at the C
//! plugin's depth — because an inline script is file-scoped by nature and
//! the module-world JS belongs to the ts plugin.
//!
//! What counts as a fact is kept restrained: a page is not its every
//! `<div>`. Nodes are the page itself, its id-carrying elements, its inline
//! script functions — and, from CSS, class selectors and custom properties,
//! the stylesheet's declared vocabulary.
//!
//! Keys use the web's own address where it has one and the family's
//! file-namespace form where it does not: an id'd element is
//! `index.html#map` (the URL fragment — the platform's own syntax);
//! everything else is `{file}::{name-as-written}` — `site.css::.btn`,
//! `site.css::--brand`, `index.html::initMap`. The kept sigil also keeps
//! `::` unmistakable for a CSS pseudo-element, which is never followed by
//! `.` or `--`.

mod resolve;

pub use resolve::{Assembled, assemble};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use tree_sitter::Node as TsNode;

/// What the parser reads through — the plugin contract's host.
pub trait Files {
    fn list(&self, suffix: &str) -> Result<Vec<String>, String>;
    fn read(&self, path: &str) -> Result<Vec<u8>, String>;
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

/// One reference held for resolution at assemble.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ref {
    /// The page/element/function key making the reference.
    pub src: String,
    pub kind: RefKind,
    pub target: String,
    pub line: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefKind {
    /// `<link rel=stylesheet href>` / `<script src>` / CSS `@import` — a
    /// file reference, resolved against the parsed set.
    Import,
    /// `<a href>` — a link to a page (or a page#fragment) in this tree.
    Link,
    /// `class="btn"` in markup — binds to the class's definition: this
    /// page's own `<style>` first, else the unique stylesheet definition.
    Class,
    /// `var(--name)` in CSS — same binding rule as classes.
    Var,
    /// A bare call inside an inline script — binds within the page's own
    /// functions; anything else is a value's business, counted.
    Call,
}

/// A name this file defines (CSS class, custom property, or inline JS
/// function), for the assemble-phase indexes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decl {
    pub name: String,
    pub key: String,
}

/// Everything one file contributes — the opaque partial the component
/// serializes between the two phases.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileFacts {
    pub file: String,
    pub failed: bool,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub refs: Vec<Ref>,
    /// `.class` definitions — from a stylesheet, or a page's `<style>`.
    pub classes: Vec<Decl>,
    /// `--custom-property` definitions.
    pub vars: Vec<Decl>,
    /// Inline `<script>` function definitions (pages only).
    pub functions: Vec<Decl>,
    /// Call sites too dynamic to name.
    pub opaque: usize,
}

/// The extensions this plugin claims. SCSS/LESS compile *to* CSS and have
/// grammars of their own — deliberately not claimed in v1.
pub const EXTENSIONS: &[&str] = &["html", "htm", "css"];

/// Parse options, decoded from the plugin's `[plugins.web]` settings.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    pub include_source: bool,
    /// `dom = "full"`: every element becomes a node, keyed positionally
    /// (`page::body>div[2]>ul[1]`) — deterministic within a snapshot, but
    /// positional keys churn when markup shifts, which is why the default
    /// stays restrained (id-carrying elements only).
    pub dom: bool,
}

pub fn parse_chunk(files: &dyn Files, paths: &[String], opts: Options) -> Vec<FileFacts> {
    paths
        .iter()
        .map(|p| match files.read(p) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => parse_file(p, &text, opts),
                Err(_) => failed(p),
            },
            Err(_) => failed(p),
        })
        .collect()
}

pub fn parse_document(name: &str, bytes: &[u8], opts: Options) -> Vec<FileFacts> {
    match String::from_utf8(bytes.to_vec()) {
        Ok(text) => vec![parse_file(name, &text, opts)],
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

fn is_css(path: &str) -> bool {
    path.ends_with(".css")
}

fn parse_with(language: tree_sitter::Language, text: &str) -> Option<tree_sitter::Tree> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).ok()?;
    parser.parse(text, None)
}

fn parse_file(path: &str, text: &str, opts: Options) -> FileFacts {
    let mut facts = FileFacts {
        file: path.to_string(),
        ..Default::default()
    };
    if is_css(path) {
        let Some(tree) = parse_with(tree_sitter_css::LANGUAGE.into(), text) else {
            return failed(path);
        };
        let mut w = Walker::new(&mut facts, text, path, opts);
        let rules = w.css(tree.root_node(), 0);
        let mut props = Props::new();
        props.insert("rules".into(), Value::from(rules));
        facts.nodes.insert(
            0,
            Node {
                key: path.to_string(),
                label: "Stylesheet".into(),
                extra_labels: Vec::new(),
                props,
            },
        );
    } else {
        let Some(tree) = parse_with(tree_sitter_html::LANGUAGE.into(), text) else {
            return failed(path);
        };
        let mut w = Walker::new(&mut facts, text, path, opts);
        let title = w.html(tree.root_node());
        let mut props = Props::new();
        if let Some(t) = title {
            props.insert("title".into(), Value::String(t));
        }
        facts.nodes.insert(
            0,
            Node {
                key: path.to_string(),
                label: "Page".into(),
                extra_labels: Vec::new(),
                props,
            },
        );
    }
    facts
}

struct Walker<'a> {
    facts: &'a mut FileFacts,
    src: &'a str,
    file: String,
    opts: Options,
}

impl<'a> Walker<'a> {
    fn new(facts: &'a mut FileFacts, src: &'a str, file: &str, opts: Options) -> Self {
        Self {
            facts,
            src,
            file: file.to_string(),
            opts,
        }
    }

    // ---- HTML -------------------------------------------------------------

    /// Walks the page recursively, carrying the enclosing scope — the page,
    /// or the nearest ancestor that became a node — so elements nest the way
    /// the DOM nests. Returns the `<title>` text.
    fn html(&mut self, root: TsNode) -> Option<String> {
        let mut title = None;
        let file = self.file.clone();
        self.children(root, &file, "", &mut title);
        title
    }

    /// Recurse into `node`'s children under `parent` (a node key) at DOM
    /// path `path` (for positional keys under `dom = "full"`).
    fn children(&mut self, node: TsNode, parent: &str, path: &str, title: &mut Option<String>) {
        let mut nth: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
        let mut cursor = node.walk();
        let children: Vec<TsNode> = node.children(&mut cursor).collect();
        for child in children {
            match child.kind() {
                "element" => {
                    // Positional path: tag[n], counting per tag among siblings.
                    let tag = self.start_tag_of(child).map(|(t, _)| t).unwrap_or_default();
                    let n = nth.entry(tag.clone()).or_default();
                    *n += 1;
                    let child_path = if path.is_empty() {
                        format!("{tag}[{n}]")
                    } else {
                        format!("{path}>{tag}[{n}]")
                    };
                    self.element(child, parent, &child_path, title);
                }
                "script_element" => self.inline_script(child, parent),
                "style_element" => self.inline_style(child),
                _ => self.children(child, parent, path, title),
            }
        }
    }

    fn start_tag_of<'t>(&self, node: TsNode<'t>) -> Option<(String, TsNode<'t>)> {
        let start = node
            .named_children(&mut node.walk())
            .find(|c| c.kind() == "start_tag" || c.kind() == "self_closing_tag")?;
        let tag = start
            .named_children(&mut start.walk())
            .find(|c| c.kind() == "tag_name")
            .map(|c| self.text(c))?;
        Some((tag, start))
    }

    fn element(&mut self, node: TsNode, parent: &str, path: &str, title: &mut Option<String>) {
        let Some((tag, start)) = self.start_tag_of(node) else {
            return;
        };
        let attrs = self.attributes(start);
        let line = self.line(start);

        match tag.as_str() {
            "title" => {
                if title.is_none() {
                    *title = element_text(node, self.src);
                }
            }
            "link" => {
                // Only a stylesheet link is an import; icons and preloads
                // are bookkeeping.
                if attrs.get("rel").map(String::as_str) == Some("stylesheet")
                    && let Some(href) = attrs.get("href")
                {
                    self.push_ref(RefKind::Import, href.clone(), line);
                }
            }
            "script" => {} // handled as script_element
            "a" => {
                if let Some(href) = attrs.get("href") {
                    self.push_ref(RefKind::Link, href.clone(), line);
                }
            }
            _ => {}
        }

        // An id names a thing other files can address — the URL fragment is
        // the web's own syntax for it, so an id'd element keys as `page#id`
        // whatever else is on. Under `dom = "full"` every other element
        // becomes a node too, keyed by its position — and either way a node
        // hangs from its nearest node ancestor, the way the DOM nests.
        let own_key = if let Some(id) = attrs.get("id") {
            Some(format!("{}#{id}", self.file))
        } else if self.opts.dom {
            Some(format!("{}::{path}", self.file))
        } else {
            None
        };
        if let Some(key) = &own_key
            && !self.facts.nodes.iter().any(|n| n.key == *key)
        {
            let mut props = Props::new();
            props.insert("tag".into(), Value::String(tag.clone()));
            props.insert("line".into(), Value::from(line));
            props.insert("file".into(), Value::String(self.file.clone()));
            self.facts.nodes.push(Node {
                key: key.clone(),
                label: "Element".into(),
                extra_labels: Vec::new(),
                props,
            });
            self.facts
                .edges
                .push(edge_at(parent, key, "CONTAINS", line));
        }
        let scope = own_key.clone().unwrap_or_else(|| parent.to_string());

        // Classes bind the nearest scope — the element itself when it is a
        // node, else its closest node ancestor — to the stylesheet
        // vocabulary.
        if let Some(classes) = attrs.get("class") {
            for class in classes.split_whitespace() {
                self.facts.refs.push(Ref {
                    src: scope.clone(),
                    kind: RefKind::Class,
                    target: class.to_string(),
                    line,
                });
            }
        }

        self.children(node, &scope, path, title);
    }

    /// `<script src>` is an import; an inline body is shallow-parsed with
    /// the JS grammar — function declarations, and the calls between them.
    fn inline_script(&mut self, node: TsNode, parent: &str) {
        if let Some((_, start)) = self.start_tag_of(node) {
            let attrs = self.attributes(start);
            if let Some(src) = attrs.get("src") {
                self.push_ref(RefKind::Import, src.clone(), self.line(start));
                return;
            }
        }
        let Some(raw) = node
            .named_children(&mut node.walk())
            .find(|c| c.kind() == "raw_text")
        else {
            return;
        };
        let body = self.text(raw);
        let base_line = raw.start_position().row as u64; // body line 1 = this +1
        let Some(tree) = parse_with(tree_sitter_javascript::LANGUAGE.into(), &body) else {
            return;
        };
        self.inline_js(tree.root_node(), &body, base_line, parent);
    }

    /// The shallow walk: `function name(…)` and `const f = (…) =>` become
    /// Functions keyed `page::name`; bare calls inside any of them are held
    /// for page-local resolution; anything deeper is counted.
    fn inline_js(&mut self, root: TsNode, body: &str, base_line: u64, parent: &str) {
        let text = |n: TsNode| body[n.byte_range()].to_string();
        let line = |n: TsNode| base_line + n.start_position().row as u64 + 1;

        let mut stack = vec![root];
        let mut functions: Vec<(String, TsNode)> = Vec::new();
        while let Some(node) = stack.pop() {
            match node.kind() {
                "function_declaration" => {
                    if let Some(name) = node.child_by_field_name("name").map(&text) {
                        functions.push((name, node));
                    }
                }
                "lexical_declaration" | "variable_declaration" => {
                    for d in node.named_children(&mut node.walk()) {
                        if d.kind() == "variable_declarator"
                            && let (Some(name), Some(value)) = (
                                d.child_by_field_name("name").map(&text),
                                d.child_by_field_name("value"),
                            )
                            && matches!(value.kind(), "arrow_function" | "function_expression")
                        {
                            functions.push((name, value));
                        }
                    }
                }
                _ => {}
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                stack.push(child);
            }
        }

        for (name, fn_node) in &functions {
            let key = format!("{}::{name}", self.file);
            if self.facts.nodes.iter().any(|n| n.key == key) {
                continue;
            }
            let fline = line(*fn_node);
            let mut props = Props::new();
            let params = fn_node
                .child_by_field_name("parameters")
                .map(&text)
                .unwrap_or_default();
            props.insert(
                "signature".into(),
                Value::String(format!("function {name}{params}")),
            );
            props.insert("line".into(), Value::from(fline));
            props.insert("file".into(), Value::String(self.file.clone()));
            if self.opts.include_source {
                props.insert(
                    "_code".into(),
                    serde_json::json!({
                        "$desc": "source as written, for retrieval — not indexed or embedded",
                        "$value": text(*fn_node),
                    }),
                );
            }
            self.facts.nodes.push(Node {
                key: key.clone(),
                label: "Function".into(),
                extra_labels: Vec::new(),
                props,
            });
            self.facts
                .edges
                .push(edge_at(parent, &key, "CONTAINS", fline));
            self.facts.functions.push(Decl {
                name: name.clone(),
                key: key.clone(),
            });

            // Calls inside this function's body, bare names only — an
            // inline script's world is its page.
            if let Some(fn_body) = fn_node.child_by_field_name("body") {
                let mut cstack = vec![fn_body];
                while let Some(n) = cstack.pop() {
                    if n.kind() == "call_expression"
                        && let Some(f) = n.child_by_field_name("function")
                    {
                        {
                            if f.kind() == "identifier" {
                                self.facts.refs.push(Ref {
                                    src: key.clone(),
                                    kind: RefKind::Call,
                                    target: text(f),
                                    line: line(n),
                                });
                            } else {
                                self.facts.opaque += 1;
                            }
                        }
                    }
                    let mut cursor = n.walk();
                    for child in n.children(&mut cursor) {
                        cstack.push(child);
                    }
                }
            }
        }
    }

    /// An inline `<style>` block parses with the same CSS grammar and
    /// defines in the page's own namespace.
    fn inline_style(&mut self, node: TsNode) {
        let Some(raw) = node
            .named_children(&mut node.walk())
            .find(|c| c.kind() == "raw_text")
        else {
            return;
        };
        let body = self.text(raw);
        let base_line = raw.start_position().row as u64;
        let Some(tree) = parse_with(tree_sitter_css::LANGUAGE.into(), &body) else {
            return;
        };
        // Borrow juggling: the CSS walker reads `body`, not the page source.
        let mut inner = Walker {
            facts: self.facts,
            src: &body,
            file: self.file.clone(),
            opts: self.opts,
        };
        inner.css(tree.root_node(), base_line);
    }

    // ---- CSS --------------------------------------------------------------

    /// Walks a stylesheet (or an inline `<style>` body, offset by
    /// `base_line`); returns the rule count.
    fn css(&mut self, root: TsNode, base_line: u64) -> u64 {
        let mut rules = 0u64;
        // class name → the rules it appears in, as written, in source order —
        // attached to the Class node at the end of the walk.
        let mut class_rules: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            match node.kind() {
                "rule_set" => {
                    rules += 1;
                    self.css_rule(node, base_line, &mut class_rules);
                }
                "import_statement" => {
                    if let Some(target) = self.css_import_target(node) {
                        self.push_ref(RefKind::Import, target, base_line + self.line(node));
                    }
                }
                _ => {}
            }
            // Reversed, so the LIFO stack yields source order — the rules
            // list promises it.
            let mut cursor = node.walk();
            let children: Vec<TsNode> = node.children(&mut cursor).collect();
            for child in children.into_iter().rev() {
                stack.push(child);
            }
        }
        // A minified stylesheet's one-line rules are noise, not reading
        // material; its Class nodes keep only their line.
        if !is_minified(&self.file) {
            for (name, entries) in class_rules {
                let key = format!("{}::.{name}", self.file);
                if let Some(n) = self.facts.nodes.iter_mut().find(|n| n.key == key) {
                    n.props.insert(
                        "rules".into(),
                        serde_json::json!({
                            "$desc": "the rules this class appears in, as written",
                            "$value": entries,
                        }),
                    );
                }
            }
        }
        rules
    }

    fn css_rule(
        &mut self,
        node: TsNode,
        base_line: u64,
        class_rules: &mut BTreeMap<String, Vec<String>>,
    ) {
        let line = base_line + self.line(node);
        // The whole rule as one compact line: `selector { prop: value; … }`.
        let rule_text = {
            let raw = self.text(node);
            let mut compact = String::with_capacity(raw.len());
            let mut last_space = false;
            for ch in raw.chars() {
                if ch.is_whitespace() {
                    if !last_space {
                        compact.push(' ');
                        last_space = true;
                    }
                } else {
                    compact.push(ch);
                    last_space = false;
                }
            }
            compact
        };
        if let Some(selectors) = node
            .named_children(&mut node.walk())
            .find(|c| c.kind() == "selectors")
        {
            let mut stack = vec![selectors];
            while let Some(n) = stack.pop() {
                if n.kind() == "class_selector"
                    && let Some(name) = n
                        .named_children(&mut n.walk())
                        .find(|c| c.kind() == "class_name")
                        .map(|c| self.text(c))
                {
                    self.define_class(&name, line);
                    class_rules.entry(name).or_default().push(rule_text.clone());
                }
                let mut cursor = n.walk();
                for child in n.children(&mut cursor) {
                    stack.push(child);
                }
            }
        }

        if let Some(block) = node
            .named_children(&mut node.walk())
            .find(|c| c.kind() == "block")
        {
            for decl in block.named_children(&mut block.walk()) {
                if decl.kind() != "declaration" {
                    continue;
                }
                let Some(prop) = decl.named_child(0).map(|c| self.text(c)) else {
                    continue;
                };
                if prop.starts_with("--") {
                    self.define_var(&prop, decl, base_line);
                }
                // `var(--x)` anywhere in the value is a use.
                let mut stack = vec![decl];
                while let Some(n) = stack.pop() {
                    if n.kind() == "call_expression"
                        && self.text(n).starts_with("var(")
                        && let Some(args) = n
                            .named_children(&mut n.walk())
                            .find(|c| c.kind() == "arguments")
                        && let Some(name) = args.named_child(0).map(|c| self.text(c))
                        && name.starts_with("--")
                    {
                        self.facts.refs.push(Ref {
                            src: self.file.clone(),
                            kind: RefKind::Var,
                            target: name,
                            line: base_line + self.line(n),
                        });
                    }
                    let mut cursor = n.walk();
                    for child in n.children(&mut cursor) {
                        stack.push(child);
                    }
                }
            }
        }
    }

    fn define_class(&mut self, name: &str, line: u64) {
        let key = format!("{}::.{name}", self.file);
        if self.facts.nodes.iter().any(|n| n.key == key) {
            return;
        }
        let mut props = Props::new();
        props.insert("line".into(), Value::from(line));
        props.insert("file".into(), Value::String(self.file.clone()));
        self.facts.nodes.push(Node {
            key: key.clone(),
            label: "Class".into(),
            extra_labels: Vec::new(),
            props,
        });
        self.facts
            .edges
            .push(edge_at(&self.file, &key, "CONTAINS", line));
        self.facts.classes.push(Decl {
            name: name.to_string(),
            key,
        });
    }

    fn define_var(&mut self, name: &str, decl: TsNode, base_line: u64) {
        let key = format!("{}::{name}", self.file);
        if self.facts.nodes.iter().any(|n| n.key == key) {
            return;
        }
        let line = base_line + self.line(decl);
        let mut props = Props::new();
        // The value as written — everything after the colon.
        if let Some(text) = self.text(decl).split_once(':').map(|(_, v)| v) {
            props.insert(
                "value".into(),
                Value::String(text.trim().trim_end_matches(';').to_string()),
            );
        }
        props.insert("line".into(), Value::from(line));
        props.insert("file".into(), Value::String(self.file.clone()));
        self.facts.nodes.push(Node {
            key: key.clone(),
            label: "Const".into(),
            extra_labels: Vec::new(),
            props,
        });
        self.facts
            .edges
            .push(edge_at(&self.file, &key, "CONTAINS", line));
        self.facts.vars.push(Decl {
            name: name.to_string(),
            key,
        });
    }

    fn css_import_target(&self, node: TsNode) -> Option<String> {
        for c in node.named_children(&mut node.walk()) {
            match c.kind() {
                "string_value" => {
                    return Some(self.text(c).trim_matches(['"', '\'']).to_string());
                }
                "call_expression" => {
                    // `@import url("x.css")`
                    let text = self.text(c);
                    return Some(
                        text.trim_start_matches("url(")
                            .trim_end_matches(')')
                            .trim_matches(['"', '\''])
                            .to_string(),
                    );
                }
                _ => {}
            }
        }
        None
    }

    // ---- shared -----------------------------------------------------------

    fn attributes(&self, start: TsNode) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for attr in start.named_children(&mut start.walk()) {
            if attr.kind() != "attribute" {
                continue;
            }
            let Some(name) = attr
                .named_children(&mut attr.walk())
                .find(|c| c.kind() == "attribute_name")
                .map(|c| self.text(c))
            else {
                continue;
            };
            let value = attr
                .named_children(&mut attr.walk())
                .find(|c| c.kind() == "quoted_attribute_value" || c.kind() == "attribute_value")
                .map(|c| self.text(c).trim_matches(['"', '\'']).to_string())
                .unwrap_or_default();
            out.insert(name, value);
        }
        out
    }

    fn push_ref(&mut self, kind: RefKind, target: String, line: u64) {
        self.facts.refs.push(Ref {
            src: self.file.clone(),
            kind,
            target,
            line,
        });
    }

    fn text(&self, node: TsNode) -> String {
        self.src[node.byte_range()].to_string()
    }

    /// 1-based, like every editor's gutter (relative to this walker's own
    /// source — inline bodies add their base offset at the call sites).
    fn line(&self, node: TsNode) -> u64 {
        node.start_position().row as u64 + 1
    }
}

/// The text directly inside an element — for `<title>`.
fn element_text(node: TsNode, src: &str) -> Option<String> {
    let text = node
        .named_children(&mut node.walk())
        .find(|c| c.kind() == "text")
        .map(|c| src[c.byte_range()].trim().to_string())?;
    (!text.is_empty()).then_some(text)
}

/// A build artifact's name says so: `x.min.css`, `x.min.js`.
pub(crate) fn is_minified(path: &str) -> bool {
    path.rsplit('/').next().unwrap_or(path).contains(".min.")
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
