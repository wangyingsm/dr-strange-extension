//! A query written in code: what its text says it touches.
//!
//! The same grammar that reads the migrations reads these, so a statement is
//! either understood or refused — never half-guessed. What a literal cannot
//! carry is the function it sits in: that key belongs to the language's own
//! plugin, and a key invented here would be wrong in eight different ways.
//! So each statement records the file and line it was written on, and which
//! function encloses it is a question about spans that the graph can answer
//! without this plugin asserting anything.

use std::ops::ControlFlow;

use sqlparser::ast::{
    AssignmentTarget, Delete, Expr, FromTable, Insert, Select, SelectItem, SetExpr, Statement,
    TableFactor, TableObject, TableWithJoins, Update, Visit, Visitor, visit_expressions,
};
use sqlparser::dialect::Dialect;
use sqlparser::parser::Parser;

use crate::change::Name;
use crate::literal::Literal;
use crate::lower::name_of;

/// Where in the statement a column was named. A projected column is part of
/// the shape the caller gets back; one in a predicate is part of how the rows
/// were chosen; and telling them apart is most of what makes the edge worth
/// following.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Role {
    Projection,
    Assignment,
    Reference,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Projection => "projection",
            Self::Assignment => "assignment",
            Self::Reference => "reference",
        }
    }
}

/// One column the statement names, and whose it is where that can be said.
pub type ColumnUse = (Option<Name>, String, Role);

/// One query found in a source file.
#[derive(Debug)]
pub struct Stmt {
    pub file: String,
    pub line: u32,
    pub end_line: u32,
    pub kind: &'static str,
    /// The statement as written, whitespace collapsed, holes still in it.
    pub sql: String,
    pub reads: Vec<Name>,
    pub writes: Vec<Name>,
    pub columns: Vec<ColumnUse>,
    /// A hole in the text was read as a bind parameter.
    pub parameterized: bool,
    /// The declared grammar refused it and the broadest one read it.
    pub by_generic: bool,
}

/// Why a literal was not read as a query.
pub enum Refused {
    /// Not a query: the text does not open with a word that starts one.
    NotSql,
    /// A statement, but one that names no relation — `SELECT 1`, or the
    /// `"SELECT a "` half of a string some language joins with a `+` this one
    /// does not. This plugin has nothing to say about a query that is about
    /// no table, and a node for one would carry a literal and no facts.
    NoRelation,
    /// It opens like one, and the grammar refused it.
    Unparsed(String),
}

/// Read one string literal as a statement, against the declared grammar or —
/// where that refuses it — the broadest one.
///
/// The declared dialect is about the *migrations*, where the wrong grammar
/// changes the schema. A query is a different risk: a more permissive grammar
/// can only add a statement whose table names came from its own text. And a
/// repository routinely holds both — a Postgres primary and a SQLite cache,
/// `$1` and `?` placeholders in neighbouring modules — so one declared
/// grammar cannot read all of it. Retrying is how both get read, and the node
/// says which grammar read it.
pub fn read(
    file: &str,
    lit: &Literal,
    dialect: &dyn Dialect,
    broader: Option<&dyn Dialect>,
) -> Result<Stmt, Refused> {
    let bare = strip_leading_comments(&lit.text);
    if !opens_a_query(bare) {
        return Err(Refused::NotSql);
    }
    let (filled, parameterized) = holes(bare);
    let (parsed, by_generic) = match Parser::parse_sql(dialect, &filled) {
        Ok(parsed) => (parsed, false),
        // The declared grammar's complaint, never the fallback's: it is the
        // one the operator chose and the one they can act on.
        Err(why) => match broader.map(|b| Parser::parse_sql(b, &filled)) {
            Some(Ok(parsed)) => (parsed, true),
            _ => return Err(Refused::Unparsed(why.to_string())),
        },
    };
    let [statement] = parsed.as_slice() else {
        return Err(Refused::Unparsed(format!(
            "{} statements in one literal",
            parsed.len()
        )));
    };
    let writes = writes(statement);
    let relations = relations(statement, &writes);
    if relations.is_empty() {
        return Err(Refused::NoRelation);
    }
    Ok(Stmt {
        file: file.to_string(),
        line: lit.line,
        end_line: lit.end_line,
        kind: kind(statement),
        sql: collapse(bare),
        reads: relations
            .iter()
            .filter(|r| !writes.iter().any(|w| w.may_be(r)))
            .cloned()
            .collect(),
        columns: columns(statement, &relations, writes.first()),
        writes,
        parameterized,
        by_generic,
    })
}

/// A leading `--` comment is a header, not a reason to refuse the statement
/// under it.
fn strip_leading_comments(text: &str) -> &str {
    let mut rest = text.trim();
    while let Some(after) = rest.strip_prefix("--") {
        match after.find('\n') {
            Some(n) => rest = after[n..].trim_start(),
            // A comment with nothing under it is not a statement.
            None => return "",
        }
    }
    rest
}

/// Whether the text reads like a query: it opens with a word that starts one
/// *and* carries the clause that word needs.
///
/// The gate rather than the answer — a literal that passes is still handed to
/// the grammar, which is what decides. The second half of the gate is what
/// makes it worth having. A user interface is full of strings that open with
/// one of these words: a `Delete` on a button, a `Select a workspace` in a
/// placeholder, an `Update available` in a banner. On one real tree the
/// opening word alone sent 178 of them to the parser, and a report naming 178
/// strings that were never queries explains nothing. A statement about a
/// table always names the table somewhere, and the keyword that introduces it
/// is the cheapest proof there is.
fn opens_a_query(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let opener: &str = &lower[..lower
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(lower.len())];
    let needs = match opener {
        "select" | "delete" | "with" => "from",
        "insert" | "replace" | "upsert" => "into",
        "update" => "set",
        _ => return false,
    };
    has_word(&lower, needs)
}

/// Whether `word` stands alone somewhere in already-lowercased `text`.
fn has_word(text: &str, word: &str) -> bool {
    let alnum = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    text.match_indices(word).any(|(at, _)| {
        !alnum(text[..at].chars().next_back()) && !alnum(text[at + word.len()..].chars().next())
    })
}

/// Every interpolation hole replaced by a bind parameter.
///
/// `${…}`, `#{…}`, `{…}`, `$name` and `%s` stand where a *value* goes far
/// more often than not, and a grammar shown the hole refuses the whole
/// statement — which is exactly the query a reader most wants read. A `?` in
/// its place reads the text the way the driver will see it. Where the hole
/// stood in an identifier position instead, `FROM ?` is not a statement and
/// the parse fails: the honest outcome, since a table named at runtime is not
/// a fact about this tree.
///
/// Holes inside a SQL string literal are left alone, so a `'{"a":1}'` is
/// still the JSON it was. `$1` is left alone too — it is already a parameter.
fn holes(text: &str) -> (String, bool) {
    let mut out = String::with_capacity(text.len());
    let mut quoted = false;
    let mut filled = false;
    let mut chars = text.char_indices().peekable();
    while let Some((at, ch)) = chars.next() {
        if ch == '\'' {
            quoted = !quoted;
            out.push(ch);
            continue;
        }
        if quoted {
            out.push(ch);
            continue;
        }
        let rest = &text[at..];
        if let Some(len) = brace_hole(rest) {
            out.push('?');
            filled = true;
            advance(&mut chars, len - ch.len_utf8());
            continue;
        }
        if let Some(len) = named_hole(rest) {
            out.push('?');
            filled = true;
            advance(&mut chars, len - ch.len_utf8());
            continue;
        }
        out.push(ch);
    }
    (out, filled)
}

/// The byte length of a `${…}`, `#{…}` or `{…}` hole at the front of `rest`.
fn brace_hole(rest: &str) -> Option<usize> {
    let lead = ["${", "#{", "{"]
        .into_iter()
        .find(|p| rest.starts_with(p))?
        .len();
    // `{{` is an escaped brace in every formatter that uses `{}`, so it is
    // text rather than a hole.
    if rest[lead..].starts_with('{') {
        return None;
    }
    rest[lead..].find('}').map(|end| lead + end + 1)
}

/// The byte length of a `$name` or `%s` hole at the front of `rest`.
fn named_hole(rest: &str) -> Option<usize> {
    if let Some(after) = rest.strip_prefix('%') {
        let one = after.chars().next()?;
        return matches!(one, 's' | 'd' | 'f').then_some(2);
    }
    // `$1` is a parameter already; `$name` is Kotlin interpolating.
    let after = rest.strip_prefix('$')?;
    let name: String = after
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    let starts_a_name = name.starts_with(|c: char| c.is_alphabetic() || c == '_');
    starts_a_name.then_some(1 + name.len())
}

/// Skip `bytes` more of the iterator, having already taken the first char.
fn advance(chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>, mut bytes: usize) {
    while bytes > 0 {
        match chars.next() {
            Some((_, ch)) => bytes = bytes.saturating_sub(ch.len_utf8()),
            None => return,
        }
    }
}

/// One line, one space between words: a statement written across ten lines of
/// Java reads as a statement rather than as its indentation.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn kind(statement: &Statement) -> &'static str {
    match statement {
        Statement::Query(_) => "select",
        Statement::Insert(_) => "insert",
        Statement::Update(_) => "update",
        Statement::Delete(_) => "delete",
        _ => "other",
    }
}

/// Every relation the statement names, subqueries and CTEs included.
///
/// Not every `ObjectName` in a relation position is a relation: a
/// table-valued function sits exactly where a table does — `FROM
/// generate_series(1, 10)`, `FROM unnest($1)` — and names no table at all.
/// Reading one as a table would mint a `Statement` node whose every edge then
/// fails to resolve, which is a literal with a node around it.
fn relations(statement: &Statement, writes: &[Name]) -> Vec<Name> {
    let mut found = Relations(writes.to_vec());
    let _: ControlFlow<()> = statement.visit(&mut found);
    found.0
}

/// The relations a statement reads, gathered where the factor says whether it
/// is a table or a call.
struct Relations(Vec<Name>);

impl Visitor for Relations {
    type Break = ();

    fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<()> {
        if let TableFactor::Table {
            name, args: None, ..
        } = factor
        {
            let name = name_of(name);
            if !self.0.contains(&name) {
                self.0.push(name);
            }
        }
        ControlFlow::Continue(())
    }
}

/// The tables the statement writes to. Its own target only: a table in a
/// `FROM` beside it supplies rows and is read.
fn writes(statement: &Statement) -> Vec<Name> {
    match statement {
        Statement::Insert(Insert {
            table: TableObject::TableName(name),
            ..
        }) => vec![name_of(name)],
        Statement::Update(Update { table, .. }) => {
            factor_name(&table.relation).into_iter().collect()
        }
        Statement::Delete(Delete { tables, from, .. }) if !tables.is_empty() => {
            tables.iter().map(name_of).collect()
        }
        Statement::Delete(Delete { from, .. }) => {
            let (FromTable::WithFromKeyword(from) | FromTable::WithoutKeyword(from)) = from;
            factors(from).into_iter().map(|(name, _)| name).collect()
        }
        _ => Vec::new(),
    }
}

/// The table a factor names, where it names one rather than computing rows.
fn factor_name(factor: &TableFactor) -> Option<Name> {
    match factor {
        TableFactor::Table { name, .. } => Some(name_of(name)),
        _ => None,
    }
}

/// `(table, alias)` for every plain table in these `FROM` clauses.
fn factors(from: &[TableWithJoins]) -> Vec<(Name, Option<String>)> {
    let mut out = Vec::new();
    for item in from {
        let relations =
            std::iter::once(&item.relation).chain(item.joins.iter().map(|j| &j.relation));
        for factor in relations {
            if let TableFactor::Table { name, alias, .. } = factor {
                out.push((name_of(name), alias.as_ref().map(|a| a.name.value.clone())));
            }
        }
    }
    out
}

/// The outermost `SELECT`, which is the one whose projection the caller sees.
fn first_select(statement: &Statement) -> Option<&Select> {
    let query = match statement {
        Statement::Query(query) => query.as_ref(),
        Statement::Insert(Insert {
            source: Some(query),
            ..
        }) => query.as_ref(),
        _ => return None,
    };
    let mut body = query.body.as_ref();
    // A union's shape is its first branch's.
    while let SetExpr::SetOperation { left, .. } = body {
        body = left.as_ref();
    }
    match body {
        SetExpr::Select(select) => Some(select),
        _ => None,
    }
}

/// Every alias in scope at the top level, as `alias -> table`.
fn aliases(statement: &Statement) -> Vec<(String, Name)> {
    let mut from: Vec<(Name, Option<String>)> = Vec::new();
    if let Some(select) = first_select(statement) {
        from.extend(factors(&select.from));
    }
    match statement {
        Statement::Update(Update { table, .. }) => {
            from.extend(factors(std::slice::from_ref(table)))
        }
        Statement::Delete(Delete { from: f, using, .. }) => {
            let (FromTable::WithFromKeyword(f) | FromTable::WithoutKeyword(f)) = f;
            from.extend(factors(f));
            if let Some(using) = using {
                from.extend(factors(using));
            }
        }
        _ => {}
    }
    from.into_iter()
        .filter_map(|(name, alias)| alias.map(|a| (a, name)))
        .collect()
}

/// A column reference, as the text qualified it.
type Written = (Option<String>, String);

/// Every column the statement names, each with the role it was named in and
/// the table it belongs to where that can be said.
fn columns(statement: &Statement, relations: &[Name], target: Option<&Name>) -> Vec<ColumnUse> {
    let aliases = aliases(statement);
    let mut seen: Vec<(Written, Role)> = Vec::new();
    let mut note = |written: Written, role: Role| {
        if !seen.iter().any(|(w, _)| *w == written) {
            seen.push((written, role));
        }
    };
    for item in projection(statement) {
        if let Some(written) = written_of(item) {
            note(written, Role::Projection);
        }
    }
    for written in assigned(statement) {
        note(written, Role::Assignment);
    }
    let _: ControlFlow<()> = visit_expressions(statement, |expr| {
        if let Some(written) = written_of(expr) {
            note(written, Role::Reference);
        }
        ControlFlow::Continue(())
    });
    seen.into_iter()
        .map(|((qualifier, column), role)| {
            let owner = match (&qualifier, role) {
                (Some(q), _) => resolve(q, &aliases, relations),
                // An assignment with no qualifier belongs to the statement's
                // own target, whatever else the statement reads.
                (None, Role::Assignment) => target.cloned(),
                // Otherwise only a statement naming exactly one relation says
                // whose a bare column is. Two and a guess would be wrong half
                // the time.
                (None, _) => relations.first().filter(|_| relations.len() == 1).cloned(),
            };
            (owner, column, role)
        })
        .collect()
}

/// The table a qualifier denotes: an alias first, then a table named outright.
fn resolve(qualifier: &str, aliases: &[(String, Name)], relations: &[Name]) -> Option<Name> {
    if let Some((_, name)) = aliases
        .iter()
        .find(|(a, _)| a.eq_ignore_ascii_case(qualifier))
    {
        return Some(name.clone());
    }
    relations
        .iter()
        .find(|r| r.name.eq_ignore_ascii_case(qualifier))
        .cloned()
}

/// The expressions the caller gets back: a projection, or a `RETURNING`.
fn projection(statement: &Statement) -> Vec<&Expr> {
    let returning = match statement {
        Statement::Insert(Insert { returning, .. })
        | Statement::Update(Update { returning, .. })
        | Statement::Delete(Delete { returning, .. }) => returning.as_deref(),
        _ => None,
    };
    let select = first_select(statement).map(|s| s.projection.as_slice());
    [select, returning]
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(projected)
        .collect()
}

/// The expression a projected item names, where it names one rather than
/// computing something a column node could not stand for.
fn projected(item: &SelectItem) -> Option<&Expr> {
    match item {
        SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => Some(expr),
        _ => None,
    }
}

/// The columns the statement assigns to.
fn assigned(statement: &Statement) -> Vec<Written> {
    let names = |objects: &[sqlparser::ast::ObjectName]| -> Vec<Written> {
        objects
            .iter()
            .map(|o| {
                let name = name_of(o);
                (name.schema, name.name)
            })
            .collect()
    };
    match statement {
        Statement::Insert(Insert { columns, .. }) => names(columns),
        Statement::Update(Update { assignments, .. }) => assignments
            .iter()
            .flat_map(|a| match &a.target {
                AssignmentTarget::ColumnName(one) => names(std::slice::from_ref(one)),
                AssignmentTarget::Tuple(many) => names(many),
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The column an expression names, as the text qualified it.
fn written_of(expr: &Expr) -> Option<Written> {
    match expr {
        Expr::Identifier(id) => Some((None, id.value.clone())),
        Expr::CompoundIdentifier(parts) => match parts.as_slice() {
            [.., qualifier, column] => Some((Some(qualifier.value.clone()), column.value.clone())),
            _ => None,
        },
        _ => None,
    }
}
