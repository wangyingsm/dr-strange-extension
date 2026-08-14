//! The drsg web plugin: the parser, wrapped for the wasm contract.
//! Everything language-shaped lives in the parser crate, natively tested;
//! this file only crosses the boundary.

use dr_strange_ext::bindings::exports::drsg::preprocess::preprocessor::{
    Edge, Guest, Input, Manifest, Node, Output, Report,
};
use dr_strange_ext::export_plugin;
use drsg_web_parser::{EXTENSIONS, FileFacts, Options, assemble, parse_chunk, parse_document};

struct WebPlugin;

/// The parser reads through the host — the same adapter shape as the rust
/// plugin's.
struct HostFiles;

impl drsg_web_parser::Files for HostFiles {
    fn list(&self, suffix: &str) -> Result<Vec<String>, String> {
        dr_strange_ext::bindings::drsg::preprocess::host::list(suffix)
    }

    fn read(&self, path: &str) -> Result<Vec<u8>, String> {
        dr_strange_ext::bindings::drsg::preprocess::host::read(path)
    }

    fn label(&self) -> Option<String> {
        dr_strange_ext::bindings::drsg::preprocess::host::label()
    }
}

fn parse_options(options: &[(String, String)]) -> Options {
    let flag = |key: &str, val: &str| options.iter().any(|(k, v)| k == key && v == val);
    Options {
        include_source: flag("include_source", "true"),
        // `[plugins.web] dom = "full"` — every element becomes a node.
        dom: flag("dom", "full") || flag("dom", "true"),
    }
}

impl Guest for WebPlugin {
    fn describe() -> Manifest {
        Manifest {
            name: "web".into(),
            version: "1".into(),
            extensions: EXTENSIONS.iter().map(|e| e.to_string()).collect(),
        }
    }

    fn parse(subject: Input, options: Vec<(String, String)>) -> Result<Vec<u8>, String> {
        let opts = parse_options(&options);
        let facts: Vec<FileFacts> = match subject {
            Input::Files(paths) => parse_chunk(&HostFiles, &paths, opts),
            Input::Document(doc) => parse_document(&doc.name, &doc.bytes, opts),
        };
        // The partial is ours to shape; the host shuttles it and never looks.
        rmp_serde::to_vec(&facts).map_err(|e| format!("serializing a partial: {e}"))
    }

    fn assemble(partials: Vec<Vec<u8>>, _options: Vec<(String, String)>) -> Result<Output, String> {
        // In the order given — the host guarantees chunk order, and order is
        // what keeps the result independent of where the boundaries fell.
        let mut all: Vec<FileFacts> = Vec::new();
        for bytes in &partials {
            let chunk: Vec<FileFacts> = rmp_serde::from_slice(bytes)
                .map_err(|e| format!("a partial did not decode: {e}"))?;
            all.extend(chunk);
        }

        let assembled = assemble(all);
        let facts = (assembled.nodes.len() + assembled.edges.len()) as u32;
        Ok(Output {
            nodes: assembled
                .nodes
                .into_iter()
                .map(|n| Node {
                    key: n.key,
                    label: n.label,
                    extra_labels: n.extra_labels,
                    properties: serde_json::Value::Object(n.props).to_string(),
                })
                .collect(),
            edges: assembled
                .edges
                .into_iter()
                .map(|e| Edge {
                    src: e.src,
                    dst: e.dst,
                    type_: e.ty,
                    properties: serde_json::Value::Object(e.props).to_string(),
                })
                .collect(),
            prose: String::new(),
            report: Report {
                facts,
                prose_chars: 0,
                skipped: assembled.skipped as u32,
                notes: assembled.notes,
            },
        })
    }
}

export_plugin!(WebPlugin);
