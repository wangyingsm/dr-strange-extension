//! What a statement touches, read from its text by a tolerant scanner.
//!
//! Not a SQL parser, deliberately. A MyBatis statement is assembled from
//! branches with holes in it — `${…}` pastes text a parser cannot see past —
//! and a grammar that refuses the whole statement refuses exactly the one a
//! reader most wants read. So this scans for the shapes it is sure of and
//! names what it could not bind, rather than failing or guessing.

/// How a table was introduced, which is what says whether the statement reads
/// it or writes it — more precise than the statement's own kind, since an
/// `INSERT … SELECT … FROM other` does both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

/// What one statement's text says it touches.
#[derive(Debug, Default)]
pub struct Touched {
    /// Tables as written, in the order they appear, each with how it is used.
    pub tables: Vec<(String, Access)>,
    /// Columns bound to a table — by an alias, a qualifier, or because the
    /// statement names exactly one table.
    pub bound: Vec<(String, String)>,
    /// Columns the text names without saying whose they are. Kept as names so
    /// a reader sees them, with no edge that would put them under a guess.
    pub unbound: Vec<String>,
    /// A table position holding a `${…}` interpolation: the table is named at
    /// runtime, so this statement's tables are an incomplete list.
    pub interpolated_table: bool,
}

/// One scanned token. Everything a scanner needs to tell apart.
#[derive(Debug, PartialEq, Eq)]
enum Tok {
    /// A name, possibly qualified (`users`, `u.id`, `` `users` ``).
    Name(String),
    /// A `"…"` name: an identifier in PostgreSQL and standard SQL, a string
    /// literal in MySQL's default mode. The same bytes, two meanings, so the
    /// statement has to say which — see [`Scan::double_quoted_ids`].
    Quoted(String),
    /// A `${…}` hole: text pasted at runtime, opaque here — and it may be an
    /// identifier, which is why a table position has to admit it.
    Interp,
    /// A `#{…}` bind parameter: a *value* the driver sends separately. It can
    /// never be an identifier, so it is read as one opaque token and nothing
    /// inside it is a column.
    Bind,
    /// PostgreSQL's `::` cast. What follows names a type, not a column.
    Cast,
    Open,
    Close,
    Comma,
    Star,
    /// Anything else — an operator, a literal, a placeholder.
    Other,
}

/// The words a scanner must not read as a column or a table name: SQL's own,
/// across the dialects a mapper may target, plus the pseudo-columns and
/// niladic functions that stand bare where a column could (`SYSDATE`,
/// `ROWNUM`, `CURRENT_TIMESTAMP`, `DUAL`).
///
/// Not the whole of any dialect, and it does not need to be: a word this list
/// misses becomes a name on the statement rather than an edge to a table,
/// which is the failure this design prefers.
///
/// **Sorted**, and a test holds it so: the lookup binary-searches rather than
/// walking all of them for every word in every statement.
const KEYWORDS: &[&str] = &[
    "all",
    "and",
    "any",
    "apply",
    "as",
    "asc",
    "between",
    "binary",
    "by",
    "case",
    "cast",
    "collate",
    "conflict",
    "connect",
    "convert",
    "cross",
    "current_date",
    "current_time",
    "current_timestamp",
    "current_user",
    "default",
    "delete",
    "desc",
    "distinct",
    "div",
    "do",
    "dual",
    "duplicate",
    "else",
    "end",
    "escape",
    "except",
    "exists",
    "false",
    "fetch",
    "filter",
    "first",
    "for",
    "force",
    "from",
    "full",
    "group",
    "having",
    "holdlock",
    "ignore",
    "ilike",
    "in",
    "index",
    "inner",
    "insert",
    "intersect",
    "interval",
    "into",
    "is",
    "join",
    "key",
    "last",
    "lateral",
    "left",
    "level",
    "like",
    "limit",
    "localtime",
    "localtimestamp",
    "lock",
    "locked",
    "merge",
    "minus",
    "mod",
    "mode",
    "natural",
    "next",
    "nolock",
    "not",
    "nothing",
    "nowait",
    "null",
    "nulls",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "outer",
    "output",
    "over",
    "paglock",
    "partition",
    "percent",
    "prior",
    "readcommitted",
    "readpast",
    "readuncommitted",
    "recursive",
    "regexp",
    "repeatableread",
    "replace",
    "returning",
    "right",
    "rlike",
    "row",
    "rowid",
    "rowlock",
    "rownum",
    "rows",
    "select",
    "separator",
    "serializable",
    "session_user",
    "set",
    "share",
    "similar",
    "skip",
    "snapshot",
    "some",
    "soundex",
    "sounds",
    "start",
    "straight_join",
    "sysdate",
    "systimestamp",
    "tablock",
    "tablockx",
    "then",
    "ties",
    "top",
    "true",
    "union",
    "update",
    "updlock",
    "use",
    "user",
    "using",
    "value",
    "values",
    "when",
    "where",
    "window",
    "with",
    "within",
    "xlock",
    "xor",
];

/// The longest keyword there is. A longer word cannot be one, which is the
/// cheapest test available and rejects most identifiers outright.
const LONGEST_KEYWORD: usize = 17;

/// Is this one of SQL's own words? Case-insensitive, and without allocating a
/// lowercased copy of every word in the statement: the comparison runs in a
/// stack buffer, over a sorted list.
fn is_keyword(word: &str) -> bool {
    if word.len() > LONGEST_KEYWORD {
        return false;
    }
    let mut lower = [0u8; LONGEST_KEYWORD];
    for (i, b) in word.bytes().enumerate() {
        lower[i] = b.to_ascii_lowercase();
    }
    match std::str::from_utf8(&lower[..word.len()]) {
        Ok(w) => KEYWORDS.binary_search(&w).is_ok(),
        Err(_) => false,
    }
}

/// The text split into the tokens the passes below walk. Comments and string
/// literals are dropped: a name inside either names nothing.
fn scan(sql: &str) -> Vec<Tok> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        match c {
            // `-- …` and `/* … */` say nothing about tables.
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i = (i + 2).min(b.len());
            }
            // `'…'` is a string literal in every dialect there is.
            b'\'' => {
                i += 1;
                while i < b.len() && b[i] != b'\'' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
                out.push(Tok::Other);
            }
            // Every spelling of a name, bare or quoted, and a dotted chain of
            // them — `orders`, `"orders"`, `` `db`.`orders` ``, `[dbo].[Orders]`,
            // `myschema."orders"` — is one name.
            b'"' | b'`' | b'[' => {
                let (name, next, ambiguous) = name_at(b, sql, i);
                i = next;
                out.push(if ambiguous {
                    Tok::Quoted(name)
                } else {
                    Tok::Name(name)
                });
            }
            // `${…}`: a hole whose contents are not identifiers of this text.
            b'$' if b.get(i + 1) == Some(&b'{') => {
                i += 2;
                while i < b.len() && b[i] != b'}' {
                    i += 1;
                }
                i += 1;
                out.push(Tok::Interp);
            }
            // `#{…}`: MyBatis's bind parameter, kept as written in the stored
            // SQL and read here as the value it stands for.
            b'#' if b.get(i + 1) == Some(&b'{') => {
                i += 2;
                while i < b.len() && b[i] != b'}' {
                    i += 1;
                }
                i += 1;
                out.push(Tok::Bind);
            }

            b':' if b.get(i + 1) == Some(&b':') => {
                out.push(Tok::Cast);
                i += 2;
            }
            b'(' => {
                out.push(Tok::Open);
                i += 1;
            }
            b')' => {
                out.push(Tok::Close);
                i += 1;
            }
            b',' => {
                out.push(Tok::Comma);
                i += 1;
            }
            b'*' => {
                out.push(Tok::Star);
                i += 1;
            }
            _ if c.is_ascii_alphabetic() || c == b'_' => {
                let (name, next, _) = name_at(b, sql, i);
                i = next;
                out.push(Tok::Name(name));
            }
            _ if c.is_ascii_whitespace() => i += 1,
            // An operator, a number, a `?` placeholder: one token, and the
            // passes below only care that it is not a name.
            _ => {
                out.push(Tok::Other);
                i += 1;
            }
        }
    }
    out
}

/// A name at `at`: a dotted chain of parts, each bare or quoted in any of the
/// three ways a dialect spells it. Returns the joined name, where to carry on
/// from, and whether it is **ambiguous** — true only for a lone `"…"`, which
/// in MySQL's default mode is a string. A chain of two or more parts, or any
/// other quoting, can only be an identifier.
fn name_at(b: &[u8], sql: &str, at: usize) -> (String, usize, bool) {
    let mut name = String::new();
    let mut i = at;
    let mut parts = 0usize;
    let mut only_double_quoted = true;
    loop {
        let (part, next) = match b.get(i) {
            Some(b'"') => (quoted(b, sql, i, b'"').0, quoted(b, sql, i, b'"').1),
            Some(b'`') => {
                only_double_quoted = false;
                (quoted(b, sql, i, b'`').0, quoted(b, sql, i, b'`').1)
            }
            Some(b'[') => {
                only_double_quoted = false;
                (quoted(b, sql, i, b']').0, quoted(b, sql, i, b']').1)
            }
            Some(c) if c.is_ascii_alphanumeric() || *c == b'_' || *c == b'$' => {
                only_double_quoted = false;
                let start = i;
                let mut j = i;
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] == b'$')
                {
                    j += 1;
                }
                (sql[start..j].to_string(), j)
            }
            _ => break,
        };
        if parts > 0 {
            name.push('.');
        }
        name.push_str(&part);
        parts += 1;
        i = next;
        // A dot continues the chain only when a name follows it, in any of the
        // spellings — `s."region_id"`, `` `db`.`t` ``, `[dbo].[T]`.
        let continues = match b.get(i + 1) {
            Some(&c) => {
                c.is_ascii_alphanumeric() || c == b'_' || c == b'"' || c == b'`' || c == b'['
            }
            None => false,
        };
        if b.get(i) == Some(&b'.') && continues {
            i += 1;
            continue;
        }
        break;
    }
    (name, i, parts == 1 && only_double_quoted)
}

/// The text inside a quote that opens at `at`, and where to carry on from. A
/// doubled quote is an escaped one and stays inside the name, which is how
/// both PostgreSQL and MySQL spell it.
fn quoted(b: &[u8], sql: &str, at: usize, quote: u8) -> (String, usize) {
    let mut name = String::new();
    let mut i = at + 1;
    while i < b.len() {
        if b[i] == quote {
            if b.get(i + 1) == Some(&quote) {
                name.push(quote as char);
                i += 2;
                continue;
            }
            i += 1;
            break;
        }
        let start = i;
        while i < b.len() && b[i] != quote {
            i += 1;
        }
        name.push_str(&sql[start..i]);
    }
    (name, i)
}

/// What the table pass builds, and what the column pass then reads.
struct Scan {
    touched: Touched,
    /// alias (or the name itself) → the table it stands for.
    names: Vec<(String, String)>,
    /// This statement has used `"…"` where only an identifier can go, so its
    /// other `"…"` are identifiers too. Without that evidence they are left
    /// alone: in MySQL's default mode they are strings, and a column invented
    /// from a string literal is worse than a column named and not placed.
    double_quoted_ids: bool,
}

/// What the statement touches: tables first, because a column binds to one.
pub fn touched(sql: &str) -> Touched {
    let toks = scan(sql);
    let mut scan = Scan {
        touched: Touched::default(),
        names: Vec::new(),
        double_quoted_ids: false,
    };

    let mut i = 0usize;
    // Whether the statement's verb writes, which is what `FROM` means after a
    // `DELETE` and what a bare `DELETE t` / `INSERT t` relies on.
    let mut writing = false;
    while i < toks.len() {
        if let Tok::Name(word) = &toks[i] {
            let is = |kw: &str| word.eq_ignore_ascii_case(kw);
            // `DELETE FROM t` writes, every other `FROM` reads. `USING` is
            // MERGE's source — and `JOIN … USING (col)` puts a `(` there,
            // which the table walk declines.
            let access = if is("from") {
                Some(if writing { Access::Write } else { Access::Read })
            } else if is("join") || is("using") {
                Some(Access::Read)
            } else if is("into") || is("update") || is("merge") {
                Some(Access::Write)
            } else {
                None
            };
            // Oracle and T-SQL both let the verb stand alone: `DELETE t`,
            // `INSERT t VALUES (…)`. The table is then the next word.
            if is("delete") || is("insert") {
                writing = true;
                if let Some(Tok::Name(next)) = toks.get(i + 1)
                    && !is_keyword(next)
                {
                    i = tables_at(&toks, i + 1, Access::Write, &mut scan);
                    continue;
                }
            } else if access.is_none() && is_keyword(word) {
                writing = false;
            }
            if let Some(access) = access {
                i = tables_at(&toks, i + 1, access, &mut scan);
                continue;
            }
        }
        i += 1;
    }

    // One table, and every bare column in the statement is its own.
    let sole = (scan.touched.tables.len() == 1 && !scan.touched.interpolated_table)
        .then(|| scan.touched.tables[0].0.clone());
    let mut bound: Vec<(String, String)> = Vec::new();
    let mut unbound: Vec<String> = Vec::new();
    for (i, tok) in toks.iter().enumerate() {
        let (word, ambiguous) = match tok {
            Tok::Name(w) => (w, false),
            Tok::Quoted(w) => (w, true),
            _ => continue,
        };
        if is_keyword(word) {
            continue;
        }
        // A name followed by `(` is a function, not a column.
        if toks.get(i + 1) == Some(&Tok::Open) {
            continue;
        }
        // After `AS` comes an output alias or a cast's type — neither is a
        // column of any table. After `::`, a type.
        if toks.get(i.wrapping_sub(1)) == Some(&Tok::Cast)
            || matches!(toks.get(i.wrapping_sub(1)), Some(Tok::Name(w)) if w.eq_ignore_ascii_case("as"))
        {
            continue;
        }
        // A table name, or an alias standing for one, is not a column — which
        // has to be settled before the quoting rule, since an alias may be
        // quoted too.
        if scan.names.iter().any(|(n, _)| n == word) {
            continue;
        }
        // A lone `"…"` counts as a column only where the statement has shown
        // that is what its quotes mean; otherwise it is named on the statement
        // rather than bound to a table it may not belong to.
        if ambiguous && !scan.double_quoted_ids {
            unbound.push(word.clone());
            continue;
        }
        // `myschema.orders.id` qualifies with everything up to the last dot,
        // which is how a schema-qualified table is written.
        match word.rsplit_once('.') {
            Some((qualifier, column)) => match scan.names.iter().find(|(n, _)| n == qualifier) {
                Some((_, table)) => bound.push((table.clone(), column.to_string())),
                // A qualifier naming no table this statement declares — a
                // schema, or a table named by a `${…}`.
                None => unbound.push(column.to_string()),
            },
            None => match &sole {
                Some(table) => bound.push((table.clone(), word.clone())),
                None => unbound.push(word.clone()),
            },
        }
    }
    bound.sort();
    bound.dedup();
    unbound.sort();
    unbound.dedup();
    let mut t = scan.touched;
    t.bound = bound;
    t.unbound = unbound;
    t
}

/// One name standing for a table, recorded once. A statement that repeats a
/// `FROM` — a long `UNION ALL`, a generated `<foreach>` — would otherwise grow
/// this list without bound, and the column pass searches it per token.
fn remember(names: &mut Vec<(String, String)>, name: &str, table: &str) {
    if !names.iter().any(|(n, _)| n == name) {
        names.push((name.to_string(), table.to_string()));
    }
}

/// The table list after a `FROM` / `JOIN` / `INTO` / `UPDATE`, with aliases.
/// Returns where to carry on from.
fn tables_at(toks: &[Tok], mut i: usize, access: Access, scan: &mut Scan) -> usize {
    loop {
        // A string literal cannot stand where a table does, so a `"…"` here
        // settles what this statement's quotes mean.
        let named = match toks.get(i) {
            Some(Tok::Name(w)) if !is_keyword(w) => Some(w.clone()),
            Some(Tok::Quoted(w)) => {
                scan.double_quoted_ids = true;
                Some(w.clone())
            }
            _ => None,
        };
        match (named, toks.get(i)) {
            // `FROM (SELECT …)` — a subquery is not a table. Its own `FROM`
            // is reached by the walk that follows.
            (_, Some(Tok::Open)) => return i + 1,
            (_, Some(Tok::Interp)) => {
                scan.touched.interpolated_table = true;
                i += 1;
            }
            (Some(table), _) => {
                if !scan
                    .touched
                    .tables
                    .iter()
                    .any(|(n, a)| *n == table && *a == access)
                {
                    scan.touched.tables.push((table.clone(), access));
                }
                remember(&mut scan.names, &table, &table);
                i += 1;
                // `users u` or `users AS u`.
                if let Some(Tok::Name(next)) = toks.get(i)
                    && next.eq_ignore_ascii_case("as")
                {
                    i += 1;
                }
                match toks.get(i) {
                    Some(Tok::Name(alias)) if !is_keyword(alias) && !alias.contains('.') => {
                        remember(&mut scan.names, alias, &table);
                        i += 1;
                    }
                    Some(Tok::Quoted(alias)) => {
                        remember(&mut scan.names, alias, &table);
                        i += 1;
                    }
                    _ => {}
                }
            }
            _ => return i,
        }
        // A comma continues the list; anything else ends it.
        match toks.get(i) {
            Some(Tok::Comma) => i += 1,
            _ => return i,
        }
    }
}

#[cfg(test)]
mod tests;
