//! The MyBatis mapper dialect: a `<mapper namespace="…">` whose statements are
//! the other half of a Java interface.
//!
//! The namespace *is* the interface's qualified name and a statement's `id` is
//! the method's, so `namespace.id` is the key the java plugin already minted
//! for that method. The statement names it as `Method` + `External` — an
//! assertion that the key belongs to something this reader did not parse — and
//! the host's merge makes the two meet: where the interface was parsed, its
//! declaration outranks the stand-in and keeps its label; where it was not,
//! the edge still stands and says so, rather than being dropped as dangling.

use crate::sql;
use dr_strange_ext::{NodeBuilder, Output, edge, node};
use quick_xml::events::Event;

/// A statement's `id`: unique within its mapper, and the name of the Java
/// method it implements.
#[derive(Clone, PartialEq, Eq)]
struct StatementId(String);

/// A `<sql>` fragment's `id` — what an `<include refid="…">` has to match.
/// Distinct from a statement's: the two namespaces are separate, and comparing
/// one to the other is a mistake worth making impossible.
#[derive(Clone, PartialEq, Eq)]
struct FragmentId(String);

/// A `<resultMap>`'s `id` — what a statement's `resultMap` attribute matches.
#[derive(Clone, PartialEq, Eq)]
struct MapId(String);

/// The Java interface a mapper declares itself against.
#[derive(Clone, Default)]
struct Namespace(String);

impl Namespace {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The key the java plugin minted for that method: the interface's
    /// qualified name, then the statement's id. MyBatis's own rule.
    fn method(&self, id: &StatementId) -> String {
        format!("{}.{}", self.0, id.0)
    }
}

/// Which element a statement is written as. MyBatis has four, and
/// `<selectKey>` is deliberately not among them: it runs *with* its insert,
/// and its text rides that statement's SQL.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Select,
    Insert,
    Update,
    Delete,
}

impl Kind {
    fn of(element: &str) -> Option<Self> {
        match element {
            "select" => Some(Self::Select),
            "insert" => Some(Self::Insert),
            "update" => Some(Self::Update),
            "delete" => Some(Self::Delete),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Select => "select",
            Self::Insert => "insert",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }
}

/// Where a declaration sits in its file — the two lines travel together, and
/// one without the other says nothing useful.
#[derive(Clone, Copy)]
struct Span {
    line: u64,
    end_line: u64,
}

/// The attributes a statement writes down, each named rather than carried as
/// a pair whose key has to be re-spelled on the way out.
#[derive(Default)]
struct Attrs {
    parameter_type: Option<String>,
    result_type: Option<String>,
    /// `resultMap="a,b"` — MyBatis allows several, comma-separated.
    result_maps: Vec<MapId>,
    /// `databaseId`: the same `id` may be written once per database, and the
    /// container picks by the configured vendor. Two statements, two nodes.
    database_id: Option<String>,
}

impl Attrs {
    fn read(attr: impl Fn(&str) -> Option<String>) -> Self {
        Self {
            parameter_type: attr("parameterType"),
            result_type: attr("resultType"),
            result_maps: attr("resultMap")
                .map(|v| {
                    v.split(',')
                        .map(str::trim)
                        .filter(|p| !p.is_empty())
                        .map(|p| MapId(p.to_string()))
                        .collect()
                })
                .unwrap_or_default(),
            database_id: attr("databaseId"),
        }
    }

    /// Each one onto the node under the property name the graph spells it
    /// with — snake_case, whatever the XML wrote.
    fn write(&self, mut n: NodeBuilder) -> NodeBuilder {
        if let Some(v) = &self.parameter_type {
            n = n.prop("parameter_type", v.clone());
        }
        if let Some(v) = &self.result_type {
            n = n.prop("result_type", v.clone());
        }
        if !self.result_maps.is_empty() {
            let named: Vec<String> = self.result_maps.iter().map(|m| m.0.clone()).collect();
            n = n.prop("result_map", named.join(", "));
        }
        if let Some(v) = &self.database_id {
            n = n.described(
                "database_id",
                "the vendor this statement is written for; its id is shared with \
                 the statements written for the others",
                v.clone(),
            );
        }
        n
    }
}

/// One piece of a statement's body: text as written, or an unresolved
/// `<include refid="…">` waiting for the fragment it names.
enum Part {
    Text(String),
    Include(FragmentId),
}

/// A statement as read, before its includes are spliced.
struct Statement {
    id: StatementId,
    kind: Kind,
    parts: Vec<Part>,
    attrs: Attrs,
    span: Span,
}

/// A reusable `<sql>` body, by the id an include names it with.
struct Fragment {
    id: FragmentId,
    parts: Vec<Part>,
}

/// A `<resultMap>`: the columns its mappings name, which is the only answer
/// for a `SELECT *`.
struct ResultMap {
    id: MapId,
    columns: Vec<String>,
    /// `extends="other"` — the columns of the map it extends are its own too.
    extends: Option<MapId>,
}

/// Where the event just read ends — its closing `>`, which is the line the
/// element is written on. The position *before* a read is the previous event's
/// end, which for a statement after a blank line is the wrong line entirely.
fn here(reader: &quick_xml::Reader<&[u8]>) -> usize {
    (reader.buffer_position() as usize).saturating_sub(1)
}

/// Byte offsets of every line start, so an event's position becomes a line.
struct Lines(Vec<usize>);

impl Lines {
    fn of(text: &str) -> Self {
        let mut starts = vec![0usize];
        starts.extend(
            text.bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i + 1),
        );
        Self(starts)
    }

    /// The 1-based line holding `byte`.
    fn at(&self, byte: usize) -> u64 {
        match self.0.binary_search(&byte) {
            Ok(i) => i as u64 + 1,
            Err(i) => i as u64,
        }
    }
}

/// What the reader could not do, said in numbers rather than left silent.
#[derive(Default)]
pub struct Bounds {
    pub statements: usize,
    /// `refid`s naming a fragment this file does not hold — a cross-file
    /// include, which needs every mapper at once and is not resolved here.
    pub foreign_includes: usize,
    /// `${…}` interpolations: text pasted into the SQL, which may be the table
    /// name itself. Left exactly as written.
    pub interpolations: usize,
    /// Tables a statement reads or writes, as its text names them.
    pub tables: usize,
    /// Columns bound to a table — by a qualifier, or because the statement
    /// names exactly one.
    pub columns: usize,
    /// Columns the text names without saying whose they are. They ride the
    /// statement, with no edge that would put them under a guess.
    pub unbound_columns: usize,
    /// Statements whose table is named by a `${…}` at runtime.
    pub interpolated_tables: usize,
}

/// What one mapper document declares, before any of it becomes facts.
struct Parsed {
    namespace: Namespace,
    fragments: Vec<Fragment>,
    statements: Vec<Statement>,
    result_maps: Vec<ResultMap>,
}

/// One mapper file into facts. Returns what it could not resolve.
pub fn read(path: &str, text: &str, out: &mut Output) -> Bounds {
    let mut bounds = Bounds::default();
    let parsed = walk(text);

    out.nodes.push(
        node(path, "Mapper")
            .described(
                "namespace",
                "the Java interface this mapper's statements implement",
                parsed.namespace.0.clone(),
            )
            .prop("file", path)
            .build(),
    );
    for s in &parsed.statements {
        emit(path, &parsed, s, out, &mut bounds);
    }
    bounds
}

/// What the walk is in the middle of reading: at most one of each, and text
/// belongs to whichever is open.
#[derive(Default)]
struct Open {
    statement: Option<Statement>,
    fragment: Option<Fragment>,
    result_map: Option<ResultMap>,
    /// The text a dynamic tag owes on the way out — `</trim>`'s suffix,
    /// `</foreach>`'s close — paired with the element that owes it, so nesting
    /// unwinds in order.
    closers: Vec<(String, String)>,
}

/// The document's own structure, read once: the namespace, the `<sql>`
/// fragments an `<include>` will want, the statements, and the result maps.
fn walk(text: &str) -> Parsed {
    let lines = Lines::of(text);
    let mut reader = quick_xml::Reader::from_str(text);
    reader.config_mut().trim_text(true);

    let mut p = Parsed {
        namespace: Namespace::default(),
        fragments: Vec::new(),
        statements: Vec::new(),
        result_maps: Vec::new(),
    };
    let mut open = Open::default();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                started(&e, lines.at(here(&reader)), &mut p, &mut open);
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                ended(&name, lines.at(here(&reader)), &mut p, &mut open);
            }
            // Text is unescaped; CDATA is already raw, which is how a
            // statement with a `<` in it is written.
            Ok(Event::Text(t)) => {
                collect(t.unescape().unwrap_or_default().to_string(), &mut open);
            }
            Ok(Event::CData(t)) => {
                collect(String::from_utf8_lossy(&t).to_string(), &mut open);
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    p
}

/// An element opening: the mapper's namespace, a fragment or result map to
/// collect into, an include to splice later, or a statement beginning.
fn started(e: &quick_xml::events::BytesStart<'_>, line: u64, p: &mut Parsed, open: &mut Open) {
    let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
    let attr = |want: &str| -> Option<String> {
        e.attributes().flatten().find_map(|a| {
            (a.key.as_ref() == want.as_bytes())
                .then(|| String::from_utf8_lossy(&a.value).to_string())
        })
    };
    match name.as_str() {
        "mapper" if p.namespace.is_empty() => {
            p.namespace = Namespace(attr("namespace").unwrap_or_default());
        }
        "sql" => {
            if let Some(id) = attr("id") {
                open.fragment = Some(Fragment {
                    id: FragmentId(id),
                    parts: Vec::new(),
                });
            }
        }
        "resultMap" => {
            if let Some(id) = attr("id") {
                open.result_map = Some(ResultMap {
                    id: MapId(id),
                    columns: Vec::new(),
                    extends: attr("extends").map(MapId),
                });
            }
        }
        // Inside a result map every mapping names a column — including the
        // ones that drive a nested select (`<association>`, `<collection>`)
        // and the one a `<discriminator>` switches on.
        "id" | "result" | "idArg" | "arg" | "association" | "collection" | "discriminator"
            if open.result_map.is_some() =>
        {
            if let (Some(map), Some(c)) = (open.result_map.as_mut(), attr("column")) {
                map.columns.push(c);
            }
        }
        // A dynamic tag whose SQL is in its attributes rather than its text:
        // without these a `<set>` loses the word SET and its assignments read
        // as an alias of the table being updated.
        "where" | "set" | "trim" | "foreach" => {
            let (opening, closing) = match name.as_str() {
                "where" => ("WHERE".to_string(), String::new()),
                "set" => ("SET".to_string(), String::new()),
                "trim" => (
                    attr("prefix").unwrap_or_default(),
                    attr("suffix").unwrap_or_default(),
                ),
                _ => (
                    attr("open").unwrap_or_default(),
                    attr("close").unwrap_or_default(),
                ),
            };
            if !opening.is_empty() {
                collect(opening, open);
            }
            open.closers.push((name.clone(), closing));
        }
        "include" => {
            let target = match attr("refid") {
                Some(r) => Part::Include(FragmentId(r)),
                None => Part::Text(String::new()),
            };
            if let Some(s) = open.statement.as_mut() {
                s.parts.push(target);
            } else if let Some(f) = open.fragment.as_mut() {
                f.parts.push(target);
            }
        }
        // A statement opens here; a dynamic tag falls through, contributing its
        // text and not its markup, since the SQL a reader wants is what the
        // branches hold.
        element => {
            if let (Some(kind), Some(id)) = (Kind::of(element), attr("id")) {
                open.statement = Some(Statement {
                    id: StatementId(id),
                    kind,
                    parts: Vec::new(),
                    attrs: Attrs::read(attr),
                    span: Span {
                        line,
                        end_line: line,
                    },
                });
            }
        }
    }
}

/// An element closing: whatever was being collected is done, and a dynamic
/// tag pays the text it owes.
fn ended(name: &str, line: u64, p: &mut Parsed, open: &mut Open) {
    if let Some(i) = open.closers.iter().rposition(|(e, _)| e == name) {
        let (_, closing) = open.closers.remove(i);
        if !closing.is_empty() {
            collect(closing, open);
        }
        return;
    }
    if name == "sql" {
        if let Some(frag) = open.fragment.take() {
            p.fragments.push(frag);
        }
    } else if name == "resultMap" {
        if let Some(map) = open.result_map.take() {
            p.result_maps.push(map);
        }
    } else if Kind::of(name).is_some()
        && let Some(mut s) = open.statement.take()
    {
        s.span.end_line = line;
        p.statements.push(s);
    }
}

impl Parsed {
    /// The columns a result map names, its `extends` chain included. Depth is
    /// capped: a map extending itself must not be able to hang the ingest.
    fn columns_of(&self, want: &MapId, depth: usize) -> Vec<String> {
        if depth > 8 {
            return Vec::new();
        }
        match self.result_maps.iter().find(|m| m.id == *want) {
            Some(m) => {
                let mut cols = m.columns.clone();
                if let Some(base) = &m.extends {
                    cols.extend(self.columns_of(base, depth + 1));
                }
                cols
            }
            None => Vec::new(),
        }
    }
}

/// One statement's facts: the node, the method it implements, and the tables
/// and columns its SQL touches.
fn emit(path: &str, parsed: &Parsed, s: &Statement, out: &mut Output, bounds: &mut Bounds) {
    // A `databaseId` makes the same `id` several statements; the key has to
    // keep them apart or the second would be dropped as a duplicate.
    let key = match &s.attrs.database_id {
        Some(db) => format!("{path}#{}@{db}", s.id.0),
        None => format!("{path}#{}", s.id.0),
    };
    let sql = assemble(&s.parts, &parsed.fragments, bounds);
    bounds.interpolations += sql.matches("${").count();
    let mut touched = sql::touched(&sql);
    // A `SELECT *` names no columns; its result map is where they are written
    // down, so they count as this statement's too.
    let mapped = s
        .attrs
        .result_maps
        .iter()
        .flat_map(|want| parsed.columns_of(want, 0))
        .collect::<Vec<String>>();
    if !mapped.is_empty() {
        let sole = (touched.tables.len() == 1).then(|| touched.tables[0].0.clone());
        for c in mapped {
            match &sole {
                Some(table) => touched.bound.push((table.clone(), c)),
                None => touched.unbound.push(c),
            }
        }
        touched.bound.sort();
        touched.bound.dedup();
        touched.unbound.sort();
        touched.unbound.dedup();
    }

    let mut n = node(&key, "Statement")
        .described(
            "kind",
            "the statement element: select, insert, update or delete",
            s.kind.as_str(),
        )
        .described(
            "sql",
            "the statement as assembled — dynamic branches included, every \
             `#{…}` and `${…}` exactly as the source wrote it",
            sql,
        )
        .prop("file", path)
        .prop("line", s.span.line)
        .prop("end_line", s.span.end_line);
    if !touched.unbound.is_empty() {
        n = n.described(
            "unbound_columns",
            "columns the text names without saying whose they are — several \
             tables and no qualifier, so no edge is drawn",
            touched.unbound.join(", "),
        );
    }
    n = s.attrs.write(n);
    out.nodes.push(n.build());
    out.edges.push(edge(path, "CONTAINS", &key).build());

    if !parsed.namespace.is_empty() {
        let method = parsed.namespace.method(&s.id);
        // The stand-in, not a declaration: the java plugin owns this key.
        out.nodes
            .push(node(&method, "Method").also("External").build());
        out.edges.push(
            edge(&key, "IMPLEMENTS", &method)
                .described(
                    "_resolved_by",
                    "the namespace and id name the method; MyBatis's own rule",
                    "mybatis-namespace",
                )
                .build(),
        );
    }
    for (table, access) in &touched.tables {
        out.nodes
            .push(node(table, "Table").also("External").build());
        let ty = match access {
            sql::Access::Read => "READS",
            sql::Access::Write => "WRITES",
        };
        out.edges.push(edge(&key, ty, table).build());
        bounds.tables += 1;
    }
    for (table, column) in &touched.bound {
        let col = format!("{table}.{column}");
        out.nodes
            .push(node(&col, "Column").also("External").build());
        out.edges.push(edge(table, "HAS_COLUMN", &col).build());
        out.edges.push(edge(&key, "USES_COLUMN", &col).build());
        bounds.columns += 1;
    }
    bounds.unbound_columns += touched.unbound.len();
    if touched.interpolated_table {
        bounds.interpolated_tables += 1;
    }
    bounds.statements += 1;
}

/// Text belongs to whichever element is open — a statement, or the `<sql>`
/// fragment a statement will include. Outside both it is structure, not SQL.
fn collect(raw: String, open: &mut Open) {
    if let Some(s) = open.statement.as_mut() {
        s.parts.push(Part::Text(raw));
    } else if let Some(f) = open.fragment.as_mut() {
        f.parts.push(Part::Text(raw));
    }
}

/// A statement's text, with every `<include>` replaced by the fragment it
/// names **in this file**. A `refid` naming another mapper's fragment is
/// counted, not guessed at.
fn assemble(parts: &[Part], fragments: &[Fragment], bounds: &mut Bounds) -> String {
    let mut sql = String::new();
    for part in parts {
        match part {
            Part::Text(t) => push_sql(&mut sql, t),
            Part::Include(refid) => match fragments.iter().find(|f| f.id == *refid) {
                // One level: a fragment including a fragment is rare, and a
                // cycle must not be able to hang the ingest.
                Some(found) => {
                    for p in &found.parts {
                        if let Part::Text(t) = p {
                            push_sql(&mut sql, t);
                        }
                    }
                }
                None => bounds.foreign_includes += 1,
            },
        }
    }
    sql.trim().to_string()
}

/// Text joined with single spaces, and otherwise exactly as written —
/// `#{…}` and `${…}` included. What a parameter marker looks like is the
/// driver's business; what the source says is this plugin's.
fn push_sql(sql: &mut String, text: &str) {
    for word in text.split_whitespace() {
        if !sql.is_empty() {
            sql.push(' ');
        }
        sql.push_str(word);
    }
}
