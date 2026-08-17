//! The smallest plugin that is still a plugin — and, since the baseline
//! program, an honest TOML extractor: it claims `.toml`, and emits the
//! file's structure rather than one opaque node.
//!
//! The model follows the family's conventions, not a foreign schema: a
//! table (`[server]`, nested `[server.http]`, each element of a
//! `[[servers]]` array) is a **node** the file CONTAINS; its pairs ride the
//! node as **properties with values exactly as written** — never evaluated,
//! because `256 * 1024` folded wrongly is worse than the expression that
//! produced it. Top-level pairs ride the file node the same way. A file of
//! comments is just the file node; an empty table is a node with no pairs.
//!
//! It still implements [`Simple`] and depends on the SDK alone — a format
//! with no cross-file structure writes one function, and the SDK derives
//! both phases of the contract.

use dr_strange_ext::{
    Input, Manifest, Output, OutputExt, Simple, edge, host, node, output, simple_plugin,
};

/// Shown beside the name in UIs (`manifest.logo`): an original mark
/// evoking the language, not its trademarked logo.
const LOGO: &str = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24'><rect x='3' y='3' width='18' height='18' rx='3' fill='#9c4221'/><path d='M7 7.5h10M12 7.5V17' stroke='#fff' stroke-width='2' stroke-linecap='round'/></svg>";

struct Toml;

impl Simple for Toml {
    fn describe() -> Manifest {
        Manifest {
            name: "toml".into(),
            version: "1".into(),
            extensions: vec!["toml".into()],
            logo: Some(LOGO.into()),
        }
    }

    fn process(subject: Input, _options: &[(String, String)]) -> Result<Output, String> {
        let mut out = output();
        let files = match subject {
            Input::Files(paths) => paths,
            // A single document arrives with its bytes already in hand.
            Input::Document(doc) => {
                extract(&doc.name, &String::from_utf8_lossy(&doc.bytes), &mut out);
                return Ok(out.finish());
            }
        };

        for path in files {
            match host::read(&path) {
                Ok(bytes) => extract(&path, &String::from_utf8_lossy(&bytes), &mut out),
                // One unreadable file should not sink the ingest; it is counted
                // and named so a thin graph explains itself.
                Err(why) => {
                    out.report.skipped += 1;
                    out.note(format!("{path}: {why}"));
                }
            }
        }
        Ok(out.finish())
    }
}

/// One file into facts. A file that does not parse still yields its node —
/// counted and named, never fatal.
fn extract(path: &str, text: &str, out: &mut Output) {
    let mut file = node(path, "Manifest").described("path", "where the file was read from", path);
    match text.parse::<toml_edit::DocumentMut>() {
        Ok(doc) => {
            let table = doc.as_table();
            for (key, item) in table.iter() {
                if let toml_edit::Item::Value(v) = item {
                    file = file.prop(key, written(v));
                }
            }
            let file_node = file.build();
            let file_key = file_node.key.clone();
            out.nodes.push(file_node);
            for (key, item) in table.iter() {
                walk(path, &file_key, key, item, out);
            }
        }
        Err(why) => {
            out.nodes.push(file.build());
            out.note(format!("{path}: {why}"));
        }
    }
}

/// A table (or each element of an array of tables) becomes a node the
/// parent CONTAINS, pairs riding it as written; nested tables recurse.
fn walk(path: &str, parent_key: &str, name: &str, item: &toml_edit::Item, out: &mut Output) {
    let dotted = if parent_key.contains("::") {
        format!("{}.{name}", parent_key.split_once("::").unwrap().1)
    } else {
        name.to_string()
    };
    match item {
        toml_edit::Item::Table(t) => {
            let key = format!("{path}::{dotted}");
            emit_table(path, parent_key, &key, t, out);
        }
        toml_edit::Item::ArrayOfTables(arr) => {
            for (i, t) in arr.iter().enumerate() {
                let key = format!("{path}::{dotted}[{i}]");
                emit_table(path, parent_key, &key, t, out);
            }
        }
        _ => {}
    }
}

fn emit_table(path: &str, parent_key: &str, key: &str, t: &toml_edit::Table, out: &mut Output) {
    let mut n = node(key, "Table");
    for (k, item) in t.iter() {
        if let toml_edit::Item::Value(v) = item {
            n = n.prop(k, written(v));
        }
    }
    out.nodes.push(n.build());
    out.edges.push(edge(parent_key, "CONTAINS", key).build());
    for (k, item) in t.iter() {
        walk(path, key, k, item, out);
    }
}

/// The value exactly as the file wrote it — `0x10`, `1979-05-27`, a
/// multi-line string — trimmed of the decor toml_edit preserves around it.
fn written(v: &toml_edit::Value) -> String {
    v.to_string().trim().to_string()
}

simple_plugin!(Toml);

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str) -> Output {
        let mut out = output();
        extract("config.toml", text, &mut out);
        out
    }

    use dr_strange_ext::Value;

    fn node_at<'a>(o: &'a Output, key: &str) -> &'a dr_strange_ext::Node {
        o.nodes.iter().find(|n| n.key == key).unwrap_or_else(|| {
            panic!(
                "no node {key} in {:?}",
                o.nodes.iter().map(|n| &n.key).collect::<Vec<_>>()
            )
        })
    }

    fn props(n: &dr_strange_ext::Node) -> Value {
        n.properties.parse().expect("node properties are JSON")
    }

    fn prop(n: &dr_strange_ext::Node, name: &str) -> String {
        props(n)[name]
            .as_str()
            .unwrap_or_else(|| panic!("no prop {name} on {}", n.key))
            .to_string()
    }

    /// cbm golden: a table is a node, its pairs ride it as written.
    #[test]
    fn tables_are_nodes_with_pairs_as_written() {
        let o = run("[database]\nhost = \"localhost\"\nport = 5432\n");
        let t = node_at(&o, "config.toml::database");
        assert_eq!(t.label, "Table");
        assert_eq!(prop(t, "host"), "\"localhost\"");
        assert_eq!(prop(t, "port"), "5432");
        assert!(o.edges.iter().any(|e| e.src == "config.toml"
            && e.type_ == "CONTAINS"
            && e.dst == "config.toml::database"));
    }

    /// Nested headers nest the nodes; each [[element]] is its own node.
    #[test]
    fn nested_and_array_tables_structure() {
        let o = run(
            "[server.http]\nport = 8080\n\n[[servers]]\nname = \"a\"\n\n[[servers]]\nname = \"b\"\n",
        );
        assert_eq!(node_at(&o, "config.toml::server.http").label, "Table");
        assert_eq!(
            prop(node_at(&o, "config.toml::servers[0]"), "name"),
            "\"a\""
        );
        assert_eq!(
            prop(node_at(&o, "config.toml::servers[1]"), "name"),
            "\"b\""
        );
        // the intermediate [server] header parent contains its child
        assert!(o.edges.iter().any(|e| e.src == "config.toml::server"
            && e.type_ == "CONTAINS"
            && e.dst == "config.toml::server.http"));
    }

    /// Top-level pairs ride the file node; values stay exactly as written.
    #[test]
    fn top_level_pairs_ride_the_file_as_written() {
        let o = run("name = \"cbm\"\ncount = 0x10\nenabled = true\n");
        let f = node_at(&o, "config.toml");
        assert_eq!(prop(f, "name"), "\"cbm\"");
        assert_eq!(prop(f, "count"), "0x10");
        assert_eq!(prop(f, "enabled"), "true");
    }

    /// cbm negatives: an empty table is a node with no pairs; a file of
    /// comments is just the file node.
    #[test]
    fn empty_table_and_comments_only_negatives() {
        let o = run("[empty]\n");
        let t = node_at(&o, "config.toml::empty");
        assert_eq!(
            props(t).as_object().map(|m| m.len()),
            Some(0),
            "{}",
            t.properties
        );

        let o = run("# just a comment\n# another\n");
        assert_eq!(o.nodes.len(), 1, "{:?}", o.nodes);
        assert!(o.edges.is_empty());
    }

    /// A truncated file still yields its node — counted, never fatal.
    #[test]
    fn malformed_input_is_noted_not_fatal() {
        let o = run("name = \"cbm\"\n[[language\n");
        assert_eq!(node_at(&o, "config.toml").label, "Manifest");
        assert!(!o.report.notes.is_empty());
    }
}
