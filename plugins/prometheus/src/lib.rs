//! What a Prometheus configuration *declares* it scrapes, and what it alerts on.
//!
//! The third ops reader after Compose and Kubernetes ([arch/10-ops.md] in the
//! main repository), and the first to read a file whose subject is not a thing
//! that runs but a thing that *watches* what runs. A scrape job names an
//! address; a Service node already exists at that address; the edge between
//! them is the point of this plugin.
//!
//! ## The model
//!
//! A `ScrapeJob` HAS_TARGET one `ScrapeTarget` per declared address, and a
//! target SCRAPES the `Service` or `Workload` it resolves to. An `Alert`
//! ALERTS_ON every `Metric` its expression names, and on the job the
//! expression selects by `job=`. A recording rule mints the `Metric` it
//! records, which DERIVES_FROM the metrics it is computed over.
//!
//! ## Where a target's service comes from, and where it does not
//!
//! Only one address form yields a node key on its own: Kubernetes cluster DNS,
//! `api.prod.svc.cluster.local`, which is `prod/Service/api` and nothing else.
//! That edge is drawn here, by this plugin, with `matched_by: dns`.
//!
//! A bare `api:9090` — which is how a Compose stack and a short-name
//! Kubernetes target are both written — names no namespace and so no key. It
//! is left to the host, which can see every ops node in the plane and match on
//! the name, and which marks what it finds `matched_by: name` with a lower
//! confidence. Guessing a key here would produce an edge that looks exactly
//! like the certain one.
//!
//! ## Rule files are followed, not guessed
//!
//! A rule file has no fixed name, so no filename routing can claim it. The
//! configuration declares its own under `rule_files:`, and this plugin follows
//! that declaration — a glob is resolved against the tree the host exposes.
//! One consequence is worth knowing: a rule file edited *alone*, with its
//! configuration untouched, is re-read at the next full digest rather than by
//! an incremental fold, because the fold re-parses the files that changed and
//! the file that changed is claimed by no plugin. Pointing the host at a rules
//! tree explicitly (`--handler prometheus`) reads them as their own subject.
//!
//! ## Values, and what is not recorded
//!
//! Scalars are kept **as written**, the rule every ops reader follows: a
//! `__address__` that a relabel rule will rewrite stays the address the author
//! typed, because a target resolved against a cluster this parser cannot see
//! is a fact about nothing.
//!
//! **No `basic_auth`, `authorization`, `oauth2` or `tls_config` value is
//! recorded** — not the password, not the credentials file, not the bearer
//! token. Their *presence* is, as `auth: basic`, because "this job
//! authenticates" is a fact about the topology and the secret is not.

use dr_strange_ext::{
    Input, Manifest, Output, OutputExt, Simple, Value, edge, host, node, output, simple_plugin,
};
use yaml_rust2::{Yaml, YamlLoader};

/// Shown beside the name in UIs: an original mark, not a trademarked logo.
const LOGO: &str = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24'><path d='M12 2.2c2.4 3.1 1 5.2 2.5 7 1 1.2 2.3 1.9 2.3 4a4.8 4.8 0 0 1-9.6 0c0-2.3 1.5-3.2 2.3-5s.6-4 2.5-6z' fill='#e6522c'/><rect x='8.4' y='14.4' width='7.2' height='2.1' rx='1' fill='#fff'/></svg>";

struct Prometheus;

impl Simple for Prometheus {
    fn describe() -> Manifest {
        Manifest {
            name: "prometheus".into(),
            version: "1".into(),
            // Deliberately none, for the reason the Compose plugin gives:
            // claiming `.yml` would take every manifest and workflow in the
            // tree from whatever reads them. The host routes `prometheus.yml`
            // to the plugin installed under this name.
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

simple_plugin!(Prometheus);

/// The configuration a file belongs to: the directory holding it, else its
/// stem. Two Prometheus configurations in one repository describe two
/// deployments, and their jobs must not collide — the rule the Compose reader
/// uses for projects.
fn instance_of(path: &str) -> String {
    let parts: Vec<&str> = path.rsplit('/').collect();
    match parts.as_slice() {
        [file] => file.split('.').next().unwrap_or("prometheus").to_string(),
        [_file, dir, ..] if !dir.is_empty() => dir.to_string(),
        _ => "prometheus".to_string(),
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

/// Every string in a value that may be one scalar or a sequence.
fn one_or_many(y: &Yaml) -> Vec<String> {
    match y {
        Yaml::Array(items) => items.iter().filter_map(written).collect(),
        other => written(other).into_iter().collect(),
    }
}

/// A file is a configuration, a rules file, or neither — decided by what it
/// declares rather than by what it is called.
enum Kind {
    Config,
    Rules,
}

/// What this document is, if it is either.
fn kind_of(doc: &Yaml) -> Option<Kind> {
    if !doc["scrape_configs"].is_badvalue() || !doc["rule_files"].is_badvalue() {
        return Some(Kind::Config);
    }
    if !doc["groups"].is_badvalue() {
        return Some(Kind::Rules);
    }
    None
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
    match kind_of(doc) {
        Some(Kind::Config) => config(path, doc, out),
        Some(Kind::Rules) => rules(&instance_of(path), path, doc, out),
        None => {
            out.report.skipped += 1;
            out.note(format!(
                "{path}: no `scrape_configs`, `rule_files` or `groups` — not a Prometheus file"
            ));
        }
    }
}

/// One configuration: its jobs, and the rule files it declares.
fn config(path: &str, doc: &Yaml, out: &mut Output) {
    let instance = instance_of(path);
    if let Yaml::Array(jobs) = &doc["scrape_configs"] {
        for spec in jobs {
            job(&instance, path, spec, out);
        }
    }
    for pattern in one_or_many(&doc["rule_files"]) {
        follow(&instance, path, &pattern, out);
    }
}

/// One `scrape_configs` entry, with every address it declares.
fn job(instance: &str, path: &str, spec: &Yaml, out: &mut Output) {
    let Some(name) = written(&spec["job_name"]) else {
        out.note(format!("{path}: a scrape config without a `job_name`"));
        return;
    };
    let key = format!("{instance}/job/{name}");
    let mut job = node(&key, "ScrapeJob")
        .prop("name", name.as_str())
        .prop("file", path);
    for (field, prop) in [
        ("metrics_path", "metrics_path"),
        ("scheme", "scheme"),
        ("scrape_interval", "scrape_interval"),
        ("scrape_timeout", "scrape_timeout"),
    ] {
        if let Some(value) = written(&spec[field]) {
            job = job.prop(prop, value);
        }
    }
    // Which authentication, never the credential — see the module docs.
    let auth: Vec<String> = ["basic_auth", "authorization", "oauth2", "tls_config"]
        .iter()
        .filter(|field| !spec[**field].is_badvalue())
        .map(|field| field.trim_end_matches("_auth").trim_end_matches("_config"))
        .map(str::to_string)
        .collect();
    if !auth.is_empty() {
        job = job.described(
            "auth",
            "which authentication the job is configured with; no credential is recorded",
            Value::from(auth),
        );
    }
    let discovery = discovery_kinds(spec);
    if !discovery.is_empty() {
        job = job.described(
            "discovery",
            "service discovery mechanisms, whose targets exist only at run time",
            Value::from(discovery.clone()),
        );
    }
    if !spec["relabel_configs"].is_badvalue() {
        job = job.described(
            "relabelled",
            "the job rewrites its targets, so the addresses below are what was written, not what is scraped",
            true,
        );
    }
    out.nodes.push(job.build());

    // A Kubernetes SD config may name the namespaces it looks in. That is a
    // key the Kubernetes reader mints, so the edge is certain.
    for ns in sd_namespaces(spec) {
        out.edges.push(edge(&key, "IN_NAMESPACE", &ns).build());
    }

    let mut declared = 0usize;
    if let Yaml::Array(statics) = &spec["static_configs"] {
        for group in statics {
            for address in one_or_many(&group["targets"]) {
                target(&key, &address, out);
                declared += 1;
            }
        }
    }
    if declared == 0 && !discovery.is_empty() {
        out.note(format!(
            "{path}: job `{name}` finds its targets through {} — none is declared in the file",
            discovery.join(", ")
        ));
    }
}

/// The service discovery mechanisms a job declares, by their Prometheus names.
fn discovery_kinds(spec: &Yaml) -> Vec<String> {
    let Yaml::Hash(fields) = spec else {
        return Vec::new();
    };
    fields
        .keys()
        .filter_map(written)
        .filter(|k| k.ends_with("_sd_configs") && k != "static_sd_configs")
        .map(|k| k.trim_end_matches("_sd_configs").to_string())
        .collect()
}

/// The namespaces a Kubernetes SD config restricts itself to.
fn sd_namespaces(spec: &Yaml) -> Vec<String> {
    let Yaml::Array(configs) = &spec["kubernetes_sd_configs"] else {
        return Vec::new();
    };
    configs
        .iter()
        .flat_map(|c| one_or_many(&c["namespaces"]["names"]))
        .collect()
}

/// One declared address: a node of its own, because it carries both the join
/// and — where no service answers to it — the fact that there was none.
fn target(job_key: &str, address: &str, out: &mut Output) {
    let key = format!("{job_key}/target/{address}");
    let (host_name, port) = split_host_port(address);
    let mut target = node(&key, "ScrapeTarget")
        .prop("address", address)
        .prop("host", host_name);
    if let Some(port) = port {
        target = target.prop("port", port);
    }
    // The key this address names, where it names one — recorded rather than
    // drawn as an edge. Whether the tree declares that service is something
    // this parser, holding one file, cannot know; the host can, and draws the
    // edge if it does.
    if let Some(service) = service_key(host_name) {
        target = target.described(
            "names",
            "the node key this address names: a Kubernetes service address and no other reading of it",
            service,
        );
    }
    out.nodes.push(target.build());
    out.edges.push(edge(job_key, "HAS_TARGET", &key).build());
}

/// An address split into host and port, leaving an IPv6 literal's own colons
/// alone.
fn split_host_port(address: &str) -> (&str, Option<&str>) {
    if let Some(end) = address.strip_prefix('[').and_then(|_| address.find(']')) {
        let host = &address[1..end];
        let rest = &address[end + 1..];
        return (host, rest.strip_prefix(':').filter(|p| !p.is_empty()));
    }
    match address.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && !host.is_empty() => (host, Some(port)),
        _ => (address, None),
    }
}

/// The Kubernetes Service a cluster DNS address names, keyed as the Kubernetes
/// reader keys it.
///
/// `api.prod.svc.cluster.local` is `prod/Service/api`. A name without the
/// `svc` label is some other host — a pod address, an external domain — and
/// yields nothing rather than a guess.
fn service_key(host: &str) -> Option<String> {
    let mut parts = host.trim_end_matches('.').split('.');
    let name = parts.next().filter(|p| !p.is_empty())?;
    let namespace = parts.next().filter(|p| !p.is_empty())?;
    (parts.next()? == "svc").then(|| format!("{namespace}/Service/{name}"))
}

/// Read what `rule_files:` points at, under the configuration's own instance
/// name so an alert can reach the job it watches.
fn follow(instance: &str, config_path: &str, pattern: &str, out: &mut Output) {
    if pattern.starts_with('/') {
        out.note(format!(
            "{config_path}: `{pattern}` is an absolute path, outside the tree this parser can read"
        ));
        return;
    }
    let pattern = join_path(config_path, pattern);
    let mut found = 0usize;
    for path in candidates() {
        if !glob_match(&pattern, &path) {
            continue;
        }
        found += 1;
        match host::read(&path) {
            Ok(bytes) => match YamlLoader::load_from_str(&String::from_utf8_lossy(&bytes)) {
                Ok(docs) => match docs.first() {
                    Some(doc) if !doc["groups"].is_badvalue() => rules(instance, &path, doc, out),
                    _ => out.note(format!("{path}: no `groups` — not a Prometheus rules file")),
                },
                Err(why) => {
                    out.report.skipped += 1;
                    out.note(format!("{path}: not valid YAML ({why})"));
                }
            },
            Err(why) => {
                out.report.skipped += 1;
                out.note(format!("{path}: {why}"));
            }
        }
    }
    if found == 0 {
        out.note(format!(
            "{config_path}: `{pattern}` matched no file in the tree"
        ));
    }
}

/// The files a rule glob may match: what the host exposes under the two
/// extensions Prometheus's own documentation uses.
fn candidates() -> Vec<String> {
    let mut paths = host::list(".yml").unwrap_or_default();
    paths.extend(host::list(".yaml").unwrap_or_default());
    paths
}

/// A pattern written beside a configuration, resolved against the directory
/// that configuration sits in.
fn join_path(config_path: &str, pattern: &str) -> String {
    let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
    match config_path.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/{pattern}"),
        None => pattern.to_string(),
    }
}

/// Whether a path matches a glob, segment by segment — `*` and `?` within one
/// segment, which is what Prometheus's own matcher does.
fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').collect();
    let path: Vec<&str> = path.split('/').collect();
    pattern.len() == path.len()
        && pattern
            .iter()
            .zip(path)
            .all(|(pat, seg)| segment_match(pat, seg))
}

/// One path segment against one pattern segment.
fn segment_match(pattern: &str, segment: &str) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = (pattern.chars().collect(), segment.chars().collect());
    let (mut pi, mut si) = (0usize, 0usize);
    // Where the last `*` was, so a failed match can give it one more character.
    let (mut star, mut resume) = (None, 0usize);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            resume = si;
            pi += 1;
        } else if let Some(back) = star {
            pi = back + 1;
            resume += 1;
            si = resume;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// One rules file: its alerting rules, and the metrics they name.
fn rules(instance: &str, path: &str, doc: &Yaml, out: &mut Output) {
    let Yaml::Array(groups) = &doc["groups"] else {
        out.note(format!("{path}: `groups` is not a sequence"));
        return;
    };
    for group in groups {
        let name = written(&group["name"]).unwrap_or_else(|| "unnamed".into());
        let Yaml::Array(items) = &group["rules"] else {
            continue;
        };
        for item in items {
            rule(instance, path, &name, item, out);
        }
    }
}

/// One rule: an alert, or a recording rule, each with what its expression
/// names.
fn rule(instance: &str, path: &str, group: &str, spec: &Yaml, out: &mut Output) {
    let expr = written(&spec["expr"]).unwrap_or_default();
    let sources = metrics_in(&expr);
    for metric in &sources {
        mint_metric(out, metric);
    }

    if let Some(record) = written(&spec["record"]) {
        let key = metric_key(&record);
        // A recording rule says more about its metric than a mention does, so
        // it replaces the bare node an earlier rule may have minted.
        out.nodes.retain(|n| n.key != key);
        out.nodes.push(
            node(&key, "Metric")
                .prop("name", record.as_str())
                .prop("file", path)
                .described(
                    "recorded",
                    "the metric is computed by a recording rule rather than scraped",
                    true,
                )
                .prop("expr", expr.as_str())
                .build(),
        );
        for source in &sources {
            if *source != record {
                out.edges
                    .push(edge(&key, "DERIVES_FROM", metric_key(source)).build());
            }
        }
        return;
    }

    let Some(name) = written(&spec["alert"]) else {
        out.note(format!(
            "{path}: a rule in group `{group}` is neither `alert` nor `record`"
        ));
        return;
    };
    let key = format!("{instance}/alert/{group}/{name}");
    let mut alert = node(&key, "Alert")
        .prop("name", name.as_str())
        .prop("group", group)
        .prop("file", path)
        .prop("expr", expr.as_str());
    for (field, prop) in [("for", "for"), ("keep_firing_for", "keep_firing_for")] {
        if let Some(value) = written(&spec[field]) {
            alert = alert.prop(prop, value);
        }
    }
    if let Some(severity) = written(&spec["labels"]["severity"]) {
        alert = alert.prop("severity", severity);
    }
    if let Some(summary) = written(&spec["annotations"]["summary"]) {
        alert = alert.prop("summary", summary);
    }
    out.nodes.push(alert.build());

    for metric in &sources {
        out.edges
            .push(edge(&key, "ALERTS_ON", metric_key(metric)).build());
    }
    // A `job=` matcher names a scrape job in this same configuration, so the
    // key is this plugin's own and the edge is certain.
    if let Some(job) = job_matcher(&expr) {
        out.edges
            .push(edge(&key, "ALERTS_ON", format!("{instance}/job/{job}")).build());
    }
}

/// Mint a metric node once: three rules naming one metric are one node, and
/// pushing it three times is a key the host counts as claimed twice.
fn mint_metric(out: &mut Output, name: &str) {
    let key = metric_key(name);
    if out.nodes.iter().any(|n| n.key == key) {
        return;
    }
    out.nodes
        .push(node(key, "Metric").prop("name", name).build());
}

/// A metric's key. Global rather than per configuration: a metric name means
/// the same thing wherever it is written, and two configurations watching one
/// metric should meet on it.
fn metric_key(name: &str) -> String {
    format!("metric/{name}")
}

/// Keywords that are not metric names, however much they look like one.
const KEYWORDS: &[&str] = &[
    "and",
    "atan2",
    "bool",
    "by",
    "end",
    "group_left",
    "group_right",
    "ignoring",
    "inf",
    "nan",
    "offset",
    "on",
    "or",
    "start",
    "unless",
    "without",
];

/// Keywords followed by a parenthesised list of **label** names, which are not
/// metrics either.
const GROUPING: &[&str] = &[
    "by",
    "without",
    "on",
    "ignoring",
    "group_left",
    "group_right",
];

/// The metric names an expression selects.
///
/// Lexical rather than parsed: an identifier counts when it is not a function
/// call, not a keyword, not a grouping label and not inside a label matcher or
/// a string. A name assembled at query time — `{__name__=~"http_.*"}` — is not
/// a selection this can see, which is why the report says so.
fn metrics_in(expr: &str) -> Vec<String> {
    let chars: Vec<char> = expr.chars().collect();
    let mut found: Vec<String> = Vec::new();
    let (mut i, mut braces, mut ranges) = (0usize, 0usize, 0usize);
    while i < chars.len() {
        match chars[i] {
            quote @ ('"' | '\'' | '`') => {
                i += 1;
                while i < chars.len() && chars[i] != quote {
                    i += if chars[i] == '\\' { 2 } else { 1 };
                }
                i += 1;
            }
            '{' => {
                braces += 1;
                i += 1;
            }
            '}' => {
                braces = braces.saturating_sub(1);
                i += 1;
            }
            '[' => {
                ranges += 1;
                i += 1;
            }
            ']' => {
                ranges = ranges.saturating_sub(1);
                i += 1;
            }
            c if c.is_ascii_alphabetic() || c == '_' || c == ':' => {
                let start = i;
                while i < chars.len()
                    && (chars[i].is_ascii_alphanumeric() || chars[i] == '_' || chars[i] == ':')
                {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                let next = chars[i..].iter().find(|c| !c.is_whitespace());
                // `5m` is a duration, whose unit follows a digit. A metric
                // name never does, because an identifier swallows its digits.
                let unit = start > 0 && chars[start - 1].is_ascii_digit();
                if GROUPING.contains(&word.as_str()) {
                    i = skip_group(&chars, i);
                } else if braces == 0
                    && ranges == 0
                    && !unit
                    && next != Some(&'(')
                    && !KEYWORDS.contains(&word.as_str())
                    && !word.is_empty()
                {
                    found.push(word);
                }
            }
            _ => i += 1,
        }
    }
    found.sort();
    found.dedup();
    found
}

/// Past the parenthesised label list a grouping keyword introduces.
fn skip_group(chars: &[char], from: usize) -> usize {
    let mut i = from;
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    if chars.get(i) != Some(&'(') {
        return i;
    }
    let mut depth = 0usize;
    while i < chars.len() {
        match chars[i] {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    i
}

/// The job an expression selects by an exact `job="…"` matcher.
///
/// Exact only: `job=~"api.*"` is a pattern over jobs that may not exist, and a
/// pattern is not a name.
fn job_matcher(expr: &str) -> Option<String> {
    let mut rest = expr;
    while let Some(at) = rest.find("job") {
        let after = rest[at + 3..].trim_start();
        if let Some(value) = after.strip_prefix('=').map(str::trim_start)
            && let Some(quoted) = value.strip_prefix('"')
            && let Some(end) = quoted.find('"')
            && !quoted[..end].is_empty()
        {
            return Some(quoted[..end].to_string());
        }
        rest = &rest[at + 3..];
    }
    None
}

#[cfg(test)]
mod tests;
