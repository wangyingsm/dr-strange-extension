//! The schema a tree declares it keeps.
//!
//! Every code plugin can see the name of a table in a string literal and
//! nothing more. What none of them reads is the DDL that says what that table
//! *is* — its columns, their types, which of them point at another table. That
//! lives in a migration directory no parser was claiming.
//!
//! ## A sequence, not a set
//!
//! Migrations are the reason this plugin implements the contract's two phases
//! directly instead of taking the [`Simple`] facade. `0001_create_users.sql`
//! adds a column that `0014_trim.sql` removes; a parser that reads each file
//! on its own and unions the results reports a schema that never existed, with
//! every column that was ever added and not one of the drops applied.
//!
//! So `parse` does the expensive half per chunk and in parallel — lex, parse,
//! and reduce each statement to the change it makes — and `assemble` replays
//! those changes in migration order and emits only the schema the last one
//! leaves. Ordering is recomputed there from the paths, because the contract
//! requires the result not to depend on where the chunk boundaries fell.
//!
//! ## Queries written in code
//!
//! A statement in a string literal is read against that schema and becomes a
//! `Statement` node keyed by where it was written, with `READS`, `WRITES` and
//! `USES_COLUMN` edges onto the tables and columns the migrations declare —
//! the vocabulary the `xml` plugin already writes for a MyBatis mapper, so
//! one query answers "who touches this table" wherever the SQL was written.
//!
//! What it does **not** write is the edge from the enclosing function. That
//! needs the function's key, which only that language's own plugin can mint,
//! and an edge onto a key no node declares fails the entire write rather than
//! the one edge. So the direction that works outright is the one this plugin
//! owns both ends of — `context users` lists every statement that reads or
//! writes the table, each with the file and line it was written on.
//!
//! The other direction is a span: every definition node carries `file`,
//! `line` and `end_line`, and a `Statement` carries the same, so which
//! function encloses one is arithmetic over facts already in the graph. It
//! takes two queries today, because the openCypher subset holds one linear
//! path per `MATCH` and this join is a cross product of two:
//!
//! ```text
//! MATCH (s:Statement) WHERE s.file = '<path>' RETURN s.line, s.sql
//! MATCH (f:Function)  WHERE f.file = '<path>' RETURN f.name, f.line, f.end_line
//! ```
//!
//! Source files are pulled through the host, not routed here: the router
//! hands each file to exactly one handler and a `.rs` belongs to the rust
//! plugin's, so asking is the only way — which is what the grant is for.
//! Set `[plugins.sql] inline = false` to read the migrations alone.
//!
//! [`Simple`]: dr_strange_ext::Simple

mod change;
mod emit;
mod inline;
mod literal;
mod lower;
mod migrate;
mod schema;

#[cfg(test)]
mod tests;

use dr_strange_ext::{Guest, Input, Manifest, Output, OutputExt, export_plugin, host, output};
use serde::{Deserialize, Serialize};
use sqlparser::dialect::{Dialect, GenericDialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect};
use sqlparser::parser::Parser;

use change::FileChanges;

/// What one chunk hands to `assemble`.
///
/// The host never looks inside a partial, so this is the plugin's own shape
/// rather than the SDK's serialized `Output`: what crosses is the *changes*
/// each file makes, because no node can be minted until the whole sequence
/// has been replayed and only `assemble` has the whole sequence.
#[derive(Default, Deserialize, Serialize)]
struct Partial {
    files: Vec<FileChanges>,
    skipped: u32,
    notes: Vec<String>,
}

/// Shown beside the name in UIs: a stack of plates, the oldest mark there is
/// for stored data, and no vendor's logo.
const LOGO: &str = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24'><ellipse cx='12' cy='5.5' rx='7.5' ry='3' fill='#2f855a'/><path d='M4.5 5.5v5c0 1.66 3.36 3 7.5 3s7.5-1.34 7.5-3v-5' fill='#2f855a' opacity='.75'/><path d='M4.5 11.5v5c0 1.66 3.36 3 7.5 3s7.5-1.34 7.5-3v-5' fill='#2f855a' opacity='.55'/></svg>";

struct Sql;

impl Guest for Sql {
    fn describe() -> Manifest {
        Manifest {
            name: "sql".into(),
            version: "1".into(),
            extensions: vec!["sql".into()],
            logo: Some(LOGO.into()),
        }
    }

    fn parse(subject: Input, options: Vec<(String, String)>) -> Result<Vec<u8>, String> {
        let dialect = dialect_from(&options);
        let mut chunk = Partial::default();

        match subject {
            Input::Document(doc) => {
                let text = String::from_utf8_lossy(&doc.bytes);
                let file = read(&doc.name, &text, dialect.as_ref(), &mut chunk);
                chunk.files.push(file);
            }
            Input::Files(paths) => {
                for path in paths {
                    if !migrate::is_up(&path) {
                        // The reverse migration. Reading it would undo the
                        // tree that was just read.
                        continue;
                    }
                    match host::read(&path) {
                        Ok(bytes) => {
                            let text = String::from_utf8_lossy(&bytes);
                            let file = read(&path, &text, dialect.as_ref(), &mut chunk);
                            chunk.files.push(file);
                        }
                        Err(why) => {
                            chunk.skipped += 1;
                            chunk.notes.push(format!("{path}: {why}"));
                        }
                    }
                }
            }
        }

        serde_json::to_vec(&chunk).map_err(|e| format!("carrying this chunk to assemble: {e}"))
    }

    fn assemble(partials: Vec<Vec<u8>>, options: Vec<(String, String)>) -> Result<Output, String> {
        let mut out = output();
        let mut files: Vec<FileChanges> = Vec::new();

        for bytes in &partials {
            let chunk: Partial = serde_json::from_slice(bytes)
                .map_err(|e| format!("reading a chunk's changes: {e}"))?;
            out.report.skipped += chunk.skipped;
            out.report.notes.extend(chunk.notes);
            files.extend(chunk.files);
        }

        let ignored: u32 = files.iter().map(|f| f.ignored).sum();
        let read_files = files.len();
        let schema = schema::replay(files);
        let applied = schema.applied.len();

        emit::emit(&schema, &mut out);

        if wants_inline(&options) {
            let broader = broader_grammar(&options);
            let scanned = scan_code(dialect_from(&options).as_ref(), broader.as_deref());
            emit::statements(&schema, &scanned.stmts, &mut out);
            out.report.notes.extend(scanned.notes());
        }

        if applied > 0 {
            out.note(format!(
                "replayed {applied} migration(s) in order into {} table(s), \
                 {} view(s), {} type(s) and {} index(es); {ignored} statement(s) \
                 changed no shape",
                schema.tables.len(),
                schema.views.len(),
                schema.types.len(),
                schema.indexes.len(),
            ));
        } else if read_files > 0 {
            out.note(format!(
                "{read_files} .sql file(s) held no DDL this plugin models — \
                 a tree of queries or seed data rather than migrations"
            ));
        }

        // The hint belongs with the failures, and only when the dialect was
        // left to default: told which dialect to use, a parse error is about
        // the file rather than about this setting.
        let declared = options.iter().any(|(k, _)| k == "dialect");
        if !declared && out.report.skipped > 0 {
            out.note(
                "parsed as generic SQL; set `[plugins.sql] dialect = \"postgres\" \
                 | \"mysql\" | \"sqlite\"` if these files are one dialect's",
            );
        }

        Ok(out.finish())
    }
}

/// A source file this plugin will not scan for queries. Past this, a file is
/// generated or vendored, and lexing it costs more than the statements it
/// holds are worth.
const CODE_CAP: usize = 1 << 20;

/// What a pass over the tree's source files found.
#[derive(Default)]
struct Scanned {
    stmts: Vec<inline::Stmt>,
    /// Files that held at least one statement, and files read in all.
    with_sql: u32,
    read: u32,
    /// Literals that opened like a query and that the grammar refused.
    refused: Vec<String>,
    /// Statements about no table, which this plugin has nothing to say about.
    tableless: u32,
    oversized: Vec<String>,
}

impl Scanned {
    /// What a reader needs to know to explain the statements above — or their
    /// absence.
    fn notes(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.read > 0 {
            let tableless = match self.tableless {
                0 => String::new(),
                n => format!(", and {n} that name no table"),
            };
            out.push(format!(
                "read {} source file(s) for queries written in code and found \
                 {} in {}{tableless}; which function encloses one is a span \
                 query, not an edge this plugin asserts",
                self.read,
                self.stmts.len(),
                self.with_sql
            ));
        }
        if !self.refused.is_empty() {
            let shown: Vec<&str> = self.refused.iter().take(5).map(String::as_str).collect();
            out.push(format!(
                "{} literal(s) opened like a query and would not parse — \
                 assembled from branches, or naming a table at runtime: {}{}",
                self.refused.len(),
                shown.join("; "),
                if self.refused.len() > shown.len() {
                    ", …"
                } else {
                    ""
                }
            ));
        }
        if !self.oversized.is_empty() {
            out.push(format!(
                "{} source file(s) past {} KiB were not scanned for queries: {}",
                self.oversized.len(),
                CODE_CAP >> 10,
                self.oversized.join(", ")
            ));
        }
        out
    }
}

/// Whether to read the tree's source files as well as its migrations.
fn wants_inline(options: &[(String, String)]) -> bool {
    !options
        .iter()
        .any(|(k, v)| k == "inline" && matches!(v.trim(), "false" | "no" | "0" | "off"))
}

/// Every query written in the tree's source files.
///
/// Serial, in `assemble`, and deliberately: the host hands this plugin one
/// `.sql` file per `parse` call, so there is no chunk to spread this over
/// that would give the same answer whatever the chunking — and resolving a
/// table name needs the replayed schema, which only `assemble` holds.
fn scan_code(dialect: &dyn Dialect, broader: Option<&dyn Dialect>) -> Scanned {
    let mut found = Scanned::default();
    let Ok(paths) = host::list("") else {
        return found;
    };
    for path in paths {
        if literal::syntax_of(&path).is_none() {
            continue;
        }
        let Ok(bytes) = host::read(&path) else {
            continue;
        };
        if bytes.len() > CODE_CAP {
            found.oversized.push(path);
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        scan_text(&path, &text, dialect, broader, &mut found);
    }
    found
}

/// One source file's queries onto `found` — the half of [`scan_code`] that
/// needs no host, so a test exercises the same path with the text in hand.
fn scan_text(
    path: &str,
    text: &str,
    dialect: &dyn Dialect,
    broader: Option<&dyn Dialect>,
    found: &mut Scanned,
) {
    let Some(syntax) = literal::syntax_of(path) else {
        return;
    };
    found.read += 1;
    let before = found.stmts.len();
    for lit in literal::literals(text, &syntax) {
        match inline::read(path, &lit, dialect, broader) {
            Ok(stmt) => found.stmts.push(stmt),
            Err(inline::Refused::NotSql) => {}
            Err(inline::Refused::NoRelation) => found.tableless += 1,
            Err(inline::Refused::Unparsed(why)) => {
                found.refused.push(format!("{path}:{}: {why}", lit.line));
            }
        }
    }
    if found.stmts.len() > before {
        found.with_sql += 1;
    }
}

/// One file into the changes it applies.
fn read(path: &str, text: &str, dialect: &dyn Dialect, chunk: &mut Partial) -> FileChanges {
    let sql = migrate::up_section(text);
    let statements = match Parser::parse_sql(dialect, sql) {
        Ok(statements) => statements,
        Err(why) => {
            chunk.skipped += 1;
            chunk.notes.push(format!("{path}: {why}"));
            return FileChanges {
                path: path.to_string(),
                ..Default::default()
            };
        }
    };
    let mut changes = Vec::new();
    let mut ignored = 0u32;
    for statement in &statements {
        let made = lower::lower(statement);
        if made.is_empty() {
            ignored += 1;
        }
        changes.extend(made);
    }
    FileChanges {
        path: path.to_string(),
        changes,
        ignored,
    }
}

/// The dialect to parse with.
///
/// Declared, never sniffed: guessing a dialect from a file's contents is the
/// kind of guess this family refuses, and the three named here disagree about
/// enough syntax that a wrong guess is a parse error attributed to the file.
/// Absent, the generic dialect accepts the broadest grammar, and a file it
/// cannot read is counted and named with the hint to set this.
fn dialect_from(options: &[(String, String)]) -> Box<dyn Dialect> {
    match declared(options).as_deref() {
        Some("postgres" | "postgresql" | "pg") => Box::new(PostgreSqlDialect {}),
        Some("mysql" | "mariadb") => Box::new(MySqlDialect {}),
        Some("sqlite") => Box::new(SQLiteDialect {}),
        _ => Box::new(GenericDialect {}),
    }
}

/// The dialect the options name, lowercased.
fn declared(options: &[(String, String)]) -> Option<String> {
    options
        .iter()
        .find(|(k, _)| k == "dialect")
        .map(|(_, v)| v.trim().to_ascii_lowercase())
}

/// The grammar a refused *query* is retried against — nothing, when the
/// declared grammar is already the broadest one there is.
fn broader_grammar(options: &[(String, String)]) -> Option<Box<dyn Dialect>> {
    declared(options).map(|_| Box::new(GenericDialect {}) as Box<dyn Dialect>)
}

export_plugin!(Sql);
