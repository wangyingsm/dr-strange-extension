//! The smallest plugin that is still a plugin: it claims `.toml`, reads each
//! file the router hands it, and emits one node per file.
//!
//! It exists to prove three things at once — that the contract is implementable
//! from the SDK alone, that the host's capability grant is enough to do real
//! work, and that a plugin needs nothing from the database's own crates.
//!
//! It implements [`Simple`], which is the fourth thing it proves: a format
//! with no cross-file structure writes **one function**, and the SDK derives
//! both phases of the contract — `parse` runs it per chunk, `assemble`
//! concatenates in chunk order.

use dr_strange_ext::{
    Input, Manifest, Output, OutputExt, Simple, host, node, output, simple_plugin,
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
                out.nodes.push(
                    node(&doc.name, "Manifest")
                        .prop("bytes", doc.bytes.len() as i64)
                        .build(),
                );
                return Ok(out.finish());
            }
        };

        for path in files {
            match host::read(&path) {
                Ok(bytes) => out.nodes.push(
                    node(&path, "Manifest")
                        .prop("bytes", bytes.len() as i64)
                        .described("path", "where the file was read from", path.as_str())
                        .build(),
                ),
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

simple_plugin!(Toml);
