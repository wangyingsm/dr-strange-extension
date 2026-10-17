use super::*;

/// Both phases, the way the host runs them: `read` fills a chunk, `resolve`
/// is the whole-tree pass. A test that only called `read` would miss every
/// cross-file answer, which is the half this plugin exists for.
fn run(path: &str, text: &str) -> Output {
    let mut chunk = Chunk::default();
    read(path, text, &mut chunk);
    resolve(chunk)
}

/// Two chunks, as the host would split two files — the case that broke when
/// this plugin derived its partial instead of owning it.
fn run_chunks(files: &[(&str, &str)]) -> Output {
    let mut all = Chunk::default();
    for (path, text) in files {
        let mut chunk = Chunk::default();
        read(path, text, &mut chunk);
        // Serialized and merged, exactly as `assemble` receives it.
        let bytes = rmp_serde::to_vec(&chunk).unwrap();
        let back: Chunk = rmp_serde::from_slice(&bytes).unwrap();
        all.nodes.extend(back.nodes);
        all.edges.extend(back.edges);
        all.workloads.extend(back.workloads);
        all.selectors.extend(back.selectors);
        all.namespaces.extend(back.namespaces);
        all.skipped += back.skipped;
        all.notes.extend(back.notes);
    }
    resolve(all)
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
    at.as_str()
        .or_else(|| at["$value"].as_str())
        .unwrap_or_else(|| panic!("no prop {name} on {}: {v:?}", n.key))
        .to_string()
}

/// A described property whose value is a list — `data`, `selector`, `keys`.
fn list(n: &dr_strange_ext::Node, name: &str) -> Vec<String> {
    let v = props(n);
    v[name]["$value"]
        .as_array()
        .unwrap_or_else(|| panic!("no list prop {name} on {}: {v:?}", n.key))
        .iter()
        .filter_map(|x| x.as_str().map(str::to_string))
        .collect()
}

fn has_edge(o: &Output, src: &str, ty: &str, dst: &str) -> bool {
    o.edges
        .iter()
        .any(|e| e.src == src && e.type_ == ty && e.dst == dst)
}

const MANIFEST: &str = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: api
  namespace: prod
spec:
  replicas: 3
  template:
    metadata:
      labels:
        app: api
        tier: web
    spec:
      volumes:
        - name: cfg
          configMap:
            name: api-config
        - name: creds
          secret:
            secretName: api-secret
      containers:
        - name: api
          image: ghcr.io/acme/api:sha-9f2c1b7
          env:
            - name: LOG_LEVEL
              value: debug
            - name: TOKEN
              valueFrom:
                secretKeyRef:
                  name: api-secret
                  key: token
          envFrom:
            - configMapRef:
                name: api-config
          volumeMounts:
            - name: cfg
              mountPath: /etc/api
---
apiVersion: v1
kind: Service
metadata:
  name: api
  namespace: prod
spec:
  type: ClusterIP
  selector:
    app: api
  ports:
    - port: 80
      targetPort: 8080
---
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: public
  namespace: prod
spec:
  rules:
    - host: shop.example.com
      http:
        paths:
          - path: /api
            backend:
              service:
                name: api
                port:
                  number: 80
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: api-config
  namespace: prod
data:
  LOG_FORMAT: json
---
apiVersion: v1
kind: Secret
metadata:
  name: api-secret
  namespace: prod
type: Opaque
data:
  token: aHVudGVyMg==
"#;

/// Every object in a multi-document manifest, and the edges between them.
#[test]
fn a_multi_document_manifest_becomes_a_topology() {
    let o = run("k8s/all.yaml", MANIFEST);

    assert_eq!(
        prop(node_at(&o, "prod/Deployment/api"), "kind"),
        "Deployment"
    );
    assert_eq!(prop(node_at(&o, "prod/Deployment/api"), "replicas"), "3");
    assert!(has_edge(&o, "prod/Deployment/api", "IN_NAMESPACE", "prod"));
    assert!(has_edge(
        &o,
        "prod/Deployment/api",
        "RUNS",
        "prod/Deployment/api/api"
    ));
    assert!(has_edge(
        &o,
        "prod/Deployment/api/api",
        "USES_IMAGE",
        "ghcr.io/acme/api:sha-9f2c1b7"
    ));
    // One namespace node, however many objects name it.
    assert_eq!(
        o.nodes.iter().filter(|n| n.key == "prod").count(),
        1,
        "the namespace was minted more than once"
    );
}

/// The one edge with real work behind it: a selector matched against a pod
/// template's labels, across documents.
#[test]
fn a_selector_matches_the_pod_template_it_names() {
    let o = run("k8s/all.yaml", MANIFEST);
    assert!(has_edge(
        &o,
        "prod/Service/api",
        "SELECTS",
        "prod/Deployment/api"
    ));

    // A selector naming labels nothing carries leaves no edge, and says so.
    let o = run(
        "k8s/x.yaml",
        "apiVersion: v1\nkind: Service\nmetadata:\n  name: ghost\nspec:\n  selector:\n    app: nowhere\n",
    );
    assert!(!o.edges.iter().any(|e| e.type_ == "SELECTS"));
    assert_eq!(
        list(node_at(&o, "default/Service/ghost"), "selector"),
        ["app=nowhere"]
    );
    assert!(
        o.report
            .notes
            .iter()
            .any(|n| n.contains("matched no workload")),
        "{:?}",
        o.report.notes
    );

    // A selector must match *every* label it names, not merely one.
    let partial = r#"
apiVersion: apps/v1
kind: Deployment
metadata: {name: w}
spec:
  template:
    metadata:
      labels: {app: api}
    spec:
      containers: [{name: c, image: alpine}]
---
apiVersion: v1
kind: Service
metadata: {name: s}
spec:
  selector: {app: api, tier: web}
"#;
    let o = run("k8s/p.yaml", partial);
    assert!(
        !o.edges.iter().any(|e| e.type_ == "SELECTS"),
        "a partial label match became an edge"
    );
}

/// The selector must match across *chunks*, not merely across documents in
/// one file. The host splits a tree into chunks, and a Service in one with
/// its Deployment in another is the ordinary case — so the answer cannot
/// live in a per-chunk accumulator.
#[test]
fn a_selector_matches_a_workload_in_another_chunk() {
    let o = run_chunks(&[
        (
            "k8s/deploy.yaml",
            "apiVersion: apps/v1\nkind: Deployment\nmetadata: {name: api, namespace: prod}\nspec:\n  template:\n    metadata:\n      labels: {app: api}\n    spec:\n      containers: [{name: api, image: alpine}]\n",
        ),
        (
            "k8s/svc.yaml",
            "apiVersion: v1\nkind: Service\nmetadata: {name: api, namespace: prod}\nspec:\n  selector: {app: api}\n",
        ),
    ]);
    assert!(
        has_edge(&o, "prod/Service/api", "SELECTS", "prod/Deployment/api"),
        "edges: {:?}",
        o.edges
            .iter()
            .map(|e| format!("{} -{}-> {}", e.src, e.type_, e.dst))
            .collect::<Vec<_>>()
    );
    // And the namespace both chunks named is still one node.
    assert_eq!(o.nodes.iter().filter(|n| n.key == "prod").count(), 1);
}

/// A mount reaches what backs the volume, not the volume's local name.
#[test]
fn a_mount_resolves_through_the_pods_volume_list() {
    let o = run("k8s/all.yaml", MANIFEST);
    assert!(has_edge(
        &o,
        "prod/Deployment/api/api",
        "MOUNTS",
        "prod/ConfigMap/api-config"
    ));
    // envFrom and a single keyed env reference are both mounts.
    let via: Vec<&str> = o
        .edges
        .iter()
        .filter(|e| e.type_ == "MOUNTS" && e.dst == "prod/Secret/api-secret")
        .map(|e| e.src.as_str())
        .collect();
    assert!(!via.is_empty(), "the keyed secret reference made no edge");
}

/// A ConfigMap's values are recorded because the author chose `ConfigMap`; a
/// Secret's are never recorded, in any encoding.
#[test]
fn a_secrets_values_never_enter_the_graph() {
    let o = run("k8s/all.yaml", MANIFEST);

    assert_eq!(
        list(node_at(&o, "prod/ConfigMap/api-config"), "data"),
        ["LOG_FORMAT=json"]
    );

    let s = node_at(&o, "prod/Secret/api-secret");
    assert_eq!(prop(s, "type"), "Opaque");
    assert_eq!(list(s, "keys"), ["token"]);
    // The base64 of "hunter2" is in the manifest and must be nowhere here.
    let everything = format!("{:?}{:?}", o.nodes, o.edges);
    assert!(
        !everything.contains("aHVudGVyMg"),
        "a Secret's value was recorded"
    );
}

/// A CronJob's pod template sits one level deeper than every other
/// workload's, which is the kind of thing that silently records nothing.
#[test]
fn a_cronjobs_template_is_found_one_level_deeper() {
    let o = run(
        "k8s/cron.yaml",
        "apiVersion: batch/v1\nkind: CronJob\nmetadata: {name: nightly}\nspec:\n  schedule: \"0 2 * * *\"\n  jobTemplate:\n    spec:\n      template:\n        spec:\n          containers:\n            - name: job\n              image: ghcr.io/acme/batch:git-abc1234\n",
    );
    assert_eq!(
        prop(node_at(&o, "default/CronJob/nightly"), "schedule"),
        "0 2 * * *"
    );
    assert!(has_edge(
        &o,
        "default/CronJob/nightly/job",
        "USES_IMAGE",
        "ghcr.io/acme/batch:git-abc1234"
    ));
    assert_eq!(
        prop(node_at(&o, "ghcr.io/acme/batch:git-abc1234"), "revision"),
        "abc1234"
    );
}

/// An Ingress carries where it routes, not merely that it routes.
#[test]
fn an_ingress_rule_carries_its_host_and_path() {
    let o = run("k8s/all.yaml", MANIFEST);
    let e = o
        .edges
        .iter()
        .find(|e| e.type_ == "ROUTES_TO")
        .expect("no ROUTES_TO edge");
    assert_eq!(e.src, "prod/Ingress/public");
    assert_eq!(e.dst, "prod/Service/api");
    let p: Value = e.properties.parse().unwrap();
    assert_eq!(p["host"].as_str(), Some("shop.example.com"));
    assert_eq!(p["path"].as_str(), Some("/api"));
    assert_eq!(p["port"].as_str(), Some("80"));
}

/// A YAML file that is not a manifest is declined and named, rather than
/// half-read into something that looks like a topology.
#[test]
fn a_file_that_is_not_a_manifest_is_declined() {
    for text in [
        "name: my-chart\nversion: 1.0.0\n",
        "jobs:\n  build:\n    runs-on: ubuntu-latest\n",
        "services:\n  api:\n    image: nginx\n",
    ] {
        let o = run("values.yaml", text);
        assert!(o.nodes.is_empty(), "{:?} from {text:?}", o.nodes);
        assert!(
            o.report
                .notes
                .iter()
                .any(|n| n.contains("not a Kubernetes manifest")),
            "{:?}",
            o.report.notes
        );
    }
}

/// A kind this plugin does not model is still recorded, so a reader can see
/// something was there.
#[test]
fn an_unmodelled_kind_is_recorded_not_dropped() {
    let o = run(
        "k8s/hpa.yaml",
        "apiVersion: autoscaling/v2\nkind: HorizontalPodAutoscaler\nmetadata:\n  name: api\n  namespace: prod\n",
    );
    let n = node_at(&o, "prod/HorizontalPodAutoscaler/api");
    assert_eq!(n.label, "Resource");
    assert_eq!(prop(n, "kind"), "HorizontalPodAutoscaler");
}
