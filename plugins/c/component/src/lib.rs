//! The drsg c plugin: the parser, wrapped for the wasm contract.
//! Everything language-shaped lives in the parser crate, natively tested;
//! this file only crosses the boundary.

use dr_strange_ext::bindings::exports::drsg::preprocess::preprocessor::{
    Edge, Guest, Input, Manifest, Node, Output, Report,
};
use dr_strange_ext::export_plugin;
use drsg_c_parser::{EXTENSIONS, FileFacts, assemble, parse_chunk, parse_document};

/// Shown beside the name in UIs (`manifest.logo`): an original mark
/// evoking the language, not its trademarked logo.
const LOGO: &str = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24'><path d='M12 1.8l8.8 5.1v10.2L12 22.2l-8.8-5.1V6.9z' fill='#659ad2'/><text x='12' y='15.8' font-family='system-ui,sans-serif' font-size='10' font-weight='700' fill='#fff' text-anchor='middle'>C</text></svg>";

struct CPlugin;

/// The parser reads through the host — the same adapter shape as the rust
/// plugin's.
struct HostFiles;

impl drsg_c_parser::Files for HostFiles {
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

fn include_source(options: &[(String, String)]) -> bool {
    options
        .iter()
        .any(|(k, v)| k == "include_source" && v == "true")
}

impl Guest for CPlugin {
    fn describe() -> Manifest {
        Manifest {
            name: "c".into(),
            version: "1".into(),
            extensions: EXTENSIONS.iter().map(|e| e.to_string()).collect(),
            logo: Some(LOGO.into()),
        }
    }

    fn parse(subject: Input, options: Vec<(String, String)>) -> Result<Vec<u8>, String> {
        let include = include_source(&options);
        let facts: Vec<FileFacts> = match subject {
            Input::Files(paths) => parse_chunk(&HostFiles, &paths, include),
            Input::Document(doc) => parse_document(&doc.name, &doc.bytes, include),
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

export_plugin!(CPlugin);
