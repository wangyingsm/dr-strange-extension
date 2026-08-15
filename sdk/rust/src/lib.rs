//! Write a preprocessor plugin for dr-strange, in Rust.
//!
//! A plugin turns a format-specific input into **facts** — nodes and edges it
//! is certain about — and **prose**, the residue that still needs a model. An
//! input that yields only facts is digested with no model call at all, which is
//! the point: an AST does not *infer* that `parse()` calls `lex()`, it knows,
//! and handing that to a model as prose spends tokens for a worse answer.
//!
//! ## Two ways in
//!
//! The contract is two-phase — [`Guest::parse`] turns one chunk of input into
//! an opaque partial, and [`Guest::assemble`] turns every partial into the
//! result. The host runs `parse` over chunks **in parallel** and calls
//! `assemble` once, with partials in chunk order.
//!
//! Most formats have no cross-file structure and never need to see the split.
//! Implement [`Simple`] and both phases are derived: `parse` runs your one
//! function on the chunk, `assemble` concatenates in order.
//!
//! ```ignore
//! use dr_strange_ext::{Input, Manifest, Output, OutputExt, host, node, output, simple_plugin};
//!
//! struct Toml;
//!
//! impl dr_strange_ext::Simple for Toml {
//!     fn describe() -> Manifest {
//!         Manifest {
//!             name: "toml".into(),
//!             version: "1".into(),
//!             extensions: vec!["toml".into()],
//!             logo: None, // an inline SVG here shows beside the name in UIs
//!         }
//!     }
//!
//!     fn process(subject: Input, _options: &[(String, String)]) -> Result<Output, String> {
//!         let mut out = output();
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
//! simple_plugin!(Toml);
//! ```
//!
//! A plugin that resolves *across* files — a code parser following imports —
//! implements [`Guest`] directly: `parse` returns whatever serialization its
//! own `assemble` wants to read back. The bytes are opaque to the host, so the
//! format is entirely the plugin's business. Finish with
//! [`export_plugin!`](crate::export_plugin).
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
//! The clock is frozen, so time is not a source of variety. `parse` calls may
//! run concurrently in instances that share nothing, and `assemble` must not
//! depend on where the chunk boundaries fell — the same tree has to yield the
//! same graph. Prefer `BTreeMap` wherever ordering reaches the output: relying
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

pub use bindings::drsg::preprocess::host;
/// Export your [`Guest`] implementation as the component's entry point.
///
/// ```ignore
/// dr_strange_ext::export_plugin!(MyPlugin);
/// ```
///
/// A [`Simple`] plugin uses [`simple_plugin!`] instead, which derives the
/// two-phase `Guest` and calls this for you.
pub use bindings::export_plugin;
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

/// A plugin with no cross-file structure: one function, both phases derived.
///
/// `process` is handed one chunk at a time and its outputs are concatenated in
/// chunk order — correct exactly when nothing in one chunk refers to another,
/// which is what "simple" means here. A format where files reference each
/// other implements [`Guest`] directly instead.
pub trait Simple {
    fn describe() -> Manifest;
    fn process(subject: Input, options: &[(String, String)]) -> Result<Output, String>;
}

/// Derive the two-phase [`Guest`] from a [`Simple`] implementation and export
/// it. `parse` runs `process` on the chunk and serializes the output as the
/// partial; `assemble` concatenates the partials in order.
#[macro_export]
macro_rules! simple_plugin {
    // `ident`, not `ty`: the generated `export_plugin!` underneath matches an
    // identifier, and a `ty` fragment cannot be re-matched as one.
    ($t:ident) => {
        impl $crate::Guest for $t {
            fn describe() -> $crate::Manifest {
                <$t as $crate::Simple>::describe()
            }

            fn parse(
                subject: $crate::Input,
                options: ::std::vec::Vec<(::std::string::String, ::std::string::String)>,
            ) -> ::std::result::Result<::std::vec::Vec<u8>, ::std::string::String> {
                let out = <$t as $crate::Simple>::process(subject, &options)?;
                ::std::result::Result::Ok($crate::partial::encode(&out))
            }

            fn assemble(
                partials: ::std::vec::Vec<::std::vec::Vec<u8>>,
                _options: ::std::vec::Vec<(::std::string::String, ::std::string::String)>,
            ) -> ::std::result::Result<$crate::Output, ::std::string::String> {
                $crate::partial::merge(partials)
            }
        }
        $crate::export_plugin!($t);
    };
}

/// The partial format behind [`Simple`]: a serialized [`Output`] per chunk.
///
/// Only the facade uses it — a plugin implementing [`Guest`] directly owns its
/// partial format outright and never needs this module. Public because the
/// `simple_plugin!` macro expands in the author's crate.
pub mod partial {
    use super::{Edge, Node, Output, OutputExt, Report, output};
    use serde_json::{Value, json};

    pub fn encode(out: &Output) -> Vec<u8> {
        let v = json!({
            "nodes": out.nodes.iter().map(|n| json!({
                "key": n.key,
                "label": n.label,
                "extra": n.extra_labels,
                "props": n.properties,
            })).collect::<Vec<_>>(),
            "edges": out.edges.iter().map(|e| json!({
                "src": e.src,
                "dst": e.dst,
                "type": e.type_,
                "props": e.properties,
            })).collect::<Vec<_>>(),
            "prose": out.prose,
            "skipped": out.report.skipped,
            "notes": out.report.notes,
        });
        v.to_string().into_bytes()
    }

    pub fn decode(bytes: &[u8]) -> Result<Output, String> {
        let v: Value =
            serde_json::from_slice(bytes).map_err(|e| format!("a partial did not decode: {e}"))?;
        let str_of = |v: &Value, k: &str| -> Result<String, String> {
            v[k].as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("a partial is missing `{k}`"))
        };

        let mut out = output();
        for n in v["nodes"].as_array().into_iter().flatten() {
            out.nodes.push(Node {
                key: str_of(n, "key")?,
                label: str_of(n, "label")?,
                extra_labels: n["extra"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect(),
                properties: str_of(n, "props")?,
            });
        }
        for e in v["edges"].as_array().into_iter().flatten() {
            out.edges.push(Edge {
                src: str_of(e, "src")?,
                dst: str_of(e, "dst")?,
                type_: str_of(e, "type")?,
                properties: str_of(e, "props")?,
            });
        }
        out.prose = v["prose"].as_str().unwrap_or_default().to_string();
        out.report = Report {
            facts: 0,
            prose_chars: 0,
            skipped: v["skipped"].as_u64().unwrap_or(0) as u32,
            notes: v["notes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect(),
        };
        Ok(out)
    }

    /// Concatenate partials **in the order given**, which the host guarantees
    /// is chunk order — that is what keeps the result independent of where the
    /// boundaries fell.
    pub fn merge(partials: Vec<Vec<u8>>) -> Result<Output, String> {
        let mut out = output();
        let mut prose = Vec::new();
        for bytes in &partials {
            let part = decode(bytes)?;
            out.nodes.extend(part.nodes);
            out.edges.extend(part.edges);
            if !part.prose.is_empty() {
                prose.push(part.prose);
            }
            out.report.skipped += part.report.skipped;
            out.report.notes.extend(part.report.notes);
        }
        out.prose = prose.join("\n\n");
        Ok(out.finish())
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
        assert_eq!(
            v["_generated_by"]["$desc"],
            "what produced this, not a model"
        );
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

    #[test]
    fn a_partial_round_trips() {
        let mut out = output();
        out.nodes.push(
            node("k::a", "Thing")
                .also("External")
                .prop("n", 1i64)
                .build(),
        );
        out.edges.push(edge("k::a", "USES", "k::b").build());
        out.prose = "residue".into();
        out.report.skipped = 2;
        out.note("one file was unreadable");

        let back = partial::decode(&partial::encode(&out)).unwrap();
        assert_eq!(back.nodes.len(), 1);
        assert_eq!(back.nodes[0].key, "k::a");
        assert_eq!(back.nodes[0].extra_labels, vec!["External".to_string()]);
        assert_eq!(back.edges[0].type_, "USES");
        assert_eq!(back.prose, "residue");
        assert_eq!(back.report.skipped, 2);
        assert_eq!(back.report.notes.len(), 1);
    }

    /// Order is the contract: the host passes partials in chunk order, and
    /// concatenation in that order is what makes the result independent of
    /// where the boundaries fell.
    #[test]
    fn merge_concatenates_in_the_order_given() {
        let mut a = output();
        a.nodes.push(node("first", "T").build());
        a.prose = "one".into();
        let mut b = output();
        b.nodes.push(node("second", "T").build());
        b.prose = "two".into();
        b.report.skipped = 1;

        let merged = partial::merge(vec![partial::encode(&a), partial::encode(&b)]).unwrap();
        assert_eq!(merged.nodes[0].key, "first");
        assert_eq!(merged.nodes[1].key, "second");
        assert_eq!(merged.prose, "one\n\ntwo");
        assert_eq!(merged.report.skipped, 1);
        assert_eq!(merged.report.facts, 2);
    }
}
