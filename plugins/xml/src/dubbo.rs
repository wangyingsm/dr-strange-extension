//! The Dubbo wiring dialect: `<dubbo:service>` and `<dubbo:reference>`, which
//! live inside a Spring `<beans>` file rather than in a root of their own.
//!
//! A Dubbo service is identified by its **interface, group and version** — the
//! method is not part of that identity — and the two halves of one call are
//! normally in different repositories. So each half is *stated* in the same
//! `serves` / `requests` properties an HTTP route uses, and `wire` matches
//! them on demand. An edge drawn here would freeze a guess about which
//! provider answers.
//!
//! What this reader cannot do is follow `ref="orderServiceImpl"`. That is a
//! Spring bean id, which is not a key any plugin mints: the java plugin keys a
//! type by its qualified name, and the id is a name the container made up. It
//! is recorded as a property, with no edge, rather than guessed at by
//! capitalizing it.

use dr_strange_ext::{Output, edge, node, route};
use quick_xml::events::{BytesStart, Event};
use std::collections::BTreeMap;

/// What one file declared.
#[derive(Default)]
pub struct Read {
    pub services: usize,
    pub references: usize,
    /// Declarations naming no `interface`, which identify no service.
    pub anonymous: usize,
    /// Other elements in the beans file, which this reader does not model.
    pub unread: usize,
}

/// One `<dubbo:service>` or `<dubbo:reference>`, as the file writes it.
struct Declared {
    /// The interface's qualified name.
    interface: String,
    group: String,
    version: String,
    /// `ref` on a service, `id` on a reference: a Spring bean id, recorded
    /// but never resolved.
    bean: String,
    line: u64,
}

impl Declared {
    /// The address both halves of this service spell the same way.
    fn address(&self) -> String {
        route::dubbo(&self.interface, None, &self.group, &self.version)
    }
}

/// Every attribute of one element, by name.
fn attrs(e: &BytesStart<'_>) -> BTreeMap<String, String> {
    e.attributes()
        .flatten()
        .map(|a| {
            let key = String::from_utf8_lossy(a.key.as_ref()).to_string();
            let value = String::from_utf8_lossy(&a.value).to_string();
            (key, value)
        })
        .collect()
}

/// The local name of an element written under a namespace prefix:
/// `dubbo:service` is `service` where `prefix` is `dubbo`.
fn local<'a>(name: &'a str, prefix: &str) -> Option<&'a str> {
    name.strip_prefix(prefix)?.strip_prefix(':')
}

/// Read one beans file's Dubbo wiring.
///
/// Two passes over what was collected rather than one over the text, because
/// `<dubbo:provider version="…">` sets a default for every service in the
/// file and may be written after them.
pub fn read(path: &str, text: &str, prefix: &str, out: &mut Output) -> Read {
    let (services, references, mut found) = collect(text, prefix);
    emit(path, &services, &references, out);
    found.services = services.len();
    found.references = references.len();
    found
}

/// Everything the file declares, with the file's own defaults applied.
fn collect(text: &str, prefix: &str) -> (Vec<Declared>, Vec<Declared>, Read) {
    let mut found = Read::default();
    let mut services: Vec<Declared> = Vec::new();
    let mut references: Vec<Declared> = Vec::new();
    let mut provider_default = (String::new(), String::new());
    let mut consumer_default = (String::new(), String::new());

    let mut reader = quick_xml::Reader::from_str(text);
    reader.config_mut().trim_text(true);
    loop {
        let event = reader.read_event();
        let e = match &event {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => e,
            Ok(Event::Eof) | Err(_) => break,
            _ => continue,
        };
        let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
        let Some(local) = local(&name, prefix) else {
            continue;
        };
        let at = attrs(e);
        let line = text[..reader.buffer_position().min(text.len() as u64) as usize]
            .lines()
            .count()
            .max(1) as u64;
        let get = |key: &str| at.get(key).cloned().unwrap_or_default();
        match local {
            "service" | "reference" => {
                let Some(interface) = at.get("interface").filter(|i| !i.is_empty()) else {
                    found.anonymous += 1;
                    continue;
                };
                let declared = Declared {
                    interface: interface.clone(),
                    group: get("group"),
                    version: get("version"),
                    bean: match local {
                        "service" => get("ref"),
                        _ => get("id"),
                    },
                    line,
                };
                match local {
                    "service" => services.push(declared),
                    _ => references.push(declared),
                }
            }
            // The file's own defaults for everything declared in it.
            "provider" => provider_default = (get("group"), get("version")),
            "consumer" => consumer_default = (get("group"), get("version")),
            _ => found.unread += 1,
        }
    }

    // A declaration states its own coordinates, or inherits the file's.
    let inherit = |d: &mut Declared, from: &(String, String)| {
        if d.group.is_empty() {
            d.group = from.0.clone();
        }
        if d.version.is_empty() {
            d.version = from.1.clone();
        }
    };
    for d in &mut services {
        inherit(d, &provider_default);
    }
    for d in &mut references {
        inherit(d, &consumer_default);
    }
    (services, references, found)
}

/// One node per declaration, and the edge to the interface it names.
fn emit(path: &str, services: &[Declared], references: &[Declared], out: &mut Output) {
    // The file itself, so what it declares hangs off something: a beans file
    // that declared no wiring is not one of these.
    if !services.is_empty() || !references.is_empty() {
        out.nodes.push(node(path, "DubboConfig").build());
    }

    for (side, declared, label, ty) in [
        (
            route::Side::Serves,
            &services,
            "DubboService",
            ("EXPORTS", "ref"),
        ),
        (
            route::Side::Requests,
            &references,
            "DubboReference",
            ("CONSUMES", "id"),
        ),
    ] {
        for d in declared.iter() {
            let key = format!("{path}#{}", d.address());
            // The same two properties, with the same wording, that a route
            // and an annotated provider are written to: one `wire` reads.
            let (comparable, comparable_desc) = side.comparable();
            let (written, written_desc) = side.as_written();
            let endpoint = route::RpcRef {
                symbol: key.clone(),
                written: d.address(),
                line: d.line,
            };
            let mut n = node(&key, label)
                .described(
                    "interface",
                    "the service's interface, as the file writes it",
                    d.interface.clone(),
                )
                .described(comparable, comparable_desc, vec![endpoint.endpoint()])
                .described(written, written_desc, vec![d.address()]);
            if !d.bean.is_empty() {
                let (_, attribute) = ty;
                n = n.described(
                    attribute,
                    "the Spring bean id this declaration names — a name the container \
                     made up, not a key, so it carries no edge",
                    d.bean.clone(),
                );
            }
            out.nodes.push(n.build());
            out.edges.push(edge(path, "CONTAINS", &key).build());
            // The stand-in: the java plugin owns this key where the interface
            // was parsed, and the edge still stands where it was not.
            out.nodes
                .push(node(&d.interface, "Interface").also("External").build());
            out.edges.push(
                edge(&key, ty.0, &d.interface)
                    .described(
                        "_resolved_by",
                        "the `interface` attribute is the qualified name; Dubbo's own rule",
                        "dubbo-interface",
                    )
                    .build(),
            );
        }
    }
}
