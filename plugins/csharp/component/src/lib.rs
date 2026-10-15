//! The drsg csharp plugin: the parser, wrapped for the wasm contract.
//! Everything language-shaped lives in the parser crate, natively tested;
//! this file only crosses the boundary.

use dr_strange_ext::bindings::exports::drsg::preprocess::preprocessor::{
    Edge, Guest, Input, Manifest, Node, Output, Report,
};
use dr_strange_ext::export_plugin;
use drsg_csharp_parser::{EXTENSIONS, FileFacts, assemble, parse_chunk, parse_document};

/// Shown beside the name in UIs (`manifest.logo`): an original mark
/// evoking the language, not its trademarked logo.
const LOGO: &str = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24' fill='none' stroke='#68217a' stroke-width='1.8' stroke-linecap='round' stroke-linejoin='round'><path d='M12.5 8.5a4 4 0 1 0 0 7'/><path d='M16 8.5l-1 7M19 8.5l-1 7' stroke='#9b4f9b'/><path d='M14.5 10.5h5.5M14 13.5h5.5' stroke='#9b4f9b'/><path d='M4 6.5h4M4 17.5h4M4 6.5v11'/></svg>";

struct CSharpPlugin;

/// The parser reads through the host — the same adapter shape as the java
/// plugin's.
struct HostFiles;

impl drsg_csharp_parser::Files for HostFiles {
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

impl Guest for CSharpPlugin {
    fn describe() -> Manifest {
        Manifest {
            name: "csharp".into(),
            // `1`: the first shape. It marks test code (`test_flag`, with a
            // `_test_flag_confidence` the display skips), writes a declared
            // type as a `USES_TYPE` edge rather than leaving it in a prop,
            // and records where each declaration ends — the shape the java
            // plugin reached at `2`, since there is no earlier C# plane to
            // stay readable for.
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

export_plugin!(CSharpPlugin);
