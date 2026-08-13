//! Write a preprocessor plugin for dr-strange, in Rust.
//!
//! A plugin turns a format-specific input into **facts** — nodes and edges it
//! is certain about — and **prose**, the residue that still needs a model. An
//! input that yields only facts is digested with no model call at all, which is
//! the point: an AST does not *infer* that `parse()` calls `lex()`, it knows,
//! and handing that to a model as prose spends tokens for a worse answer.
//!
//! ```ignore
//! use dr_strange_ext::{Guest, Input, Manifest, Output, export_plugin, host, node};
//!
//! struct Toml;
//!
//! impl Guest for Toml {
//!     fn describe() -> Manifest {
//!         Manifest {
//!             name: "toml".into(),
//!             version: "1".into(),
//!             extensions: vec!["toml".into()],
//!         }
//!     }
//!
//!     fn preprocess(subject: Input, _options: Vec<(String, String)>)
//!         -> Result<Output, String>
//!     {
//!         let mut out = Output::default();
//!         if let Input::Files(paths) = subject {
//!             for path in paths {
//!                 let bytes = host::read(&path)?;
//!                 out.nodes.push(
//!                     node(&path, "Manifest").prop("bytes", bytes.len() as i64).build(),
//!                 );
//!             }
//!         }
//!         Ok(out.finish())
//!     }
//! }
//!
//! export_plugin!(Toml);
//! ```
//!
//! ## What a plugin can reach
//!
//! Exactly [`host::list`], [`host::read`] and [`host::label`], and nothing
//! else. No filesystem, no network, no environment, and **no way to write
//! anywhere**. Reads are rooted at the directory the host was pointed at and
//! checked on the *resolved* path, so `..` and symlinks do not walk out of it.
//!
//! That is not a policy document sitting beside the code — it is the whole of
//! what the host will answer, so the two cannot drift apart.
//!
//! ## Determinism
//!
//! The clock is frozen and randomness is a fixed stream, so re-ingesting the
//! same tree yields the same graph. Neither is a source of entropy here. In
//! particular, prefer `BTreeMap` wherever ordering reaches the output: relying
//! on `HashMap` iteration order would make a plugin's facts depend on its own
//! internals rather than on what it read.

/// The generated bindings.
///
/// Public because `export_plugin!` expands to paths inside it. Most plugins
/// want the re-exports at the crate root instead.
pub mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "plugin",
        // The macro is invoked from the *author's* crate, so it has to be
        // public and has to know where these types actually live.
        pub_export_macro: true,
        export_macro_name: "export_plugin",
        default_bindings_module: "dr_strange_ext::bindings",
    });
}

/// Export your `Guest` implementation as the component's entry point.
///
/// ```ignore
/// dr_strange_ext::export_plugin!(MyPlugin);
/// ```
pub use bindings::export_plugin;
pub use bindings::drsg::preprocess::host;
pub use bindings::exports::drsg::preprocess::preprocessor::{
    Doc, Edge, Guest, Input, Manifest, Node, Output, Report,
};
pub use serde_json::Value;

/// An empty result, ready to be filled.
pub fn output() -> Output {
    Output {
        nodes: Vec::new(),
        edges: Vec::new(),
        prose: String::new(),
        report: Report {
            facts: 0,
            prose_chars: 0,
            skipped: 0,
            notes: Vec::new(),
        },
    }
}

/// Fill in the counts, and say what could not be done.
pub trait OutputExt {
    /// Set `facts` and `prose-chars` from what is actually there. Call it last.
    fn finish(self) -> Self;
    /// Say something a reader would want to know — what was skipped, what could
    /// not be resolved, and why. A thin graph should be explained by its report
    /// rather than investigated by re-running the ingest.
    fn note(&mut self, note: impl Into<String>);
}

impl OutputExt for Output {
    fn finish(mut self) -> Self {
        self.report.facts = (self.nodes.len() + self.edges.len()) as u32;
        self.report.prose_chars = self.prose.chars().count() as u32;
        self
    }

    fn note(&mut self, note: impl Into<String>) {
        self.report.notes.push(note.into());
    }
}

/// Start building a node.
pub fn node(key: impl Into<String>, label: impl Into<String>) -> NodeBuilder {
    NodeBuilder {
        key: key.into(),
        label: label.into(),
        extra: Vec::new(),
        props: Props::new(),
    }
}

/// Start building an edge between two node keys.
pub fn edge(src: impl Into<String>, ty: impl Into<String>, dst: impl Into<String>) -> EdgeBuilder {
    EdgeBuilder {
        src: src.into(),
        ty: ty.into(),
        dst: dst.into(),
        props: Props::new(),
    }
}

pub struct NodeBuilder {
    key: String,
    label: String,
    extra: Vec<String>,
    props: Props,
}

impl NodeBuilder {
    /// A label asserted rather than chosen — `External` on a stand-in for
    /// something outside the tree you read. These take no part in the model's
    /// label reconciliation, because there is no vocabulary to agree on.
    pub fn also(mut self, label: impl Into<String>) -> Self {
        self.extra.push(label.into());
        self
    }

    pub fn prop(mut self, name: &str, value: impl Into<Value>) -> Self {
        self.props.set(name, value);
        self
    }

    /// A property carrying its own description into the graph, so it explains
    /// itself to whoever reads it later.
    pub fn described(mut self, name: &str, desc: &str, value: impl Into<Value>) -> Self {
        self.props.describe(name, desc, value);
        self
    }

    pub fn build(self) -> Node {
        Node {
            key: self.key,
            label: self.label,
            extra_labels: self.extra,
            properties: self.props.finish(),
        }
    }
}

pub struct EdgeBuilder {
    src: String,
    ty: String,
    dst: String,
    props: Props,
}

impl EdgeBuilder {
    pub fn prop(mut self, name: &str, value: impl Into<Value>) -> Self {
        self.props.set(name, value);
        self
    }

    pub fn described(mut self, name: &str, desc: &str, value: impl Into<Value>) -> Self {
        self.props.describe(name, desc, value);
        self
    }

    pub fn build(self) -> Edge {
        Edge {
            src: self.src,
            dst: self.dst,
            type_: self.ty,
            properties: self.props.finish(),
        }
    }
}

/// Properties, rendered as the JSON object the host reads.
///
/// They cross as JSON because a value may itself be a list or a map of values
/// and WIT has no recursive types. It is the same shape the graph's own write
/// API accepts, so nothing is invented for the occasion.
#[derive(Default)]
pub struct Props(serde_json::Map<String, Value>);

impl Props {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, name: &str, value: impl Into<Value>) -> &mut Self {
        self.0.insert(name.to_string(), value.into());
        self
    }

    /// `{"$desc": …, "$value": …}` — a property that explains itself.
    pub fn describe(&mut self, name: &str, desc: &str, value: impl Into<Value>) -> &mut Self {
        self.0.insert(
            name.to_string(),
            serde_json::json!({ "$desc": desc, "$value": value.into() }),
        );
        self
    }

    /// An embedding vector, which the graph indexes natively.
    pub fn vector(&mut self, name: &str, values: Vec<f32>) -> &mut Self {
        self.0
            .insert(name.to_string(), serde_json::json!({ "$vector": values }));
        self
    }

    pub fn finish(self) -> String {
        Value::Object(self.0).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_property_map_is_a_json_object() {
        let mut p = Props::new();
        p.set("name", "execute").set("arity", 2i64);
        let v: Value = serde_json::from_str(&p.finish()).unwrap();
        assert_eq!(v["name"], "execute");
        assert_eq!(v["arity"], 2);
    }

    #[test]
    fn a_described_property_carries_its_description() {
        let mut p = Props::new();
        p.describe("_generated_by", "what produced this, not a model", "toml@1");
        let v: Value = serde_json::from_str(&p.finish()).unwrap();
        assert_eq!(v["_generated_by"]["$desc"], "what produced this, not a model");
        assert_eq!(v["_generated_by"]["$value"], "toml@1");
    }

    #[test]
    fn a_vector_is_tagged_so_the_graph_indexes_it() {
        let mut p = Props::new();
        p.vector("embedding", vec![0.5, 0.25]);
        let v: Value = serde_json::from_str(&p.finish()).unwrap();
        assert_eq!(v["embedding"]["$vector"][0], 0.5);
    }

    #[test]
    fn a_node_carries_its_extra_labels_and_properties() {
        let n = node("std::fmt::Display", "Trait")
            .also("External")
            .described("path", "as the source wrote it", "std::fmt::Display")
            .build();
        assert_eq!(n.extra_labels, vec!["External".to_string()]);
        let v: Value = serde_json::from_str(&n.properties).unwrap();
        assert_eq!(v["path"]["$value"], "std::fmt::Display");
    }

    #[test]
    fn finish_counts_what_was_produced() {
        let mut out = output();
        out.nodes.push(node("a", "Thing").build());
        out.edges.push(edge("a", "USES", "b").build());
        out.prose = "four".into();
        let out = out.finish();
        assert_eq!(out.report.facts, 2);
        assert_eq!(out.report.prose_chars, 4);
    }
}
