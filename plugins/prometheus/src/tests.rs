//! What the reader claims, proved against written configurations.

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

/// One property's value, with a described property's wrapper removed.
fn value(n: &dr_strange_ext::Node, name: &str) -> Value {
    let v = props(n);
    let at = &v[name];
    if at.get("$value").is_some() {
        at["$value"].clone()
    } else {
        at.clone()
    }
}

fn prop(n: &dr_strange_ext::Node, name: &str) -> String {
    value(n, name)
        .as_str()
        .unwrap_or_else(|| panic!("no string prop {name} on {}: {}", n.key, n.properties))
        .to_string()
}

fn list(n: &dr_strange_ext::Node, name: &str) -> Vec<String> {
    value(n, name)
        .as_array()
        .unwrap_or_else(|| panic!("no list prop {name} on {}: {}", n.key, n.properties))
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

fn flag(n: &dr_strange_ext::Node, name: &str) -> bool {
    value(n, name) == Value::Bool(true)
}

fn has_node(o: &Output, key: &str) -> bool {
    o.nodes.iter().any(|n| n.key == key)
}

fn has_edge(o: &Output, src: &str, ty: &str, dst: &str) -> bool {
    o.edges
        .iter()
        .any(|e| e.src == src && e.type_ == ty && e.dst == dst)
}


const CONFIG: &str = r#"
global:
  scrape_interval: 15s
scrape_configs:
  - job_name: api
    metrics_path: /metrics
    scheme: https
    basic_auth:
      username: prom
      password: hunter2
    static_configs:
      - targets: ['api.prod.svc.cluster.local:9090', 'api-canary:9090']
  - job_name: node
    static_configs:
      - targets: ['10.0.3.11:9100']
  - job_name: pods
    kubernetes_sd_configs:
      - role: pod
        namespaces:
          names: [prod]
    relabel_configs:
      - source_labels: [__meta_kubernetes_pod_label_app]
        target_label: app
"#;

/// A job, its addresses, and the one edge an address can carry on its own.
#[test]
fn a_job_declares_its_targets() {
    let o = run("deploy/prometheus.yml", CONFIG);

    assert_eq!(prop(node_at(&o, "deploy/job/api"), "name"), "api");
    assert_eq!(
        prop(node_at(&o, "deploy/job/api"), "metrics_path"),
        "/metrics"
    );
    assert!(has_edge(
        &o,
        "deploy/job/api",
        "HAS_TARGET",
        "deploy/job/api/target/api.prod.svc.cluster.local:9090"
    ));
    let target = node_at(&o, "deploy/job/api/target/api-canary:9090");
    assert_eq!(prop(target, "host"), "api-canary");
    assert_eq!(prop(target, "port"), "9090");
}

/// The one address form that names a service and no other — recorded on the
/// target, because whether this tree *declares* that service is the host's
/// question, not a parser's.
#[test]
fn a_cluster_address_records_the_service_it_names() {
    let o = run("deploy/prometheus.yml", CONFIG);
    let target = node_at(&o, "deploy/job/api/target/api.prod.svc.cluster.local:9090");
    assert_eq!(prop(target, "names"), "prod/Service/api");
    assert!(
        !o.edges.iter().any(|e| e.type_ == "SCRAPES"),
        "the reader drew an edge to a key it cannot know exists"
    );
}

/// A short name and an IP name no namespace, so this reader draws nothing and
/// leaves the match to the host, which can see the whole plane.
#[test]
fn an_address_without_a_namespace_is_left_to_the_host() {
    let o = run("deploy/prometheus.yml", CONFIG);
    assert!(
        !o.edges
            .iter()
            .any(|e| e.type_ == "SCRAPES" && e.src.contains("api-canary")),
        "a short name was resolved to a key it does not name"
    );
    assert!(
        !o.edges
            .iter()
            .any(|e| e.type_ == "SCRAPES" && e.src.contains("10.0.3.11")),
        "an IP address was resolved to a key it does not name"
    );
    // But the address itself is a node, so what was not matched is visible.
    assert_eq!(
        prop(node_at(&o, "deploy/job/node/target/10.0.3.11:9100"), "host"),
        "10.0.3.11"
    );
}

/// Which authentication, never the credential.
#[test]
fn a_credential_is_not_recorded() {
    let o = run("deploy/prometheus.yml", CONFIG);
    let job = node_at(&o, "deploy/job/api");
    assert_eq!(list(job, "auth"), vec!["basic"]);
    assert!(
        !job.properties.contains("hunter2"),
        "the password reached the graph: {}",
        job.properties
    );
}

/// Discovered targets exist at run time, which the job says rather than
/// pretending it has none.
#[test]
fn service_discovery_is_recorded_as_what_it_is() {
    let o = run("deploy/prometheus.yml", CONFIG);
    let job = node_at(&o, "deploy/job/pods");
    assert_eq!(list(job, "discovery"), vec!["kubernetes"]);
    assert!(flag(job, "relabelled"));
    // The namespace a Kubernetes SD config names is a key the k8s reader mints.
    assert!(has_edge(&o, "deploy/job/pods", "IN_NAMESPACE", "prod"));
    assert!(
        o.report
            .notes
            .iter()
            .any(|n| n.contains("job `pods` finds its targets through kubernetes")),
        "{:?}",
        o.report.notes
    );
}

const RULES: &str = r#"
groups:
  - name: api
    rules:
      - alert: HighErrorRate
        expr: sum(rate(http_requests_total{job="api",status=~"5.."}[5m])) by (job) > 0.05
        for: 10m
        labels:
          severity: page
        annotations:
          summary: the API is returning errors
      - record: job:http_requests:rate5m
        expr: sum(rate(http_requests_total[5m])) by (job)
"#;

/// An alert reaches every metric it selects, and the job it selects.
#[test]
fn an_alert_names_what_it_watches() {
    let o = run("rules/api.yml", RULES);

    let alert = node_at(&o, "rules/alert/api/HighErrorRate");
    assert_eq!(prop(alert, "severity"), "page");
    assert_eq!(prop(alert, "for"), "10m");
    assert_eq!(prop(alert, "group"), "api");
    assert!(has_edge(
        &o,
        "rules/alert/api/HighErrorRate",
        "ALERTS_ON",
        "metric/http_requests_total"
    ));
    assert!(has_edge(
        &o,
        "rules/alert/api/HighErrorRate",
        "ALERTS_ON",
        "rules/job/api"
    ));
    assert!(has_node(&o, "metric/http_requests_total"));
}

/// A recording rule mints the metric it records, and says where it came from.
#[test]
fn a_recording_rule_mints_its_metric() {
    let o = run("rules/api.yml", RULES);
    let recorded = node_at(&o, "metric/job:http_requests:rate5m");
    assert!(flag(recorded, "recorded"));
    assert!(has_edge(
        &o,
        "metric/job:http_requests:rate5m",
        "DERIVES_FROM",
        "metric/http_requests_total"
    ));
}

/// What the lexer counts, and the four things it must not.
#[test]
fn an_expression_yields_its_metric_names() {
    // A function name, a grouping label, a matcher's labels and values, and a
    // duration are none of them metrics.
    assert_eq!(
        metrics_in(r#"sum(rate(http_requests_total{job="api",code=~"5.."}[5m])) by (job, code)"#),
        vec!["http_requests_total"]
    );
    // Both sides of an operator, and a bare selector.
    assert_eq!(
        metrics_in("up == 0 or node_memory_free_bytes / node_memory_total_bytes < 0.1"),
        vec!["node_memory_free_bytes", "node_memory_total_bytes", "up"]
    );
    // `and`, `unless`, `offset`, `bool` are keywords; a recorded name with
    // colons is a metric.
    assert_eq!(
        metrics_in("job:latency:p99 offset 5m > bool 1 and up"),
        vec!["job:latency:p99", "up"]
    );
    // A metric named only inside a string is not selected by it.
    assert!(metrics_in(r#"absent(up{job="api"})"#).contains(&"up".to_string()));
    assert!(
        !metrics_in(r#"vector(0) or on() label_replace(x, "dst", "$1", "src", "(.*)")"#)
            .contains(&"dst".to_string())
    );
}

/// An exact matcher names a job; a pattern does not.
#[test]
fn only_an_exact_job_matcher_names_a_job() {
    assert_eq!(job_matcher(r#"up{job="api"}"#).as_deref(), Some("api"));
    assert_eq!(job_matcher(r#"up{ job = "api" }"#).as_deref(), Some("api"));
    assert_eq!(job_matcher(r#"up{job=~"api.*"}"#), None);
    assert_eq!(job_matcher("up"), None);
}

/// Host and port, including the bracket form an IPv6 literal needs.
#[test]
fn an_address_splits_into_host_and_port() {
    assert_eq!(split_host_port("api:9090"), ("api", Some("9090")));
    assert_eq!(split_host_port("api"), ("api", None));
    assert_eq!(split_host_port("[::1]:9090"), ("::1", Some("9090")));
    assert_eq!(split_host_port("[fe80::1]"), ("fe80::1", None));
}

/// Only a service address yields a service key.
#[test]
fn only_a_service_address_yields_a_key() {
    assert_eq!(
        service_key("api.prod.svc.cluster.local").as_deref(),
        Some("prod/Service/api")
    );
    assert_eq!(
        service_key("api.prod.svc").as_deref(),
        Some("prod/Service/api")
    );
    // A pod address, an external domain and a bare name name no service.
    assert_eq!(service_key("10-0-3-11.prod.pod.cluster.local"), None);
    assert_eq!(service_key("api.example.com"), None);
    assert_eq!(service_key("api"), None);
}

/// Globs match within a segment, as Prometheus's own matcher does.
#[test]
fn a_rule_glob_matches_within_one_segment() {
    assert!(glob_match("deploy/rules/*.yml", "deploy/rules/api.yml"));
    assert!(glob_match("deploy/rules/*.yml", "deploy/rules/.yml"));
    assert!(!glob_match(
        "deploy/rules/*.yml",
        "deploy/rules/sub/api.yml"
    ));
    assert!(!glob_match("deploy/rules/*.yml", "deploy/rules/api.yaml"));
    assert!(glob_match("deploy/alerts.yml", "deploy/alerts.yml"));
    assert!(segment_match("a*c?e", "abbbcde"));
    assert!(!segment_match("a*c?e", "abbbce"));
}

/// A pattern is resolved beside the configuration that declared it.
#[test]
fn a_rule_pattern_is_resolved_beside_its_config() {
    assert_eq!(
        join_path("deploy/prometheus.yml", "rules/*.yml"),
        "deploy/rules/*.yml"
    );
    assert_eq!(
        join_path("deploy/prometheus.yml", "./a.yml"),
        "deploy/a.yml"
    );
    assert_eq!(join_path("prometheus.yml", "a.yml"), "a.yml");
}

/// A YAML file that is neither is counted and named rather than half-read.
#[test]
fn a_file_that_is_not_prometheus_is_said_to_be() {
    let o = run("deploy/values.yaml", "replicaCount: 2\nimage:\n  tag: v1\n");
    assert_eq!(o.report.skipped, 1);
    assert!(o.nodes.is_empty());
    assert!(
        o.report
            .notes
            .iter()
            .any(|n| n.contains("not a Prometheus file")),
        "{:?}",
        o.report.notes
    );
}

/// Broken YAML is one file's problem, not the ingest's.
#[test]
fn invalid_yaml_is_counted_and_named() {
    let o = run(
        "deploy/prometheus.yml",
        "scrape_configs: [\n  - job_name: ' \n",
    );
    assert_eq!(o.report.skipped, 1);
    assert!(
        o.report.notes.iter().any(|n| n.contains("not valid YAML")),
        "{:?}",
        o.report.notes
    );
}

/// Two configurations in one tree describe two deployments; their jobs must
/// not collide.
#[test]
fn two_configurations_keep_their_jobs_apart() {
    let one = run("prod/prometheus.yml", CONFIG);
    let two = run("staging/prometheus.yml", CONFIG);
    assert!(has_node(&one, "prod/job/api"));
    assert!(has_node(&two, "staging/job/api"));
    // But a metric is a metric wherever it is written.
    let rules_one = run("prod/rules.yml", RULES);
    let rules_two = run("staging/rules.yml", RULES);
    assert!(has_node(&rules_one, "metric/http_requests_total"));
    assert!(has_node(&rules_two, "metric/http_requests_total"));
}
