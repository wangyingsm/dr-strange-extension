//! What a Docker Compose file *declares* it runs.
//!
//! This is the first half of the ops graph ([arch/10-ops.md] in the main
//! repository): a topology as nodes and edges, parsed from a file in a
//! repository rather than read from a live engine. Compose first because it
//! is the small case — one file, no templating, and a service's identity is
//! the key it is written under.
//!
//! ## The model
//!
//! A project CONTAINS its services; a service RUNS one container, which
//! USES_IMAGE and MOUNTS what it is given; services DEPEND_ON each other and
//! are ATTACHED_TO networks. Nodes are keyed structurally —
//! `<project>/service/<name>` — so two Compose files in a monorepo describe
//! two projects and never collide.
//!
//! ## Routed by name, not by extension
//!
//! A Compose file is a *filename*. Claiming `.yml` would take every
//! Kubernetes manifest, CI workflow and chart in the tree from whatever
//! reads them, so this plugin declares no extensions: the host dispatches
//! `compose.yaml` and its aliases to the plugin installed under the name
//! `compose`, exactly as it already routes a build manifest to `deps`.
//!
//! ## Values, and what is not recorded
//!
//! Scalars are kept **as written** — `${TAG:-latest}` stays that string,
//! because a tag resolved against an environment this parser cannot see is a
//! fact about nothing.
//!
//! **Environment values are not recorded, only their names.** A Compose
//! `environment:` block is a ConfigMap and a Secret in one place with
//! nothing distinguishing them, and `POSTGRES_PASSWORD` sits in them
//! routinely. Recording the names answers "what is this service configured
//! by" without copying credentials into a graph that gets exported and
//! backed up. Where the ops model records a value — a Kubernetes ConfigMap —
//! it does so because the author chose `ConfigMap` over `Secret` and thereby
//! said it was not sensitive. Compose offers no such statement, so this
//! plugin does not infer one.

use dr_strange_ext::{
    Input, Manifest, Output, OutputExt, Simple, Value, edge, host, node, output, simple_plugin,
};
use yaml_rust2::{Yaml, YamlLoader};

/// Shown beside the name in UIs: an original mark, not a trademarked logo.
const LOGO: &str = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24'><rect x='2' y='9' width='9' height='6' rx='1.5' fill='#2496ed'/><rect x='13' y='9' width='9' height='6' rx='1.5' fill='#2496ed'/><rect x='7.5' y='3' width='9' height='4.5' rx='1.5' fill='#1d6fa5'/></svg>";

struct Compose;

impl Simple for Compose {
    fn describe() -> Manifest {
        Manifest {
            name: "compose".into(),
            version: "1".into(),
            // Deliberately none — see the module docs. The host routes the
            // filenames it knows to this plugin by its name.
            extensions: Vec::new(),
            logo: Some(LOGO.into()),
        }
    }

    fn process(subject: Input, _options: &[(String, String)]) -> Result<Output, String> {
        let mut out = output();
        let files = match subject {
            Input::Files(paths) => paths,
            Input::Document(doc) => {
                extract(&doc.name, &String::from_utf8_lossy(&doc.bytes), &mut out);
                return Ok(out.finish());
            }
        };
        for path in files {
            match host::read(&path) {
                Ok(bytes) => extract(&path, &String::from_utf8_lossy(&bytes), &mut out),
                // One unreadable file should not sink the ingest: it is
                // counted and named, so a thin graph explains itself.
                Err(why) => {
                    out.report.skipped += 1;
                    out.note(format!("{path}: {why}"));
                }
            }
        }
        Ok(out.finish())
    }
}

simple_plugin!(Compose);

/// The project a file describes: its own `name:`, else the directory holding
/// it, else the file's stem — which is what `docker compose` itself does.
fn project_of(path: &str, doc: &Yaml) -> String {
    if let Some(name) = doc["name"].as_str().filter(|n| !n.is_empty()) {
        return name.to_string();
    }
    let parts: Vec<&str> = path.rsplit('/').collect();
    match parts.as_slice() {
        [file] => file.split('.').next().unwrap_or("compose").to_string(),
        [_file, dir, ..] if !dir.is_empty() => dir.to_string(),
        _ => "compose".to_string(),
    }
}

/// A scalar as written. A YAML number or bool is still text here: what the
/// author typed is the fact, and the graph keeps facts.
fn written(y: &Yaml) -> Option<String> {
    match y {
        Yaml::String(s) => Some(s.clone()),
        Yaml::Integer(i) => Some(i.to_string()),
        Yaml::Real(r) => Some(r.clone()),
        Yaml::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Every string in a value that may be either one scalar or a sequence —
/// which is most of Compose's short-form syntax.
fn one_or_many(y: &Yaml) -> Vec<String> {
    match y {
        Yaml::Array(items) => items.iter().filter_map(written).collect(),
        other => written(other).into_iter().collect(),
    }
}

/// A mapping's keys, in file order, for a block whose values are withheld.
fn keys_of(y: &Yaml) -> Vec<String> {
    match y {
        Yaml::Hash(h) => h.keys().filter_map(written).collect(),
        // The `KEY=value` list form: the name is everything left of the `=`.
        Yaml::Array(items) => items
            .iter()
            .filter_map(written)
            .map(|s| s.split('=').next().unwrap_or(&s).to_string())
            .collect(),
        _ => Vec::new(),
    }
}

/// The names a file declares at the top level, so a reference further down
/// does not mint a second node under a key that already exists.
#[derive(Default)]
struct Declared {
    volumes: Vec<String>,
    networks: Vec<String>,
}

impl Declared {
    fn remember(&mut self, label: &str, name: String) {
        match label {
            "Volume" => self.volumes.push(name),
            _ => self.networks.push(name),
        }
    }

    fn has(&self, label: &str, name: &str) -> bool {
        let set = if label == "Volume" {
            &self.volumes
        } else {
            &self.networks
        };
        set.iter().any(|n| n == name)
    }
}

fn extract(path: &str, text: &str, out: &mut Output) {
    let docs = match YamlLoader::load_from_str(text) {
        Ok(docs) => docs,
        Err(why) => {
            out.report.skipped += 1;
            out.note(format!("{path}: not valid YAML ({why})"));
            return;
        }
    };
    let Some(doc) = docs.first() else {
        out.note(format!("{path}: empty"));
        return;
    };
    let services = &doc["services"];
    if services.is_badvalue() {
        out.report.skipped += 1;
        out.note(format!("{path}: no `services` block — not a Compose file"));
        return;
    }

    let project = project_of(path, doc);
    out.nodes.push(
        node(&project, "Namespace")
            .prop("kind", "compose")
            .prop("file", path)
            .described(
                "name_source",
                "whether the project name was declared or taken from the path",
                if doc["name"].as_str().is_some_and(|n| !n.is_empty()) {
                    "declared"
                } else {
                    "path"
                },
            )
            .build(),
    );

    // Declared top-level networks and volumes exist whether or not a service
    // attaches to one, so they are minted from the declaration first — and
    // remembered, so a reference further down does not mint the key twice.
    let mut declared = Declared::default();
    for (block, label) in [("networks", "Network"), ("volumes", "Volume")] {
        for name in keys_of(&doc[block]) {
            let key = format!("{project}/{}/{name}", label.to_lowercase());
            out.nodes
                .push(node(&key, label).prop("name", name.clone()).build());
            out.edges.push(edge(&key, "IN_NAMESPACE", &project).build());
            declared.remember(label, name);
        }
    }

    let Yaml::Hash(map) = services else {
        out.note(format!("{path}: `services` is not a mapping"));
        return;
    };

    for (name_y, spec) in map {
        let Some(name) = written(name_y) else {
            continue;
        };
        service(&project, &name, spec, &declared, out);
    }
}

/// One service: the workload, its container, and everything the container is
/// wired to.
fn service(project: &str, name: &str, spec: &Yaml, declared: &Declared, out: &mut Output) {
    let wkey = format!("{project}/service/{name}");
    let mut workload = node(&wkey, "Workload")
        .prop("kind", "service")
        .prop("name", name);

    // A published port is an external entry, and in Compose it is a property
    // of the service rather than a routing object of its own.
    let ports = one_or_many(&spec["ports"]);
    if !ports.is_empty() {
        workload = workload.described(
            "ports",
            "published ports, as written (`host:container`)",
            Value::from(ports),
        );
    }
    if let Some(cmd) = written(&spec["command"]).or_else(|| {
        let parts = one_or_many(&spec["command"]);
        (!parts.is_empty()).then(|| parts.join(" "))
    }) {
        workload = workload.prop("command", cmd);
    }
    if let Some(replicas) = written(&spec["deploy"]["replicas"]) {
        workload = workload.prop("replicas", replicas);
    }
    out.nodes.push(workload.build());
    out.edges.push(edge(&wkey, "IN_NAMESPACE", project).build());

    // One container per service in Compose, named for the service. It is its
    // own node so that `USES_IMAGE` and `MOUNTS` mean the same thing here as
    // they will for a Kubernetes pod template with a sidecar in it.
    let ckey = format!("{wkey}/{name}");
    let built_here = !spec["build"].is_badvalue();
    let mut container = node(&ckey, "Container").prop("name", name).described(
        "source",
        "whether the container's image is pulled or built here",
        if built_here { "build" } else { "image" },
    );
    if built_here {
        // Built here: there is no reference to join on, and saying so is
        // worth more than a node standing in for one.
        let ctx = written(&spec["build"])
            .or_else(|| written(&spec["build"]["context"]))
            .unwrap_or_else(|| ".".into());
        container = container.prop("build_context", ctx);
    }
    // Config: names only. The module docs say why.
    let env_keys = keys_of(&spec["environment"]);
    if !env_keys.is_empty() {
        container = container.described(
            "env_names",
            "the environment variables this service is given, names only — \
             a Compose file does not say which of them are secret",
            Value::from(env_keys),
        );
    }
    // Collected before the node is built: a second node under the same key
    // is a second node, not more properties on the first.
    let mut host_mounts: Vec<String> = Vec::new();
    for mount in one_or_many(&spec["volumes"]) {
        if let Some(bind) = volume_mount(project, &ckey, &mount, declared, out) {
            host_mounts.push(bind);
        }
    }
    if !host_mounts.is_empty() {
        container = container.described(
            "host_mounts",
            "bind mounts, as written — a host path is not an entity this graph holds",
            Value::from(host_mounts),
        );
    }
    out.nodes.push(container.build());
    out.edges.push(edge(&wkey, "RUNS", &ckey).build());

    if let Some(reference) = written(&spec["image"]) {
        image(&ckey, &reference, out);
    }
    for file in one_or_many(&spec["env_file"]) {
        let key = format!("{project}/configmap/{file}");
        out.nodes.push(
            node(&key, "ConfigMap")
                .prop("name", file.clone())
                .prop("kind", "env_file")
                .build(),
        );
        out.edges
            .push(edge(&ckey, "MOUNTS", &key).prop("as", "envFrom").build());
        out.edges.push(edge(&key, "IN_NAMESPACE", project).build());
    }

    for net in one_or_many(&spec["networks"]).into_iter().chain(
        // The mapping form, `networks: {frontend: {...}}`.
        keys_of(&spec["networks"]),
    ) {
        let key = format!("{project}/network/{net}");
        // An edge must not point at a node nobody minted.
        if !declared.has("Network", &net) {
            out.nodes
                .push(node(&key, "Network").prop("name", net).build());
            out.edges.push(edge(&key, "IN_NAMESPACE", project).build());
        }
        out.edges.push(edge(&wkey, "ATTACHED_TO", &key).build());
    }
    for (dep, condition) in depends_on(&spec["depends_on"]) {
        let mut e = edge(&wkey, "DEPENDS_ON", format!("{project}/service/{dep}"));
        if let Some(c) = condition {
            e = e.prop("condition", c);
        }
        out.edges.push(e.build());
    }
}

/// `depends_on` in either form: a list of names, or a mapping that adds a
/// condition to each.
fn depends_on(y: &Yaml) -> Vec<(String, Option<String>)> {
    match y {
        Yaml::Array(items) => items
            .iter()
            .filter_map(written)
            .map(|n| (n, None))
            .collect(),
        Yaml::Hash(h) => h
            .iter()
            .filter_map(|(k, v)| written(k).map(|name| (name, written(&v["condition"]))))
            .collect(),
        _ => Vec::new(),
    }
}

/// A short-form volume mount, `source:target[:mode]`. A named volume is a
/// node of this project; a host path is recorded on the edge, because a path
/// on whoever's machine ran this is not an entity the graph can hold.
fn volume_mount(
    project: &str,
    ckey: &str,
    mount: &str,
    declared: &Declared,
    out: &mut Output,
) -> Option<String> {
    let mut parts = mount.split(':');
    let source = parts.next().filter(|s| !s.is_empty())?;
    let target = parts.next().unwrap_or("");
    let named = !source.starts_with('.') && !source.starts_with('/') && !source.starts_with('~');
    if named {
        let key = format!("{project}/volume/{source}");
        // Compose requires a named volume to be declared at the top level;
        // one that is not is still recorded, since the mount is a fact.
        if !declared.has("Volume", source) {
            out.nodes
                .push(node(&key, "Volume").prop("name", source).build());
            out.edges.push(edge(&key, "IN_NAMESPACE", project).build());
        }
        let mut e = edge(ckey, "MOUNTS", &key).prop("as", "file");
        if !target.is_empty() {
            e = e.prop("path", target);
        }
        out.edges.push(e.build());
        None
    } else {
        Some(mount.to_string())
    }
}

/// The image reference, and whatever is parseable from it.
///
/// The tag is where a build pipeline usually puts the revision it built, and
/// that is the only thread from a running thing back to a commit — so a
/// revision-shaped tag component is recorded as `revision`, a digest as
/// `digest`, and a floating tag as neither. See arch/10-ops.md §5.
fn image(ckey: &str, reference: &str, out: &mut Output) {
    let mut n = node(reference, "Image").prop("reference", reference);

    let (name_part, digest) = match reference.split_once("@") {
        Some((before, d)) => (before, Some(d.to_string())),
        None => (reference, None),
    };
    // A colon in the final path segment is a tag; one before a `/` is a
    // registry port, which is not.
    let tag = name_part
        .rsplit_once('/')
        .map_or(name_part, |(_, last)| last)
        .split_once(':')
        .map(|(_, t)| t.to_string());

    if let Some(d) = &digest {
        n = n.described(
            "digest",
            "pins the bytes exactly, and says nothing about provenance",
            d.clone(),
        );
    }
    if let Some(t) = &tag {
        n = n.prop("tag", t.clone());
        if let Some(rev) = revision_in(t) {
            n = n.described(
                "revision",
                "a git revision this tag appears to name — the join to a history plane",
                rev,
            );
        }
    }
    out.nodes.push(n.build());
    out.edges.push(edge(ckey, "USES_IMAGE", reference).build());
}

/// A git revision inside a tag: a bare hex string, or one behind a `sha-` /
/// `git-` / `rev-` prefix. Seven characters is the shortest abbreviation git
/// itself prints; below that a match would be coincidence.
fn revision_in(tag: &str) -> Option<String> {
    let candidate = tag
        .strip_prefix("sha-")
        .or_else(|| tag.strip_prefix("sha256-"))
        .or_else(|| tag.strip_prefix("git-"))
        .or_else(|| tag.strip_prefix("rev-"))
        .unwrap_or(tag);
    let hex = candidate.len() >= 7
        && candidate.len() <= 40
        && candidate.chars().all(|c| c.is_ascii_hexdigit());
    // All-digit tags are version numbers far more often than revisions.
    let all_digits = candidate.chars().all(|c| c.is_ascii_digit());
    (hex && !all_digits).then(|| candidate.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(path: &str, text: &str) -> Output {
        let mut out = output();
        extract(path, text, &mut out);
        out
    }

    fn node_at<'a>(o: &'a Output, key: &str) -> &'a dr_strange_ext::Node {
        o.nodes.iter().find(|n| n.key == key).unwrap_or_else(|| {
            panic!(
                "no node {key} in {:?}",
                o.nodes.iter().map(|n| &n.key).collect::<Vec<_>>()
            )
        })
    }

    fn props(n: &dr_strange_ext::Node) -> Value {
        n.properties.parse().expect("node properties are JSON")
    }

    fn prop(n: &dr_strange_ext::Node, name: &str) -> String {
        let v = props(n);
        let at = &v[name];
        // A described property carries its value under `$value`.
        at.as_str()
            .or_else(|| at["$value"].as_str())
            .unwrap_or_else(|| panic!("no prop {name} on {}: {v:?}", n.key))
            .to_string()
    }

    fn has_edge(o: &Output, src: &str, ty: &str, dst: &str) -> bool {
        o.edges
            .iter()
            .any(|e| e.src == src && e.type_ == ty && e.dst == dst)
    }

    const SAMPLE: &str = r#"
name: shop
services:
  api:
    image: ghcr.io/acme/api:sha-9f2c1b7
    ports: ["8080:8080"]
    environment:
      DATABASE_URL: postgres://u:p@db:5432/shop
      POSTGRES_PASSWORD: hunter2
    depends_on:
      db:
        condition: service_healthy
    networks: [backend]
    volumes:
      - uploads:/var/lib/uploads
      - ./local:/mnt/local
  db:
    image: postgres:17
    volumes:
      - pgdata:/var/lib/postgresql/data
volumes:
  pgdata:
  uploads:
networks:
  backend:
"#;

    /// The topology: a project containing services, each running a container
    /// that uses an image, with the links between them.
    #[test]
    fn the_topology_is_nodes_and_edges() {
        let o = run("deploy/compose.yaml", SAMPLE);

        // The declared project name wins over the directory.
        assert_eq!(prop(node_at(&o, "shop"), "name_source"), "declared");
        assert_eq!(prop(node_at(&o, "shop/service/api"), "kind"), "service");
        assert!(has_edge(&o, "shop/service/api", "IN_NAMESPACE", "shop"));
        assert!(has_edge(
            &o,
            "shop/service/api",
            "RUNS",
            "shop/service/api/api"
        ));
        assert!(has_edge(
            &o,
            "shop/service/api/api",
            "USES_IMAGE",
            "ghcr.io/acme/api:sha-9f2c1b7"
        ));
        assert!(has_edge(
            &o,
            "shop/service/api",
            "DEPENDS_ON",
            "shop/service/db"
        ));
        assert!(has_edge(
            &o,
            "shop/service/api",
            "ATTACHED_TO",
            "shop/network/backend"
        ));
        // A named volume is an entity; both services mount their own.
        assert!(has_edge(
            &o,
            "shop/service/api/api",
            "MOUNTS",
            "shop/volume/uploads"
        ));
        assert!(has_edge(
            &o,
            "shop/service/db/db",
            "MOUNTS",
            "shop/volume/pgdata"
        ));
    }

    /// The join to a history plane, and the three cases where it is absent.
    #[test]
    fn a_revision_shaped_tag_is_the_join_and_nothing_else_is() {
        let o = run("compose.yaml", SAMPLE);
        assert_eq!(
            prop(node_at(&o, "ghcr.io/acme/api:sha-9f2c1b7"), "revision"),
            "9f2c1b7"
        );
        // A third-party image names no revision of this repository.
        let pg = props(node_at(&o, "postgres:17"));
        assert!(pg["revision"].is_null(), "{pg:?}");
        assert_eq!(prop(node_at(&o, "postgres:17"), "tag"), "17");

        // Floating and version tags name nothing; a digest pins bytes only.
        for (tag, want_rev) in [
            ("api:latest", None),
            ("api:v2.1.0", None),
            ("api:9f2c1b7", Some("9f2c1b7")),
            ("api:git-abcdef1234", Some("abcdef1234")),
        ] {
            let text = format!(
                "services:
  s:
    image: {tag}
"
            );
            let o = run("compose.yaml", &text);
            let got = props(node_at(&o, tag));
            match want_rev {
                Some(r) => assert_eq!(got["revision"]["$value"].as_str(), Some(r), "{tag}"),
                None => assert!(got["revision"].is_null(), "{tag}: {got:?}"),
            }
        }

        let o = run(
            "compose.yaml",
            "services:
  s:
    image: api@sha256:abc123
",
        );
        let n = props(node_at(&o, "api@sha256:abc123"));
        assert_eq!(n["digest"]["$value"].as_str(), Some("sha256:abc123"));
        assert!(n["revision"].is_null(), "a digest names no revision");
    }

    /// Environment values are never recorded — a Compose file does not say
    /// which of them are secret, and one of these is a password.
    #[test]
    fn environment_values_stay_out_of_the_graph() {
        let o = run("compose.yaml", SAMPLE);
        let names = props(node_at(&o, "shop/service/api/api"));
        let listed = names["env_names"]["$value"]
            .as_array()
            .expect("env_names is a list")
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>();
        assert!(listed.contains(&"POSTGRES_PASSWORD"), "{listed:?}");

        // Nowhere in the whole output does the value appear.
        let everything = format!("{:?}{:?}", o.nodes, o.edges);
        assert!(
            !everything.contains("hunter2"),
            "a secret value was recorded"
        );
        assert!(
            !everything.contains("postgres://u:p@"),
            "a URL with a password was recorded"
        );
    }

    /// A bind mount is a path on whoever's machine ran this, not an entity.
    #[test]
    fn a_host_path_is_not_a_volume_node() {
        let o = run("compose.yaml", SAMPLE);
        assert!(
            !o.nodes.iter().any(|n| n.key.contains("/volume/./local")),
            "a host path became a node"
        );
        let c = props(node_at(&o, "shop/service/api/api"));
        let mounts = c["host_mounts"]["$value"].as_array().expect("host_mounts");
        assert!(
            mounts
                .iter()
                .any(|m| m.as_str() == Some("./local:/mnt/local")),
            "{mounts:?}"
        );
    }

    /// A YAML file that is not Compose is declined, and said so, rather than
    /// half-parsed into a shape that looks like a topology.
    #[test]
    fn a_file_without_services_is_declined() {
        let o = run(
            "k8s/deployment.yaml",
            "apiVersion: apps/v1
kind: Deployment
",
        );
        assert!(o.nodes.is_empty(), "{:?}", o.nodes);
        assert_eq!(o.report.skipped, 1);
        assert!(
            o.report
                .notes
                .iter()
                .any(|n| n.contains("not a Compose file")),
            "{:?}",
            o.report.notes
        );
    }

    /// With no `name:`, the project is the directory — what `docker compose`
    /// itself does.
    #[test]
    fn the_project_falls_back_to_the_directory() {
        let o = run(
            "myapp/docker-compose.yml",
            "services:
  web:
    image: nginx
",
        );
        assert_eq!(prop(node_at(&o, "myapp"), "name_source"), "path");
        assert!(has_edge(&o, "myapp/service/web", "IN_NAMESPACE", "myapp"));
    }
}
