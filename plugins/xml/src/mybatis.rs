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

use dr_strange_ext::{Output, edge, node};
use quick_xml::events::Event;

/// The statement elements a mapper declares. `<selectKey>` is deliberately not
/// one: it runs *with* its insert, and its text rides that statement's SQL.
const STATEMENTS: &[&str] = &["select", "insert", "update", "delete"];

/// One piece of a statement's body: text as written, or an unresolved
/// `<include refid="…">` waiting for the fragment it names.
enum Part {
    Text(String),
    Include(String),
}

/// A statement as read, before its includes are spliced.
struct Statement {
    id: String,
    kind: String,
    parts: Vec<Part>,
    attrs: Vec<(String, String)>,
    line: u64,
    end_line: u64,
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
}

/// One mapper file into facts. Returns what it could not resolve.
pub fn read(path: &str, text: &str, out: &mut Output) -> Bounds {
    let lines = Lines::of(text);
    let mut bounds = Bounds::default();
    let mut reader = quick_xml::Reader::from_str(text);
    reader.config_mut().trim_text(true);

    let mut namespace = String::new();
    let mut fragments: Vec<(String, Vec<Part>)> = Vec::new();
    let mut statements: Vec<Statement> = Vec::new();
    // The element whose text is being collected: a statement, or an `<sql>`
    // fragment. Anything outside one of those is structure, not SQL.
    let mut open: Option<Statement> = None;
    let mut in_fragment: Option<(String, Vec<Part>)> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let attr = |want: &str| -> Option<String> {
                    e.attributes().flatten().find_map(|a| {
                        (a.key.as_ref() == want.as_bytes())
                            .then(|| String::from_utf8_lossy(&a.value).to_string())
                    })
                };
                match name.as_str() {
                    "mapper" if namespace.is_empty() => {
                        namespace = attr("namespace").unwrap_or_default();
                    }
                    "sql" => {
                        if let Some(id) = attr("id") {
                            in_fragment = Some((id, Vec::new()));
                        }
                    }
                    "include" => {
                        let target = match attr("refid") {
                            Some(r) => Part::Include(r),
                            None => Part::Text(String::new()),
                        };
                        if let Some(s) = open.as_mut() {
                            s.parts.push(target);
                        } else if let Some((_, parts)) = in_fragment.as_mut() {
                            parts.push(target);
                        }
                    }
                    kind if STATEMENTS.contains(&kind) => {
                        if let Some(id) = attr("id") {
                            let attrs = ["parameterType", "resultType", "resultMap"]
                                .into_iter()
                                .filter_map(|k| attr(k).map(|v| (k.to_string(), v)))
                                .collect();
                            open = Some(Statement {
                                id,
                                kind: kind.to_string(),
                                parts: Vec::new(),
                                attrs,
                                line: lines.at(here(&reader)),
                                end_line: lines.at(here(&reader)),
                            });
                        }
                    }
                    // A dynamic tag contributes its text, not its markup: the
                    // SQL a reader wants is what the branches hold.
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "sql" {
                    if let Some(frag) = in_fragment.take() {
                        fragments.push(frag);
                    }
                } else if STATEMENTS.contains(&name.as_str())
                    && let Some(mut s) = open.take()
                {
                    s.end_line = lines.at(here(&reader));
                    statements.push(s);
                }
            }
            // Text is unescaped; CDATA is already raw, which is how a
            // statement with a `<` in it is written.
            Ok(Event::Text(t)) => {
                collect(
                    t.unescape().unwrap_or_default().to_string(),
                    &mut open,
                    &mut in_fragment,
                );
            }
            Ok(Event::CData(t)) => {
                collect(
                    String::from_utf8_lossy(&t).to_string(),
                    &mut open,
                    &mut in_fragment,
                );
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    let mapper = node(path, "Mapper")
        .described(
            "namespace",
            "the Java interface this mapper's statements implement",
            namespace.clone(),
        )
        .prop("file", path)
        .build();
    out.nodes.push(mapper);

    for s in statements {
        let key = format!("{path}#{}", s.id);
        let sql = assemble(&s.parts, &fragments, &mut bounds);
        bounds.interpolations += sql.matches("${").count();
        let mut n = node(&key, "Statement")
            .described(
                "kind",
                "the statement element: select, insert, update or delete",
                s.kind.clone(),
            )
            .described(
                "sql",
                "the statement as assembled — dynamic branches included, \
                 `#{…}` bound as `?`, `${…}` left as written",
                sql,
            )
            .prop("file", path)
            .prop("line", s.line)
            .prop("end_line", s.end_line);
        for (k, v) in s.attrs {
            n = n.prop(&snake(&k), v);
        }
        out.nodes.push(n.build());
        out.edges.push(edge(path, "CONTAINS", &key).build());

        if !namespace.is_empty() {
            let method = format!("{namespace}.{}", s.id);
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
        bounds.statements += 1;
    }
    bounds
}

/// Text belongs to whichever element is open — a statement, or the `<sql>`
/// fragment a statement will include. Outside both it is structure, not SQL.
fn collect(raw: String, open: &mut Option<Statement>, fragment: &mut Option<(String, Vec<Part>)>) {
    if let Some(s) = open.as_mut() {
        s.parts.push(Part::Text(raw));
    } else if let Some((_, parts)) = fragment.as_mut() {
        parts.push(Part::Text(raw));
    }
}

/// A statement's text, with every `<include>` replaced by the fragment it
/// names **in this file**. A `refid` naming another mapper's fragment is
/// counted, not guessed at.
fn assemble(parts: &[Part], fragments: &[(String, Vec<Part>)], bounds: &mut Bounds) -> String {
    let mut sql = String::new();
    for part in parts {
        match part {
            Part::Text(t) => push_sql(&mut sql, t),
            Part::Include(refid) => match fragments.iter().find(|(id, _)| id == refid) {
                // One level: a fragment including a fragment is rare, and a
                // cycle must not be able to hang the ingest.
                Some((_, inner)) => {
                    for p in inner {
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

/// Text joined with single spaces, with `#{…}` bound as the `?` the driver
/// sends. `${…}` stays exactly as written — it pastes text, and what it pastes
/// may be the table name.
fn push_sql(sql: &mut String, text: &str) {
    let mut bound = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("#{") {
        bound.push_str(&rest[..start]);
        match rest[start..].find('}') {
            Some(end) => {
                bound.push('?');
                rest = &rest[start + end + 1..];
            }
            None => {
                rest = &rest[start..];
                break;
            }
        }
    }
    bound.push_str(rest);
    for word in bound.split_whitespace() {
        if !sql.is_empty() {
            sql.push(' ');
        }
        sql.push_str(word);
    }
}

/// `parameterType` as `parameter_type`: the graph's property names are
/// snake_case, whatever the XML spells them.
fn snake(camel: &str) -> String {
    let mut out = String::with_capacity(camel.len() + 2);
    for (i, c) in camel.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}
