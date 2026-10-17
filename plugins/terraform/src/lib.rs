//! What a Terraform configuration *declares* it builds.
//!
//! The third reader of the ops graph (`arch/10-ops.md` in the main
//! repository), and the layer underneath the other two: Compose and
//! Kubernetes describe what runs, Terraform describes what it runs *on* —
//! the cluster, the database, the load balancer, the DNS record.
//!
//! ## Where its graph lives: references
//!
//! Terraform's own dependency graph is not written down anywhere. It is
//! *derived* from references: `subnet_id = aws_subnet.main.id` is a
//! dependency on `aws_subnet.main`, and `terraform plan` works it out the
//! same way. So the edges here come from walking every expression for a
//! traversal whose root names a resource, a data source, a module output or
//! a variable — which is why an explicit `depends_on` is the *less*
//! interesting half of the answer.
//!
//! ## Routed by extension, which is safe here
//!
//! `.tf` belongs to Terraform and to nothing else, so unlike the Kubernetes
//! reader this plugin can claim its extension outright and a tree is read
//! without being told to.
//!
//! Two Terraform files are **never read**: `*.tfvars` is values rather than
//! declarations and is where credentials are kept, and `terraform.tfstate`
//! is runtime state holding every attribute of every resource in plain text.
//! Neither is a declaration, so neither is in scope — before the secret
//! question even arises. Neither extension is claimed.
//!
//! ## Sensitive, as the author declared it
//!
//! A `variable` or `output` marked `sensitive = true` has its value withheld;
//! anything else is recorded. That is the same rule the Kubernetes reader
//! applies to a ConfigMap over a Secret, and for the same reason: the author
//! said which it was, so nothing has to be guessed.

use dr_strange_ext::{
    Guest, Input, Manifest, Output, OutputExt, edge, export_plugin, host, node, ops::parse_image,
    output,
};
use hcl::expr::{Expression, Traversal};
use hcl::structure::Block;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Shown beside the name in UIs: an original mark, not a trademarked logo.
const LOGO: &str = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24'><path d='M4 7l5 2.9v6L4 13zM10 10.4l5 2.9v6l-5-2.9zM16 7l4 2.3v5.4L16 12.4z' fill='#7b42bc'/></svg>";

/// Roots that are Terraform's own meta-values rather than a reference to
/// something the configuration declares.
const META_ROOTS: &[&str] = &[
    "each",
    "count",
    "self",
    "path",
    "terraform",
    "local",
    "locals",
];

/// Argument names that carry a container image, across the providers that
/// run containers. The `Image` node they mint is the same one the Kubernetes
/// reader mints, under the same key.
const IMAGE_ARGS: &[&str] = &["image", "image_uri", "container_image"];

struct Terraform;

impl Guest for Terraform {
    fn describe() -> Manifest {
        Manifest {
            name: "terraform".into(),
            version: "1".into(),
            // `.tf` is Terraform's and nobody else's.
            //
            // `tfvars` and `tfstate` are claimed **in order to decline
            // them**. The host buckets each file to exactly one reader with
            // no fallback, so claiming an extension is the only way to stop
            // the built-in document reader taking it and putting its
            // contents in the prose — which for a `.tfvars` means a
            // credential in the graph. Not claiming them was the bug.
            extensions: vec!["tf".into(), "tfvars".into(), "tfstate".into()],
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
            all.roots.extend(c.roots);
            all.declared.extend(c.declared);
            all.skipped += c.skipped;
            all.notes.extend(c.notes);
        }
        Ok(resolve(all))
    }
}

export_plugin!(Terraform);

/// `(key, label, properties)` and `(src, type, dst, properties)` — what a
/// chunk settled on its own.
type NodeRow = (String, String, String);
type EdgeRow = (String, String, String, String);

/// One chunk's findings. A reference may name something declared in another
/// file, so the keys this chunk *declared* travel with it: the whole-tree
/// pass is what decides whether an edge points at something real.
#[derive(Default, Serialize, Deserialize)]
struct Chunk {
    nodes: Vec<NodeRow>,
    edges: Vec<EdgeRow>,
    /// Root-module directories, which stand in for a namespace.
    roots: Vec<String>,
    /// Every key this chunk declared.
    declared: Vec<String>,
    skipped: u32,
    notes: Vec<String>,
}

impl Chunk {
    fn push_node(&mut self, n: dr_strange_ext::Node) {
        self.declared.push(n.key.clone());
        self.nodes.push((n.key, n.label, n.properties));
    }

    fn push_edge(&mut self, e: dr_strange_ext::Edge) {
        self.edges.push((e.src, e.type_, e.dst, e.properties));
    }
}

/// The whole-tree pass: one node per root module, and references kept only
/// where they name something the tree actually declares.
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

    let mut seen: Vec<String> = Vec::new();
    for root in all.roots {
        if seen.contains(&root) {
            continue;
        }
        seen.push(root.clone());
        out.nodes
            .push(node(&root, "Namespace").prop("kind", "terraform").build());
    }

    // A reference to something no file in this tree declares is a reference
    // to a module's own internals, or to a configuration that simply is not
    // here. Recording the edge would point at a node nobody minted.
    let mut dangling = 0usize;
    for (src, ty, dst, props) in all.edges {
        if !all.declared.contains(&dst) && !seen.contains(&dst) {
            dangling += 1;
            continue;
        }
        out.edges.push(dr_strange_ext::Edge {
            src,
            dst,
            type_: ty,
            properties: props,
        });
    }

    out.report.skipped = all.skipped;
    for n in all.notes {
        out.note(n);
    }
    if dangling > 0 {
        out.note(format!(
            "{dangling} reference(s) named something this tree does not declare — \
             a module's internals, or a configuration that lives elsewhere"
        ));
    }
    out.finish()
}

/// The root module a file belongs to: its directory, which is the unit
/// `terraform apply` is run in. Terraform has no namespace of its own.
fn root_of(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, _)) if !dir.is_empty() => dir.to_string(),
        _ => ".".to_string(),
    }
}

fn read(path: &str, text: &str, chunk: &mut Chunk) {
    // Values and state are declined here, which is why their extensions are
    // claimed at all: a reader that declines them keeps them away from the
    // document reader, and nothing in the tree puts their contents into the
    // graph.
    let base = path.rsplit('/').next().unwrap_or(path);
    if base.ends_with(".tfvars") || base.ends_with(".tfvars.json") || base.contains(".tfstate") {
        chunk.skipped += 1;
        chunk.notes.push(format!(
            "{path}: values and state are not declarations, and are never read"
        ));
        return;
    }

    let body = match hcl::parse(text) {
        Ok(body) => body,
        Err(why) => {
            chunk.skipped += 1;
            chunk.notes.push(format!("{path}: not valid HCL ({why})"));
            return;
        }
    };

    let root = root_of(path);
    chunk.roots.push(root.clone());
    let mut blocks = 0usize;
    for block in body.blocks() {
        blocks += 1;
        structure(block, &root, chunk);
    }
    if blocks == 0 {
        chunk
            .notes
            .push(format!("{path}: no blocks — nothing is declared here"));
    }
}

/// A label of a block, as written.
fn label_at(block: &Block, i: usize) -> Option<String> {
    block.labels.get(i).map(|l| l.as_str().to_string())
}

fn structure(block: &Block, root: &str, chunk: &mut Chunk) {
    match block.identifier.as_str() {
        "resource" => resource_block(block, root, chunk),
        "data" => data_block(block, root, chunk),
        "module" => module_block(block, root, chunk),
        "variable" => variable_block(block, root, chunk),
        "output" => output_block(block, root, chunk),
        "provider" => provider_block(block, root, chunk),
        // `terraform`, `locals`, `moved`, `import`, and anything a future
        // version adds: there is nothing to key them by, so nothing is
        // invented for them.
        _ => {}
    }
}

/// Two labels, `<type>` and `<name>`, which together are the address.
fn two_labels(block: &Block, chunk: &mut Chunk) -> Option<(String, String)> {
    match (label_at(block, 0), label_at(block, 1)) {
        (Some(a), Some(b)) => Some((a, b)),
        _ => {
            chunk.skipped += 1;
            None
        }
    }
}

fn one_label(block: &Block, chunk: &mut Chunk) -> Option<String> {
    match label_at(block, 0) {
        Some(a) => Some(a),
        None => {
            chunk.skipped += 1;
            None
        }
    }
}

fn resource_block(block: &Block, root: &str, chunk: &mut Chunk) {
    let Some((ty, name)) = two_labels(block, chunk) else {
        return;
    };
    let key = format!("{ty}.{name}");
    let mut n = node(&key, "Infra")
        .prop("type", ty.as_str())
        .prop("name", name.as_str());
    if let Some(provider) = ty.split('_').next() {
        n = n.described(
            "provider",
            "the provider the type belongs to, read off the type's prefix",
            provider,
        );
    }
    if let Some(count) = attr_text(block, "count") {
        n = n.prop("count", count);
    }
    chunk.push_node(n.build());
    chunk.push_edge(edge(&key, "IN_NAMESPACE", root).build());
    if let Some(provider) = ty.split('_').next() {
        chunk.push_edge(edge(&key, "PROVIDED_BY", format!("provider.{provider}")).build());
    }
    images(block, &key, chunk);
    references(block, &key, chunk);
}

fn data_block(block: &Block, root: &str, chunk: &mut Chunk) {
    let Some((ty, name)) = two_labels(block, chunk) else {
        return;
    };
    let key = format!("data.{ty}.{name}");
    chunk.push_node(
        node(&key, "DataSource")
            .prop("type", ty.as_str())
            .prop("name", name.as_str())
            .build(),
    );
    chunk.push_edge(edge(&key, "IN_NAMESPACE", root).build());
    references(block, &key, chunk);
}

fn module_block(block: &Block, root: &str, chunk: &mut Chunk) {
    let Some(name) = one_label(block, chunk) else {
        return;
    };
    let key = format!("module.{name}");
    let mut n = node(&key, "Module").prop("name", name.as_str());
    if let Some(source) = attr_text(block, "source") {
        n = n.prop("source", source);
    }
    if let Some(version) = attr_text(block, "version") {
        n = n.prop("version", version);
    }
    chunk.push_node(n.build());
    chunk.push_edge(edge(root, "USES_MODULE", &key).build());
    references(block, &key, chunk);
}

fn variable_block(block: &Block, root: &str, chunk: &mut Chunk) {
    let Some(name) = one_label(block, chunk) else {
        return;
    };
    let key = format!("var.{name}");
    let sensitive = attr_text(block, "sensitive").as_deref() == Some("true");
    let mut n = node(&key, "Variable")
        .prop("name", name.as_str())
        .prop("sensitive", if sensitive { "true" } else { "false" });
    if let Some(ty) = attr_text(block, "type") {
        n = n.prop("type", ty);
    }
    // Recorded unless the author declared it sensitive. That declaration is
    // the whole rule — see the module docs.
    match (sensitive, attr_text(block, "default")) {
        (false, Some(default)) => n = n.prop("default", default),
        (true, Some(_)) => {
            n = n.described(
                "default",
                "withheld: the variable declares itself sensitive",
                "(sensitive)",
            );
        }
        _ => {}
    }
    chunk.push_node(n.build());
    chunk.push_edge(edge(&key, "IN_NAMESPACE", root).build());
}

fn output_block(block: &Block, root: &str, chunk: &mut Chunk) {
    let Some(name) = one_label(block, chunk) else {
        return;
    };
    let key = format!("output.{name}");
    let sensitive = attr_text(block, "sensitive").as_deref() == Some("true");
    let mut n = node(&key, "Output")
        .prop("name", name.as_str())
        .prop("sensitive", if sensitive { "true" } else { "false" });
    if !sensitive && let Some(value) = attr_text(block, "value") {
        n = n.prop("value", value);
    }
    chunk.push_node(n.build());
    chunk.push_edge(edge(&key, "IN_NAMESPACE", root).build());
    // An output names what it publishes, which is an EXPOSES edge rather
    // than a dependency.
    for target in block
        .body
        .attributes()
        .filter(|a| a.key.as_str() == "value")
        .flat_map(|a| refs_in(&a.expr))
    {
        if let Some(dst) = address_of(&target) {
            chunk.push_edge(edge(&key, "EXPOSES", dst).build());
        }
    }
}

fn provider_block(block: &Block, root: &str, chunk: &mut Chunk) {
    let Some(name) = one_label(block, chunk) else {
        return;
    };
    let key = format!("provider.{name}");
    let mut n = node(&key, "Provider").prop("name", name.as_str());
    if let Some(region) = attr_text(block, "region") {
        n = n.prop("region", region);
    }
    chunk.push_node(n.build());
    chunk.push_edge(edge(&key, "IN_NAMESPACE", root).build());
}

/// One attribute's value as written, flattened to a single line.
fn attr_text(block: &Block, key: &str) -> Option<String> {
    let expr = block
        .body
        .attributes()
        .find(|a| a.key.as_str() == key)
        .map(|a| &a.expr)?;
    let text = match expr {
        Expression::String(s) => s.clone(),
        Expression::Bool(b) => b.to_string(),
        Expression::Number(n) => n.to_string(),
        other => format!("{other}"),
    };
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!flat.is_empty()).then_some(flat)
}

/// A container image named anywhere in this block, including one level in —
/// an ECS task definition puts it inside `container_definitions`, a Cloud Run
/// service inside `template.spec.containers`.
fn images(block: &Block, key: &str, chunk: &mut Chunk) {
    fn scan(body: &hcl::Body, key: &str, chunk: &mut Chunk, depth: usize) {
        if depth > 4 {
            return;
        }
        for a in body.attributes() {
            if !IMAGE_ARGS.contains(&a.key.as_str()) {
                continue;
            }
            let Expression::String(reference) = &a.expr else {
                // An interpolated image is a reference, not a reference to an
                // image: it is caught as a dependency instead.
                continue;
            };
            let parsed = parse_image(reference);
            let mut n = node(reference, "Image").prop("reference", reference.as_str());
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
            chunk.push_edge(edge(key, "USES_IMAGE", reference.as_str()).build());
        }
        for inner in body.blocks() {
            scan(&inner.body, key, chunk, depth + 1);
        }
    }
    scan(&block.body, key, chunk, 0);
}

/// Every reference this block makes, as a dependency edge.
///
/// This is where Terraform's graph actually is: `subnet_id =
/// aws_subnet.main.id` is a dependency, and `terraform plan` reads it the
/// same way. An explicit `depends_on` is recorded too, and says so.
fn references(block: &Block, key: &str, chunk: &mut Chunk) {
    let mut seen: BTreeMap<String, &str> = BTreeMap::new();
    for a in block.body.attributes() {
        let explicit = a.key.as_str() == "depends_on";
        for t in refs_in(&a.expr) {
            if let Some(dst) = address_of(&t) {
                seen.entry(dst)
                    .or_insert(if explicit { "depends_on" } else { "reference" });
            }
        }
    }
    // Nested blocks reference things too — a `network_interface` naming a
    // subnet is the ordinary shape.
    fn nested(body: &hcl::Body, seen: &mut BTreeMap<String, &'static str>, depth: usize) {
        if depth > 4 {
            return;
        }
        for inner in body.blocks() {
            for a in inner.body.attributes() {
                for t in refs_in(&a.expr) {
                    if let Some(dst) = address_of(&t) {
                        seen.entry(dst).or_insert("reference");
                    }
                }
            }
            nested(&inner.body, seen, depth + 1);
        }
    }
    nested(&block.body, &mut seen, 0);

    for (dst, via) in seen {
        if dst == key {
            continue; // a block referring to itself through `self`
        }
        let ty = if dst.starts_with("var.") {
            "USES_VARIABLE"
        } else {
            "DEPENDS_ON"
        };
        chunk.push_edge(
            edge(key, ty, dst)
                .described("via", "how the dependency was stated", via)
                .build(),
        );
    }
}

/// The traversals inside an expression, however deeply nested.
fn refs_in(expr: &Expression) -> Vec<Traversal> {
    let mut found = Vec::new();
    walk(expr, &mut found);
    found
}

fn walk(expr: &Expression, found: &mut Vec<Traversal>) {
    match expr {
        Expression::Traversal(t) => {
            found.push((**t).clone());
            walk(&t.expr, found);
        }
        Expression::Array(items) => items.iter().for_each(|e| walk(e, found)),
        Expression::Object(obj) => obj.values().for_each(|e| walk(e, found)),
        Expression::Parenthesis(inner) => walk(inner, found),
        Expression::FuncCall(call) => call.args.iter().for_each(|e| walk(e, found)),
        Expression::Conditional(c) => {
            walk(&c.cond_expr, found);
            walk(&c.true_expr, found);
            walk(&c.false_expr, found);
        }
        Expression::Operation(op) => match &**op {
            hcl::expr::Operation::Unary(u) => walk(&u.expr, found),
            hcl::expr::Operation::Binary(b) => {
                walk(&b.lhs_expr, found);
                walk(&b.rhs_expr, found);
            }
        },
        Expression::ForExpr(f) => {
            walk(&f.collection_expr, found);
            if let Some(e) = &f.key_expr {
                walk(e, found);
            }
            walk(&f.value_expr, found);
            if let Some(e) = &f.cond_expr {
                walk(e, found);
            }
        }
        // A template's interpolations are parsed into their own expressions,
        // which `hcl` exposes as a string here. `"${aws_vpc.main.id}/24"` is
        // therefore not followed — stated as a limit rather than guessed at
        // with a regex.
        _ => {}
    }
}

/// The address a traversal names, when it names something a configuration
/// declares: `aws_subnet.main.id` → `aws_subnet.main`, `var.x.y` → `var.x`,
/// `data.aws_ami.ubuntu.id` → `data.aws_ami.ubuntu`.
fn address_of(t: &Traversal) -> Option<String> {
    let Expression::Variable(root) = &t.expr else {
        return None;
    };
    let root = root.as_str();
    if META_ROOTS.contains(&root) {
        return None;
    }
    let parts: Vec<String> = t
        .operators
        .iter()
        .filter_map(|op| match op {
            hcl::expr::TraversalOperator::GetAttr(id) => Some(id.as_str().to_string()),
            _ => None,
        })
        .collect();
    match root {
        "var" => parts.first().map(|n| format!("var.{n}")),
        "module" => parts.first().map(|n| format!("module.{n}")),
        "data" => (parts.len() >= 2).then(|| format!("data.{}.{}", parts[0], parts[1])),
        // A resource reference is `<type>.<name>`, and a type always carries
        // an underscore: `aws_subnet.main`. Without one it is a local value
        // or something this plugin has no address for.
        ty if ty.contains('_') => parts.first().map(|n| format!("{ty}.{n}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
