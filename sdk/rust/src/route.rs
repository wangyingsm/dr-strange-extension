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

/// An HTTP verb, as the frameworks spell it.
///
/// `Any` is not a verb a client sends: it is what a declaration means when it
/// names no verb (Spring's bare `@RequestMapping`, `http.HandleFunc`,
/// Django's `path()`), and what a request means when a project-local wrapper
/// hides it. It matches every verb, and a reader is told that it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
    Any,
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
        }
    }
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
