//! XML is a container format, not a language: one `.xml` is a MyBatis mapper,
//! the next is a logback config, and the two have nothing in common but their
//! angle brackets. So this plugin claims the extension and then **dispatches on
//! the dialect the file declares** — its first start element — rather than
//! pretending one schema fits.
//!
//! A dialect it knows becomes facts. Anything else leaves as **prose**, exactly
//! as an unclaimed file's text does today, so claiming `.xml` takes nothing
//! from the reader that has it now.
//!
//! The first dialect is the MyBatis mapper (issue #41): the statements in it
//! name Java methods, and those names are the java plugin's own keys, so the
//! two halves of a Mapper meet in one graph.

mod mybatis;
mod sql;

use dr_strange_ext::{Input, Manifest, Output, OutputExt, Simple, host, output, simple_plugin};

/// Shown beside the name in UIs (`manifest.logo`): an original mark evoking a
/// marked-up document, not any vendor's logo.
const LOGO: &str = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24'><rect x='3' y='2' width='18' height='20' rx='2' fill='#2b6cb0'/><path d='M9 8 6.5 12 9 16M15 8l2.5 4L15 16' stroke='#fff' stroke-width='1.8' stroke-linecap='round' stroke-linejoin='round' fill='none'/></svg>";

struct Xml;

impl Simple for Xml {
    fn describe() -> Manifest {
        Manifest {
            name: "xml".into(),
            version: "1".into(),
            extensions: vec!["xml".into()],
            logo: Some(LOGO.into()),
        }
    }

    fn process(subject: Input, _options: &[(String, String)]) -> Result<Output, String> {
        let mut out = output();
        match subject {
            Input::Document(doc) => {
                extract(&doc.name, &String::from_utf8_lossy(&doc.bytes), &mut out);
            }
            Input::Files(paths) => {
                for path in paths {
                    match host::read(&path) {
                        Ok(bytes) => extract(&path, &String::from_utf8_lossy(&bytes), &mut out),
                        // One unreadable file should not sink the ingest; it is
                        // counted and named so a thin graph explains itself.
                        Err(why) => {
                            out.report.skipped += 1;
                            out.note(format!("{path}: {why}"));
                        }
                    }
                }
            }
        }
        Ok(out.finish())
    }
}

/// Which dialect a document declares, read from the first start element it
/// opens — the file's own statement about what it is.
#[derive(Debug, PartialEq, Eq)]
enum Dialect {
    /// `<mapper namespace="…">`: a MyBatis mapper.
    MybatisMapper,
    /// A root element this plugin has no reader for.
    Unknown,
    /// Nothing opened at all — not XML, or truncated before the root.
    Malformed(String),
}

/// The root element, and whether it carries the attribute the dialect needs.
/// Reads only as far as that element: the rest is the dialect reader's.
fn dialect(text: &str) -> Dialect {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(text);
    reader.config_mut().trim_text(true);
    loop {
        match reader.read_event() {
            // `Empty` is the root too: `<mapper namespace="x"/>` is a mapper
            // that declares no statements, not a file without a root.
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let has = |want: &str| {
                    e.attributes()
                        .flatten()
                        .any(|a| a.key.as_ref() == want.as_bytes())
                };
                return match name.as_str() {
                    // A mapper without a namespace names no interface, so
                    // there is nothing to join it to.
                    "mapper" if has("namespace") => Dialect::MybatisMapper,
                    _ => Dialect::Unknown,
                };
            }
            Ok(Event::Eof) => return Dialect::Malformed("no root element".into()),
            Err(why) => return Dialect::Malformed(why.to_string()),
            // A declaration, a DOCTYPE or a comment before the root. The
            // DOCTYPE is never fetched: this sandbox has no network, and a
            // file's own root element is the better evidence anyway.
            _ => {}
        }
    }
}

/// One file into facts, or into prose where no dialect claims it.
fn extract(path: &str, text: &str, out: &mut Output) {
    match dialect(text) {
        Dialect::MybatisMapper => {
            let read = mybatis::read(path, text, out);
            out.note(format!(
                "{path}: {} statement(s) read, touching {} table(s) and {} column(s)",
                read.statements, read.tables, read.columns
            ));
            if read.unbound_columns > 0 {
                out.note(format!(
                    "{path}: {} column(s) named without saying whose they are — several \
                     tables and no qualifier, so they ride the statement rather than \
                     an edge under a guess",
                    read.unbound_columns
                ));
            }
            if read.interpolated_tables > 0 {
                out.note(format!(
                    "{path}: {} statement(s) name their table with a `${{…}}`, so their \
                     table list is what the text could say, not all of it",
                    read.interpolated_tables
                ));
            }
            if read.foreign_includes > 0 {
                out.note(format!(
                    "{path}: {} `<include refid>` named a fragment another file holds — \
                     a cross-file include is not resolved",
                    read.foreign_includes
                ));
            }
            if read.interpolations > 0 {
                out.note(format!(
                    "{path}: {} `${{…}}` interpolation(s) left as written — what they \
                     paste may be the table itself, and is not guessed at",
                    read.interpolations
                ));
            }
        }
        Dialect::Unknown => {
            // Prose, exactly as an unclaimed file's text would be. Claiming an
            // extension must not quietly remove a file from the graph.
            if !out.prose.is_empty() {
                out.prose.push_str("\n\n");
            }
            out.prose.push_str(text);
        }
        Dialect::Malformed(why) => {
            out.report.skipped += 1;
            out.note(format!("{path}: {why}"));
        }
    }
}

simple_plugin!(Xml);

#[cfg(test)]
mod tests;
