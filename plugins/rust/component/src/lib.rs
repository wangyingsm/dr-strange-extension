//! The drsg `rust` plugin: `../parser` wrapped for the wasm contract.
//!
//! Thin on purpose. The parser is a plain library whose tests run natively;
//! this crate only adapts its two functions to the contract's two phases and
//! its `Files` trait to the host interface. Anything clever belongs one level
//! down, where it can be tested without a sandbox.

use dr_strange_ext::{Guest, Input, Manifest, Output, Report, export_plugin, host};
use drsg_rust_parser::{Assembled, FileFacts, Files, assemble, parse_chunk, parse_document};

struct RustPlugin;

/// The host interface, as the parser's `Files` trait.
struct HostFiles;

impl Files for HostFiles {
    fn list(&self, suffix: &str) -> Result<Vec<String>, String> {
        host::list(suffix)
    }

    fn read(&self, path: &str) -> Result<Vec<u8>, String> {
        host::read(path)
    }

    fn label(&self) -> Option<String> {
        host::label()
    }
}

fn include_source(options: &[(String, String)]) -> bool {
    options
        .iter()
        .any(|(k, v)| k == "include_source" && v == "true")
}

impl Guest for RustPlugin {
    fn describe() -> Manifest {
        Manifest {
            name: "rust".into(),
            // `2`: the parser's second shape — properties are the contract's
            // JSON now, where `rust@1` wrote the database's native types. The
            // facts are the same; `_generated_by` says which produced them.
            version: "2".into(),
            extensions: vec!["rs".into()],
        }
    }

    fn parse(subject: Input, options: Vec<(String, String)>) -> Result<Vec<u8>, String> {
        let include = include_source(&options);
        let facts: Vec<FileFacts> = match subject {
            Input::Files(paths) => parse_chunk(&HostFiles, &paths, include),
            Input::Document(doc) => parse_document(&doc.name, &doc.bytes, include),
        };
        // The partial is ours to shape; the host shuttles it and never looks.
        serde_json::to_vec(&facts).map_err(|e| format!("serializing a partial: {e}"))
    }

    fn assemble(partials: Vec<Vec<u8>>, _options: Vec<(String, String)>) -> Result<Output, String> {
        // In the order given — the host guarantees chunk order, and order is
        // what keeps the result independent of where the boundaries fell.
        let mut all: Vec<FileFacts> = Vec::new();
        for bytes in &partials {
            let chunk: Vec<FileFacts> = serde_json::from_slice(bytes)
                .map_err(|e| format!("a partial did not decode: {e}"))?;
            all.extend(chunk);
        }

        let Assembled {
            nodes,
            edges,
            skipped,
            notes,
        } = assemble(all);

        let nodes = nodes
            .into_iter()
            .map(|n| dr_strange_ext::Node {
                key: n.key,
                label: n.label,
                extra_labels: n.extra_labels,
                properties: serde_json::Value::Object(n.props).to_string(),
            })
            .collect::<Vec<_>>();
        let edges = edges
            .into_iter()
            .map(|e| dr_strange_ext::Edge {
                src: e.src,
                dst: e.dst,
                type_: e.ty,
                properties: serde_json::Value::Object(e.props).to_string(),
            })
            .collect::<Vec<_>>();

        let facts = (nodes.len() + edges.len()) as u32;
        Ok(Output {
            nodes,
            edges,
            // A parser is certain or silent; there is no residue for a model.
            prose: String::new(),
            report: Report {
                facts,
                prose_chars: 0,
                skipped: skipped as u32,
                notes,
            },
        })
    }
}

export_plugin!(RustPlugin);
