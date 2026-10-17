//! What a set of Kubernetes manifests *declares* it runs.
//!
//! The second half of the ops graph (`arch/10-ops.md` in the main
//! repository). Compose was the small case — one file, one service per key.
//! Kubernetes is the real one: objects spread across files, a workload's pods
//! named by *label selector* rather than by key, and config reached through
//! a pod's volume list rather than named directly.
//!
//! ## Routed by an explicit handler
//!
//! This plugin claims **no extension**, and unlike the Compose reader it
//! cannot be routed by filename either: a manifest may be called anything.
//! Claiming `.yaml` was considered and rejected — the host buckets each file
//! to exactly one reader with no fallback, so claiming the extension would
//! take every `values.yaml`, CI workflow and chart in the tree from the
//! document reader and stop them being read at all.
//!
//! So a manifest tree is digested explicitly:
//!
//! ```console
//! drsg digest ./k8s --handler k8s
//! helm template ./chart > rendered.yaml && drsg digest rendered.yaml --handler k8s
//! ```
//!
//! ## What is declined
//!
//! A document without both `apiVersion` and `kind` is not a Kubernetes
//! object, and is counted and named rather than half-read. A `kind` this
//! plugin does not model is recorded as a bare `Resource` node, so a reader
//! can see that something was there without the plugin pretending to
//! understand it.
//!
//! ## Config and secrets
//!
//! A ConfigMap's **values are recorded**: choosing `ConfigMap` over `Secret`
//! is the author stating that the contents are not sensitive, and a query
//! that can see them answers most config questions. A **Secret's values
//! never are** — not even base64, which is not encryption — so its node
//! carries the key names and the type alone.

use dr_strange_ext::{
    Guest, Input, Manifest, Output, OutputExt, Value, edge, export_plugin, host, node,
    ops::parse_image, output,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use yaml_rust2::{Yaml, YamlLoader};

/// A set of labels, as pairs — a map would not survive the partial's
/// round trip in a stable order, and order is what keeps the assembled
/// result independent of where the chunk boundaries fell.
type Labels = Vec<(String, String)>;

/// `(namespace, node key, labels)` — a workload's pod-template labels, or
/// the selector a service asked for.
type Labelled = (String, String, Labels);

/// One chunk's worth of findings — the partial this plugin shuttles between
/// its own two phases.
///
/// Nodes and edges that a single document settles are already final. What
/// cannot be settled per chunk travels as facts: a Service and the Deployment
/// it selects are routinely in different files, and the host may put them in
/// different chunks.
#[derive(Default, Serialize, Deserialize)]
struct Chunk {
    nodes: Vec<(String, String, String)>,
    edges: Vec<(String, String, String, String)>,
    /// Workloads, with the labels a selector will be matched against.
    workloads: Vec<Labelled>,
    /// Services, with the labels each one asked for.
    selectors: Vec<Labelled>,
    /// Namespaces named anywhere in this chunk.
    namespaces: Vec<String>,
    skipped: u32,
    notes: Vec<String>,
}

impl Chunk {
    fn push_node(&mut self, n: dr_strange_ext::Node) {
        self.nodes.push((n.key, n.label, n.properties));
    }

    fn push_edge(&mut self, e: dr_strange_ext::Edge) {
        self.edges.push((e.src, e.type_, e.dst, e.properties));
    }
}

/// Shown beside the name in UIs: an original mark, not a trademarked logo.
const LOGO: &str = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24'><path d='M12 2l8.5 4.9v9.8L12 22l-8.5-5.3V6.9z' fill='none' stroke='#326ce5' stroke-width='2'/><circle cx='12' cy='12' r='3' fill='#326ce5'/></svg>";

/// The kinds that run containers, and the node kind each becomes.
const WORKLOAD_KINDS: &[&str] = &[
    "Deployment",
    "StatefulSet",
    "DaemonSet",
    "Job",
    "CronJob",
    "ReplicaSet",
    "ReplicationController",
    "Pod",
];

struct K8s;

impl Guest for K8s {
    fn describe() -> Manifest {
        Manifest {
            name: "k8s".into(),
            version: "1".into(),
            // Deliberately none: a manifest is neither an extension nor a
            // filename. See the module docs.
            extensions: Vec::new(),
            logo: Some(LOGO.into()),
        }
    }

    fn parse(subject: Input, _options: Vec<(String, String)>) -> Result<Vec<u8>, String> {
        let mut chunk = Chunk::default();
        match subject {
            Input::Document(doc) => {
                read(&doc.name, &String::from_utf8_lossy(&doc.bytes), &mut chunk);
            }
            Input::Files(paths) => {
                for path in paths {
                    match host::read(&path) {
                        Ok(bytes) => read(&path, &String::from_utf8_lossy(&bytes), &mut chunk),
                        Err(why) => {
                            chunk.skipped += 1;
                            chunk.notes.push(format!("{path}: {why}"));
                        }
                    }
                }
            }
        }
        rmp_serde::to_vec(&chunk).map_err(|e| format!("serializing a partial: {e}"))
    }

    fn assemble(partials: Vec<Vec<u8>>, _options: Vec<(String, String)>) -> Result<Output, String> {
        let mut all = Chunk::default();
        for bytes in &partials {
            let c: Chunk = rmp_serde::from_slice(bytes)
                .map_err(|e| format!("a partial did not decode: {e}"))?;
            all.nodes.extend(c.nodes);
            all.edges.extend(c.edges);
            all.workloads.extend(c.workloads);
            all.selectors.extend(c.selectors);
            all.namespaces.extend(c.namespaces);
            all.skipped += c.skipped;
            all.notes.extend(c.notes);
        }
        Ok(resolve(all))
    }
}

export_plugin!(K8s);

/// The whole-tree pass: one node per namespace however many objects named it,
/// and selectors matched against every workload the tree holds.
fn resolve(all: Chunk) -> Output {
    let mut out = output();
    for (key, label, props) in all.nodes {
        out.nodes.push(dr_strange_ext::Node {
            key,
            label,
            extra_labels: Vec::new(),
            properties: props,
        });
    }
    for (src, ty, dst, props) in all.edges {
        out.edges.push(dr_strange_ext::Edge {
            src,
            dst,
            type_: ty,
            properties: props,
        });
    }

    // A namespace is named by every object in it; it is one node.
    let mut seen: Vec<String> = Vec::new();
    for ns in all.namespaces {
        if seen.contains(&ns) {
            continue;
        }
        seen.push(ns.clone());
        out.nodes
            .push(node(&ns, "Namespace").prop("kind", "kubernetes").build());
    }

    // A Service names pods by label, and pods come from a workload's
    // template — so the edge is the selector matching that template, within
    // one namespace. A selector matching nothing leaves no edge, and the
    // selector stays on the node so a reader sees what was asked for.
    let mut unmatched = 0usize;
    for (ns, svc, selector) in &all.selectors {
        let mut hit = false;
        for (wns, wkey, labels) in &all.workloads {
            if wns != ns {
                continue;
            }
            if selector
                .iter()
                .all(|(k, v)| labels.iter().any(|(lk, lv)| lk == k && lv == v))
            {
                out.edges.push(
                    edge(svc, "SELECTS", wkey)
                        .described(
                            "matched_by",
                            "how the service was tied to the workload",
                            "selector",
                        )
                        .build(),
                );
                hit = true;
            }
        }
        if !hit {
            unmatched += 1;
        }
    }

    out.report.skipped = all.skipped;
    for n in all.notes {
        out.note(n);
    }
    if unmatched > 0 {
        out.note(format!(
            "{unmatched} service selector(s) matched no workload in this tree — \
             the selector is on the node, and what it names may simply live elsewhere"
        ));
    }
    out.finish()
}

fn read(path: &str, text: &str, chunk: &mut Chunk) {
    let docs = match YamlLoader::load_from_str(text) {
        Ok(docs) => docs,
        Err(why) => {
            chunk.skipped += 1;
            chunk.notes.push(format!("{path}: not valid YAML ({why})"));
            return;
        }
    };
    let mut objects = 0usize;
    for doc in &docs {
        if doc.is_badvalue() || doc.is_null() {
            continue;
        }
        let (Some(_api), Some(kind)) = (doc["apiVersion"].as_str(), doc["kind"].as_str()) else {
            chunk.skipped += 1;
            continue;
        };
        objects += 1;
        object(kind, doc, chunk);
    }
    if objects == 0 {
        chunk.notes.push(format!(
            "{path}: no object with both `apiVersion` and `kind` — not a Kubernetes manifest"
        ));
    }
}

/// `metadata.name`, `metadata.namespace`, and the labels on the object.
fn meta(doc: &Yaml) -> (String, String) {
    let name = doc["metadata"]["name"].as_str().unwrap_or("").to_string();
    let ns = doc["metadata"]["namespace"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("default")
        .to_string();
    (name, ns)
}

fn labels(y: &Yaml) -> BTreeMap<String, String> {
    match y {
        Yaml::Hash(h) => h
            .iter()
            .filter_map(|(k, v)| Some((k.as_str()?.to_string(), scalar(v)?)))
            .collect(),
        _ => BTreeMap::new(),
    }
}

/// A scalar as written — a YAML number or bool is still the text the author
/// typed, because that is the fact.
fn scalar(y: &Yaml) -> Option<String> {
    match y {
        Yaml::String(s) => Some(s.clone()),
        Yaml::Integer(i) => Some(i.to_string()),
        Yaml::Real(r) => Some(r.clone()),
        Yaml::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

fn object(kind: &str, doc: &Yaml, chunk: &mut Chunk) {
    let (name, ns) = meta(doc);
    if name.is_empty() && kind != "Namespace" {
        chunk.skipped += 1;
        return;
    }
    match kind {
        "Namespace" => {
            let own = doc["metadata"]["name"].as_str().unwrap_or("").to_string();
            if !own.is_empty() {
                chunk.namespaces.push(own.clone());
            }
        }
        k if WORKLOAD_KINDS.contains(&k) => {
            chunk.namespaces.push(ns.clone());
            workload(k, &name, &ns, doc, chunk);
        }
        "Service" => {
            chunk.namespaces.push(ns.clone());
            service(&name, &ns, doc, chunk);
        }
        "Ingress" => {
            chunk.namespaces.push(ns.clone());
            ingress(&name, &ns, doc, chunk);
        }
        "ConfigMap" => {
            chunk.namespaces.push(ns.clone());
            config_map(&name, &ns, doc, chunk);
        }
        "Secret" => {
            chunk.namespaces.push(ns.clone());
            secret(&name, &ns, doc, chunk);
        }
        "PersistentVolumeClaim" => {
            chunk.namespaces.push(ns.clone());
            let key = format!("{ns}/Volume/{name}");
            let mut n = node(&key, "Volume")
                .prop("name", name.as_str())
                .prop("kind", "pvc");
            if let Some(size) = scalar(&doc["spec"]["resources"]["requests"]["storage"]) {
                n = n.prop("storage", size);
            }
            if let Some(class) = scalar(&doc["spec"]["storageClassName"]) {
                n = n.prop("storage_class", class);
            }
            chunk.push_node(n.build());
            chunk.push_edge(edge(&key, "IN_NAMESPACE", &ns).build());
        }
        other => {
            // Recorded, not understood: a reader can see something was here.
            chunk.namespaces.push(ns.clone());
            let key = format!("{ns}/{other}/{name}");
            chunk.push_node(
                node(&key, "Resource")
                    .prop("kind", other)
                    .prop("name", name.as_str())
                    .build(),
            );
            chunk.push_edge(edge(&key, "IN_NAMESPACE", &ns).build());
        }
    }
}

/// The pod template, which for a CronJob sits one level deeper.
fn pod_template<'a>(kind: &str, doc: &'a Yaml) -> &'a Yaml {
    match kind {
        "CronJob" => &doc["spec"]["jobTemplate"]["spec"]["template"],
        "Pod" => doc,
        _ => &doc["spec"]["template"],
    }
}

fn workload(kind: &str, name: &str, ns: &str, doc: &Yaml, chunk: &mut Chunk) {
    let key = format!("{ns}/{kind}/{name}");
    let template = pod_template(kind, doc);
    let pod_labels = labels(&template["metadata"]["labels"]);

    let mut n = node(&key, "Workload").prop("kind", kind).prop("name", name);
    if let Some(replicas) = scalar(&doc["spec"]["replicas"]) {
        n = n.prop("replicas", replicas);
    }
    if let Some(schedule) = scalar(&doc["spec"]["schedule"]) {
        n = n.prop("schedule", schedule);
    }
    if !pod_labels.is_empty() {
        n = n.described(
            "pod_labels",
            "the labels on its pod template — what a service's selector matches",
            Value::from(
                pod_labels
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>(),
            ),
        );
    }
    chunk.push_node(n.build());
    chunk.push_edge(edge(&key, "IN_NAMESPACE", ns).build());
    chunk.workloads.push((
        ns.to_string(),
        key.clone(),
        pod_labels.into_iter().collect(),
    ));

    // How a volume name maps to what backs it, so a container's mount can
    // reach the ConfigMap or claim rather than stopping at a local name.
    let mut backing: BTreeMap<String, (String, String)> = BTreeMap::new();
    if let Yaml::Array(vols) = &template["spec"]["volumes"] {
        for v in vols {
            let Some(vname) = v["name"].as_str() else {
                continue;
            };
            let found = scalar(&v["configMap"]["name"])
                .map(|n| ("ConfigMap".to_string(), n))
                .or_else(|| scalar(&v["secret"]["secretName"]).map(|n| ("Secret".to_string(), n)))
                .or_else(|| {
                    scalar(&v["persistentVolumeClaim"]["claimName"])
                        .map(|n| ("Volume".to_string(), n))
                });
            if let Some(pair) = found {
                backing.insert(vname.to_string(), pair);
            }
        }
    }

    for (list, role) in [
        (&template["spec"]["containers"], "container"),
        (&template["spec"]["initContainers"], "init"),
    ] {
        if let Yaml::Array(items) = list {
            for c in items {
                container(&key, ns, c, role, &backing, chunk);
            }
        }
    }
}

fn container(
    wkey: &str,
    ns: &str,
    spec: &Yaml,
    role: &str,
    backing: &BTreeMap<String, (String, String)>,
    chunk: &mut Chunk,
) {
    let Some(name) = spec["name"].as_str() else {
        return;
    };
    let ckey = format!("{wkey}/{name}");
    let mut n = node(&ckey, "Container")
        .prop("name", name)
        .prop("role", role);
    let env_names: Vec<String> = match &spec["env"] {
        Yaml::Array(items) => items
            .iter()
            .filter_map(|e| e["name"].as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    if !env_names.is_empty() {
        n = n.described(
            "env_names",
            "the environment variables this container is given, names only",
            Value::from(env_names),
        );
    }
    chunk.push_node(n.build());
    chunk.push_edge(edge(wkey, "RUNS", &ckey).build());

    if let Some(reference) = scalar(&spec["image"]) {
        image(&ckey, &reference, chunk);
    }

    // A mount reaches what backs the volume, not the volume's local name.
    if let Yaml::Array(mounts) = &spec["volumeMounts"] {
        for m in mounts {
            let Some(vname) = m["name"].as_str() else {
                continue;
            };
            if let Some((label, target)) = backing.get(vname) {
                let mut e = edge(&ckey, "MOUNTS", format!("{ns}/{label}/{target}"))
                    .prop("as", "file")
                    .prop("volume", vname);
                if let Some(path) = m["mountPath"].as_str() {
                    e = e.prop("path", path);
                }
                chunk.push_edge(e.build());
            }
        }
    }
    // `envFrom`, and a single key through `valueFrom`.
    if let Yaml::Array(sources) = &spec["envFrom"] {
        for f in sources {
            for (field, label) in [("configMapRef", "ConfigMap"), ("secretRef", "Secret")] {
                if let Some(target) = scalar(&f[field]["name"]) {
                    chunk.push_edge(
                        edge(&ckey, "MOUNTS", format!("{ns}/{label}/{target}"))
                            .prop("as", "envFrom")
                            .build(),
                    );
                }
            }
        }
    }
    if let Yaml::Array(items) = &spec["env"] {
        for e in items {
            for (field, label) in [("configMapKeyRef", "ConfigMap"), ("secretKeyRef", "Secret")] {
                let from = &e["valueFrom"][field];
                if let Some(target) = scalar(&from["name"]) {
                    let mut edge_ =
                        edge(&ckey, "MOUNTS", format!("{ns}/{label}/{target}")).prop("as", "env");
                    if let Some(k) = from["key"].as_str() {
                        edge_ = edge_.prop("key", k);
                    }
                    chunk.push_edge(edge_.build());
                }
            }
        }
    }
}

/// The image node and its join property, parsed by the SDK so that this
/// plugin and the Compose one cannot disagree. See arch/10-ops.md §5.
fn image(ckey: &str, reference: &str, chunk: &mut Chunk) {
    let parsed = parse_image(reference);
    let mut n = node(reference, "Image").prop("reference", reference);
    if let Some(registry) = parsed.registry {
        n = n.prop("registry", registry);
    }
    n = n.prop("repository", parsed.repository);
    if let Some(d) = parsed.digest {
        n = n.described(
            "digest",
            "pins the bytes exactly, and says nothing about provenance",
            d,
        );
    }
    if let Some(t) = parsed.tag {
        n = n.prop("tag", t);
    }
    if let Some(rev) = parsed.revision {
        n = n.described(
            "revision",
            "a git revision this tag appears to name — the join to a history plane",
            rev,
        );
    }
    chunk.push_node(n.build());
    chunk.push_edge(edge(ckey, "USES_IMAGE", reference).build());
}

fn service(name: &str, ns: &str, doc: &Yaml, chunk: &mut Chunk) {
    let key = format!("{ns}/Service/{name}");
    let selector = labels(&doc["spec"]["selector"]);
    let mut n = node(&key, "Service").prop("name", name);
    if let Some(ty) = scalar(&doc["spec"]["type"]) {
        n = n.prop("type", ty);
    }
    if !selector.is_empty() {
        n = n.described(
            "selector",
            "the labels it names — what it asked for, matched or not",
            Value::from(
                selector
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>(),
            ),
        );
    }
    if let Yaml::Array(ports) = &doc["spec"]["ports"] {
        let listed: Vec<String> = ports
            .iter()
            .filter_map(|p| {
                let port = scalar(&p["port"])?;
                Some(match scalar(&p["targetPort"]) {
                    Some(t) => format!("{port}->{t}"),
                    None => port,
                })
            })
            .collect();
        if !listed.is_empty() {
            n = n.described(
                "ports",
                "`port` and its `targetPort` where one is set",
                Value::from(listed),
            );
        }
    }
    chunk.push_node(n.build());
    chunk.push_edge(edge(&key, "IN_NAMESPACE", ns).build());
    if !selector.is_empty() {
        chunk
            .selectors
            .push((ns.to_string(), key, selector.into_iter().collect()));
    }
}

fn ingress(name: &str, ns: &str, doc: &Yaml, chunk: &mut Chunk) {
    let key = format!("{ns}/Ingress/{name}");
    chunk.push_node(node(&key, "Ingress").prop("name", name).build());
    chunk.push_edge(edge(&key, "IN_NAMESPACE", ns).build());

    // A default backend, and every host/path rule.
    let default = &doc["spec"]["defaultBackend"]["service"]["name"];
    if let Some(svc) = default.as_str() {
        chunk.push_edge(
            edge(&key, "ROUTES_TO", format!("{ns}/Service/{svc}"))
                .prop("path", "/")
                .build(),
        );
    }
    let Yaml::Array(rules) = &doc["spec"]["rules"] else {
        return;
    };
    for rule in rules {
        let host_name = rule["host"].as_str();
        let Yaml::Array(paths) = &rule["http"]["paths"] else {
            continue;
        };
        for p in paths {
            let backend = &p["backend"]["service"];
            let Some(svc) = backend["name"].as_str() else {
                continue;
            };
            let mut e = edge(&key, "ROUTES_TO", format!("{ns}/Service/{svc}"));
            if let Some(h) = host_name {
                e = e.prop("host", h);
            }
            if let Some(path) = p["path"].as_str() {
                e = e.prop("path", path);
            }
            if let Some(port) =
                scalar(&backend["port"]["number"]).or_else(|| scalar(&backend["port"]["name"]))
            {
                e = e.prop("port", port);
            }
            chunk.push_edge(e.build());
        }
    }
}

fn config_map(name: &str, ns: &str, doc: &Yaml, chunk: &mut Chunk) {
    let key = format!("{ns}/ConfigMap/{name}");
    let mut n = node(&key, "ConfigMap").prop("name", name);
    // Values are recorded: choosing `ConfigMap` over `Secret` is the author
    // saying these are not sensitive. They land in the plane, and therefore
    // in its backups.
    if let Yaml::Hash(h) = &doc["data"] {
        let pairs: Vec<String> = h
            .iter()
            .filter_map(|(k, v)| Some(format!("{}={}", k.as_str()?, scalar(v)?)))
            .collect();
        if !pairs.is_empty() {
            n = n.described(
                "data",
                "its keys and values, as written",
                Value::from(pairs),
            );
        }
    }
    if let Yaml::Hash(h) = &doc["binaryData"] {
        let names: Vec<String> = h
            .keys()
            .filter_map(|k| k.as_str().map(str::to_string))
            .collect();
        if !names.is_empty() {
            n = n.described(
                "binary_keys",
                "binary entries, names only",
                Value::from(names),
            );
        }
    }
    chunk.push_node(n.build());
    chunk.push_edge(edge(&key, "IN_NAMESPACE", ns).build());
}

fn secret(name: &str, ns: &str, doc: &Yaml, chunk: &mut Chunk) {
    let key = format!("{ns}/Secret/{name}");
    let mut n = node(&key, "Secret").prop("name", name);
    if let Some(ty) = scalar(&doc["type"]) {
        n = n.prop("type", ty);
    }
    // Names only, in any encoding. base64 is not encryption, and a plugin
    // that read it would put real credentials in a graph that gets copied.
    let mut names: Vec<String> = Vec::new();
    for field in ["data", "stringData"] {
        if let Yaml::Hash(h) = &doc[field] {
            names.extend(h.keys().filter_map(|k| k.as_str().map(str::to_string)));
        }
    }
    if !names.is_empty() {
        names.sort();
        names.dedup();
        n = n.described(
            "keys",
            "the keys it holds, names only — a Secret's values are never recorded",
            Value::from(names),
        );
    }
    chunk.push_node(n.build());
    chunk.push_edge(edge(&key, "IN_NAMESPACE", ns).build());
}

#[cfg(test)]
mod tests;
