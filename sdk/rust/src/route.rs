//! The HTTP route convention: what a declaration and a request have to agree
//! on before anything can match them.
//!
//! A route is a statement a *language* makes, and every mainstream framework
//! makes it in one of three shapes — an annotation carrying a path, a
//! registration call whose callee names the verb, or a client call with a path
//! literal. What differs between frameworks is the spelling of the path's
//! variable segments, and that is exactly what this module erases:
//! `/orders/{id}`, `/orders/:id`, `` `/orders/${id}` `` and `/orders/<int:id>`
//! are one route, written four ways.
//!
//! Plugins write the result on the symbol that already exists — `serves` on a
//! handler, `requests` on the function making the call — and keep the source's
//! own spelling beside it. Nothing here mints a node: matching a request to a
//! declaration is a question asked of the graph, not an edge drawn into it, so
//! this module normalises and never resolves.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// An HTTP verb, as the frameworks spell it — or the protocol a non-HTTP
/// endpoint speaks.
///
/// `Any` is not a verb a client sends: it is what a declaration means when it
/// names no verb (Spring's bare `@RequestMapping`, `http.HandleFunc`,
/// Django's `path()`), and what a request means when a project-local wrapper
/// hides it. It matches every verb, and a reader is told that it did.
///
/// `Dubbo` is not a verb at all: it is the first token of an RPC endpoint, so
/// that one property holds both kinds and `wire --verb DUBBO` selects the RPC
/// half while an HTTP filter never does. [`Verb::from_name`] never returns it
/// — a protocol is stated by the plugin that read the wiring, never inferred
/// from a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verb {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
    Any,
    Dubbo,
}

impl Verb {
    /// The verb a framework's own name carries: `GetMapping`, `@Get`, `get`,
    /// `GET`, `HttpGet`, `get_async`. `None` when the name names no verb,
    /// which is a fact about the call site rather than a failure.
    pub fn from_name(name: &str) -> Option<Self> {
        // The name is matched on its verb word, wherever the framework puts
        // it: `GetMapping` (Spring), `HttpGet` (JAX-RS style), `getJSON`
        // (jQuery). Lowercased once, compared without allocating again.
        let lower = name.to_ascii_lowercase();
        for (word, verb) in [
            ("delete", Verb::Delete),
            ("options", Verb::Options),
            ("patch", Verb::Patch),
            ("post", Verb::Post),
            ("head", Verb::Head),
            ("put", Verb::Put),
            ("get", Verb::Get),
        ] {
            if lower == word
                || lower.starts_with(word)
                || lower.ends_with(word)
                || lower.contains(&format!("_{word}"))
            {
                return Some(verb);
            }
        }
        None
    }

    /// How the property spells it. `Any` is `*`, which is what it matches.
    pub fn as_str(self) -> &'static str {
        match self {
            Verb::Get => "GET",
            Verb::Post => "POST",
            Verb::Put => "PUT",
            Verb::Patch => "PATCH",
            Verb::Delete => "DELETE",
            Verb::Head => "HEAD",
            Verb::Options => "OPTIONS",
            Verb::Any => "*",
            Verb::Dubbo => "DUBBO",
        }
    }
}

/// The address of a Dubbo service: the interface, the method where one is
/// named, then group and version.
///
/// Dubbo exports a service per **interface, group and version** — a method is
/// not part of that identity, so the interface-level address is the one both
/// halves always state and a method-level address is the extra a parser can
/// add when it can see methods. An unstated group or version is written as
/// nothing rather than left out, so the two halves spell it the same way and
/// `a.b.C::` cannot drift from `a.b.C`.
///
/// Built, never passed through [`normalize`], which would cut at the `#` that
/// names the method and prefix a `/` no RPC address has.
pub fn dubbo(iface: &str, method: Option<&str>, group: &str, version: &str) -> String {
    let method = method.map(|m| format!("#{m}")).unwrap_or_default();
    format!("{iface}{method}:{group}:{version}")
}

/// Does this callee name a **mount** — a call that gives a router a prefix
/// rather than serving a path?
///
/// `app.use('/api', router)`, `.nest("/api", api())`, `web::scope("/api")`,
/// `r.Group("/api")`, `.mount("/api", routes![…])`: the path belongs to every
/// route hung on that router, and to none of them yet. Recording it as a
/// route of its own is a wrong fact in both directions — the function doing
/// the mounting serves nothing and asks for nothing.
///
/// Nine words rather than a framework table, and `route` is deliberately not
/// one of them: axum's `.route(path, get(h))` *is* a route and chi's
/// `Route(path, fn)` is a mount, so the word says nothing on its own and
/// chi's prefix stays a known miss.
pub fn mounts(name: &str) -> bool {
    const MOUNT_WORDS: &[&str] = &[
        "group",
        "include",
        "include_router",
        "merge",
        "mount",
        "nest",
        "nest_service",
        "register_blueprint",
        "scope",
        "use",
    ];
    let lower = name.to_ascii_lowercase();
    MOUNT_WORDS.contains(&lower.as_str())
}

/// Is this path a **pattern** rather than one a client could ask for?
///
/// A wildcard segment — `/**`, `/static/*` — is how a framework spells "and
/// everything under it": Spring's `antMatchers("/api/**")`, a CORS
/// `addMapping`, a static-resource handler. A declaration may legitimately
/// be written that way, which is why [`normalize`] reduces `*` to `{}`; a
/// *request* never is, because a client asks for a concrete path.
///
/// Without this, a security config's `/api/**` normalises to `/api/{}` and
/// matches the handler it was written to guard — a wrong answer rather than
/// a missing one.
fn wildcarded(path: &str) -> bool {
    path.split('/')
        .any(|segment| segment == "*" || segment == "**")
}

/// One route a symbol states, as the source wrote it.
///
/// The same record serves both sides, because a parser reads both the same
/// way: a handler that *serves* a path, and a function whose call *asks for*
/// one. What differs is the property it is written to, not what was read —
/// so every Rust-written plugin carries this one between its two phases
/// instead of declaring its own.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteRef {
    /// The symbol the route belongs to: the declaring handler, or the
    /// function making the call.
    pub symbol: String,
    /// The verb the source states — in the callee's name, an annotation, or
    /// a `method` argument. `None` when it states none, which a
    /// project-local `request()` wrapper never does.
    pub verb: Option<Verb>,
    /// The path exactly as the source writes it, before normalising.
    pub written: String,
    /// Where it is written, 1-based.
    pub line: u64,
}

impl RouteRef {
    /// The comparable form — [`route`] over this record's own two halves.
    pub fn route(&self) -> String {
        route(self.verb, &self.written)
    }
}

/// One RPC endpoint a symbol states, as [`dubbo`] built it.
///
/// The same record serves both sides, as [`RouteRef`] does: a type that
/// *provides* a service and one whose reference *asks for* it are read the
/// same way and differ only in which property they are written to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRef {
    /// The symbol the endpoint belongs to: the provider, or the consumer
    /// holding the reference.
    pub symbol: String,
    /// The address, from [`dubbo`].
    pub written: String,
    /// Where it is written, 1-based.
    pub line: u64,
}

impl RpcRef {
    /// The comparable form: the protocol token, a space, the address.
    pub fn endpoint(&self) -> String {
        format!("{} {}", Verb::Dubbo.as_str(), self.written)
    }
}

/// Which half of a route a set of records states.
///
/// A handler *serves* a path and a call *asks for* one. The records are read
/// the same way and differ only in which pair of properties they are written
/// to, which is all this names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// A declaration, written on the handler.
    Serves,
    /// A request, written on the function making the call.
    Requests,
}

impl Side {
    /// The property holding the comparable form, and what it says.
    ///
    /// Public because a plugin whose node builder takes properties one at a
    /// time cannot hand [`Routes`] a map to write into — the XML reader is
    /// one — and the name and wording still have to be the same ones `wire`
    /// reads.
    pub fn comparable(self) -> (&'static str, &'static str) {
        match self {
            Side::Serves => (
                "serves",
                "the routes this handler serves, as `VERB path` with every variable \
                 segment reduced to `{}` — `*` where the declaration names no verb; \
                 an RPC service this type provides is `DUBBO interface#method:group:version`, \
                 built rather than normalized. `wire` matches these against what asks for them",
            ),
            Side::Requests => (
                "requests",
                "the routes this function asks for, as `VERB path` with every variable \
                 segment reduced to `{}` — `*` where the call names no verb; an RPC \
                 service this type references is `DUBBO interface#method:group:version`, \
                 built rather than normalized. `wire` matches these against what serves them",
            ),
        }
    }

    /// The property holding the source's own spelling, and what it says.
    pub fn as_written(self) -> (&'static str, &'static str) {
        let name = match self {
            Side::Serves => "serves_as_written",
            Side::Requests => "requests_as_written",
        };
        (name, "the same paths exactly as the source writes them")
    }
}

/// One symbol's routes: the comparable forms, and the spellings they came
/// from. Sets, because one function may ask for the same path twice.
#[derive(Debug, Default)]
struct Stated<'a> {
    comparable: BTreeSet<String>,
    written: BTreeSet<&'a str>,
}

/// The routes each symbol states, folded ready to write.
///
/// Every plugin's assemble phase does the same two things with the records
/// its parse produced — fold them by symbol, then write two properties on
/// that symbol's node — so it is done here once, against the one `Props`
/// shape every plugin uses.
#[derive(Debug)]
pub struct Routes<'a> {
    side: Side,
    by_symbol: BTreeMap<&'a str, Stated<'a>>,
    /// Patterns left out of the request side, counted so the account can say
    /// so rather than drop them in silence.
    patterns: usize,
    /// Whether these are RPC endpoints, which the account names differently:
    /// a service is provided by a type, not served by a handler.
    rpc: bool,
}

impl<'a> Routes<'a> {
    /// Fold a parse's records, one side of the convention.
    pub fn of(side: Side, refs: impl IntoIterator<Item = &'a RouteRef>) -> Self {
        let mut by_symbol: BTreeMap<&str, Stated> = BTreeMap::new();
        let mut patterns = 0usize;
        for r in refs {
            if side == Side::Requests && wildcarded(&r.written) {
                patterns += 1;
                continue;
            }
            let stated = by_symbol.entry(r.symbol.as_str()).or_default();
            stated.comparable.insert(r.route());
            stated.written.insert(r.written.as_str());
        }
        Self {
            side,
            by_symbol,
            patterns,
            rpc: false,
        }
    }

    /// Fold a parse's RPC endpoints, the same side of the same convention.
    ///
    /// Separate from [`Routes::of`] only in how the comparable form is made:
    /// an address is built, never normalized. What it writes, and where, is
    /// the same code — so the two kinds of endpoint cannot drift apart in the
    /// property that `wire` reads.
    pub fn of_rpc(side: Side, refs: impl IntoIterator<Item = &'a RpcRef>) -> Self {
        let mut by_symbol: BTreeMap<&str, Stated> = BTreeMap::new();
        for r in refs {
            let stated = by_symbol.entry(r.symbol.as_str()).or_default();
            stated.comparable.insert(r.endpoint());
            stated.written.insert(r.written.as_str());
        }
        Self {
            side,
            by_symbol,
            patterns: 0,
            rpc: true,
        }
    }

    /// Write this symbol's two properties, and say how many routes that was.
    ///
    /// Zero, and nothing written, when the symbol states none: a function
    /// that asks for nothing carries no property rather than an empty one.
    pub fn write(&self, symbol: &str, props: &mut Map<String, Value>) -> usize {
        let Some(stated) = self.by_symbol.get(symbol) else {
            return 0;
        };
        let (name, desc) = self.side.comparable();
        merge(props, name, desc, stated.comparable.iter().map(String::as_str));
        let (name, desc) = self.side.as_written();
        merge(props, name, desc, stated.written.iter().copied());
        stated.comparable.len()
    }

    /// The account of what was written, when there is one to give.
    ///
    /// Counted from what [`Routes::write`] actually attached rather than from
    /// what was folded, so a record naming a symbol no node holds is not
    /// claimed as recorded.
    pub fn note(&self, recorded: usize) -> Option<String> {
        if recorded == 0 && self.patterns == 0 {
            return None;
        }
        let (property, what) = match (self.side, self.rpc) {
            (Side::Serves, false) => ("serves", "declared by a handler"),
            (Side::Requests, false) => {
                ("requests", "asked for by a call whose argument is a path")
            }
            (Side::Serves, true) => ("serves", "provided by a type the wiring exports"),
            (Side::Requests, true) => ("requests", "referenced by a type the wiring injects"),
        };
        let kind = match self.rpc {
            true => "RPC service(s)",
            false => "route(s)",
        };
        let mut note = format!(
            "{recorded} {kind} {what} — recorded as `{property}`; the other half is \
             matched on demand, never stored"
        );
        if self.patterns > 0 {
            note.push_str(&format!(
                "; {} wildcard pattern(s) left out, which a client cannot ask for",
                self.patterns
            ));
        }
        Some(note)
    }
}

/// Add to one of the four properties rather than replace it.
///
/// A type can state both kinds of endpoint — a `@RestController` that is also
/// a Dubbo provider — and the two are folded separately, so the second writer
/// must not drop what the first wrote. A set, because the same endpoint
/// stated twice is one endpoint.
fn merge<'s>(
    props: &mut Map<String, Value>,
    name: &str,
    desc: &str,
    values: impl Iterator<Item = &'s str>,
) {
    let mut all: BTreeSet<String> = props
        .get(name)
        .and_then(|v| v.get("$value"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    all.extend(values.map(str::to_string));
    props.insert(
        name.into(),
        json!({ "$desc": desc, "$value": all.iter().collect::<Vec<_>>() }),
    );
}

/// Could this string be a route? A leading `/`, or an absolute http(s) URL,
/// and no whitespace.
///
/// Deliberately broad: a path-shaped literal passed to *any* call is a route
/// reference whatever the callee is named, which is the only way a
/// project-local `request('/x')` wrapper is ever visible. A file path or a
/// router path that slips through matches no declaration and is counted —
/// a named miss, not a wrong edge.
pub fn looks_like_path(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.contains(char::is_whitespace) {
        return false;
    }
    s.starts_with('/') || s.starts_with("http://") || s.starts_with("https://")
}

/// One path in the form a declaration and a request can be compared in:
/// origin and query dropped, duplicate separators collapsed, no trailing
/// slash, and **every variable segment reduced to `{}`**, however its
/// framework spells it.
pub fn normalize(path: &str) -> String {
    let mut rest = path.trim();
    // An absolute URL names the same route as the path inside it; the host is
    // a deployment's business, not the graph's.
    for scheme in ["http://", "https://"] {
        if let Some(after) = rest.strip_prefix(scheme) {
            rest = match after.find('/') {
                Some(at) => &after[at..],
                None => "/",
            };
            break;
        }
    }
    // A query or a fragment says nothing about which handler serves it.
    for cut in ['?', '#'] {
        if let Some(at) = rest.find(cut) {
            rest = &rest[..at];
        }
    }

    let mut out = String::with_capacity(rest.len() + 1);
    out.push('/');
    for segment in rest.split('/') {
        if segment.is_empty() {
            continue;
        }
        if out.len() > 1 {
            out.push('/');
        }
        out.push_str(&normalize_segment(segment));
    }
    out
}

/// One segment, with a variable in any framework's spelling reduced to `{}`.
///
/// `{id}` (Spring, Go 1.22, gorilla), `{id:[0-9]+}` (a regex constraint),
/// `:id` (Express, gin, Nest), `${id}` (a TS template hole), `<int:id>`
/// (Flask, Django), `*` and `**` (an Ant-style wildcard — not a variable, but
/// it stands for whatever a request puts there, which is the same thing here).
fn normalize_segment(segment: &str) -> String {
    let variable = segment.starts_with('{')
        || segment.starts_with(':')
        || segment.starts_with("${")
        || segment.starts_with('<')
        || segment == "*"
        || segment == "**";
    if variable {
        return "{}".to_string();
    }
    // A segment may hold a hole without being one: `file-${id}.json`,
    // `{id}.pdf`. The whole segment is then unknowable, so it is one too.
    if segment.contains("${") || (segment.contains('{') && segment.contains('}')) {
        return "{}".to_string();
    }
    segment.to_string()
}

/// A base and a path, joined the way a client's `baseURL` and a call's path
/// are: one separator between them, and the base's own trailing slash
/// forgiven.
pub fn join(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    let path = path.trim_start_matches('/');
    if base.is_empty() {
        return format!("/{path}");
    }
    format!("{base}/{path}")
}

/// The property's value: the verb, a space, the normalized path. `*` where no
/// verb is written, which matches every verb.
///
/// The format is the whole contract between the plugins that write it and the
/// verb that reads it, so it is deliberately the simplest thing that can be
/// split back apart.
pub fn route(verb: Option<Verb>, path: &str) -> String {
    format!("{} {}", verb.unwrap_or(Verb::Any).as_str(), normalize(path))
}

#[cfg(test)]
mod tests;
