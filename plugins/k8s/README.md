# plugin: k8s

English · [简体中文](README_CN.md)

Reads **Kubernetes manifests** into graph facts: the workloads a tree
declares, the images they run, the services that select them, the routes that
reach those, and the config they mount. Manifest `k8s@1`, claims **no file
extension** and is routed by an explicit handler — see
[Dispatch](#dispatch). Pure Rust: `yaml-rust2`, `rmp-serde` and the SDK.

The second half of the ops graph; the model is specified in `arch/10-ops.md`
in the main repository, and the [Compose plugin](../compose) reads the small
dialect of the same model. Kubernetes is the real one: objects spread across
files, a workload's pods named by *label selector* rather than by key, and
config reached through a pod's volume list rather than named directly.

## Dispatch

This plugin claims no extension, and unlike the Compose reader it cannot be
routed by filename either — a manifest may be called anything.

Claiming `.yaml` was considered and **rejected**: the host buckets every file
to exactly one reader with no fallback, so claiming the extension would take
every `values.yaml`, CI workflow and chart in the tree from the document
reader and stop them being read at all. A plugin that claims a format should
be able to read it.

So a manifest tree is digested explicitly:

```console
drsg digest ./k8s --handler k8s
helm template ./chart > rendered.yaml && drsg digest rendered.yaml --handler k8s
kustomize build ./overlays/prod > rendered.yaml && drsg digest rendered.yaml --handler k8s
```

## Two phases, because the answer spans files

Most plugins implement the SDK's `Simple` facade, which runs once per chunk
and concatenates the results. This one cannot: a `Service` and the
`Deployment` it selects are routinely in different files, and the host is
free to put them in different chunks. A per-chunk accumulator finds nothing,
and — worse — mints the namespace once per chunk.

So it implements `Guest` itself and owns its partial: `parse` records what a
chunk settles plus the facts resolution needs, and `assemble` does the
whole-tree pass — one node per namespace, and every selector matched against
every workload the tree holds.

## Nodes and edges

| Node | Key | What it is |
|---|---|---|
| `Namespace` | `<ns>` | `metadata.namespace`, or `default`. One node however many objects name it |
| `Workload` | `<ns>/<kind>/<name>` | `Deployment`, `StatefulSet`, `DaemonSet`, `Job`, `CronJob`, `ReplicaSet`, `ReplicationController`, `Pod`. `kind` on the node, plus `replicas`, `schedule` and the `pod_labels` a selector matches |
| `Container` | `<workload>/<name>` | One container, with `role` (`container` or `init`) |
| `Image` | the reference as written | What it runs — and the join carrier, below |
| `Service` | `<ns>/Service/<name>` | With its `type`, `ports` and the `selector` it asked for |
| `Ingress` | `<ns>/Ingress/<name>` | External entry |
| `ConfigMap` | `<ns>/ConfigMap/<name>` | Keys **and values** — see below |
| `Secret` | `<ns>/Secret/<name>` | Key names and `type` only, never values |
| `Volume` | `<ns>/Volume/<name>` | A `PersistentVolumeClaim`, with `storage` and `storage_class` |
| `Resource` | `<ns>/<kind>/<name>` | Any other kind: recorded so a reader sees something was there, not pretended to be understood |

| Edge | From → to | Carries |
|---|---|---|
| `IN_NAMESPACE` | anything → `Namespace` | — |
| `RUNS` | `Workload` → `Container` | — |
| `USES_IMAGE` | `Container` → `Image` | — |
| `SELECTS` | `Service` → `Workload` | `matched_by` |
| `ROUTES_TO` | `Ingress` → `Service` | `host`, `path`, `port` |
| `MOUNTS` | `Container` → `ConfigMap` \| `Secret` \| `Volume` | `as` (`file`, `envFrom`, `env`), `volume`, `path`, `key` |

`SELECTS` is the one edge with work behind it. A Service names pods by label,
and pods come from a workload's pod template — so the edge is
`spec.selector` matched against `spec.template.metadata.labels`, within one
namespace, and **every** label in the selector must match. A selector that
matches nothing in the tree leaves no edge; the selector stays on the node and
the report says how many went unmatched, because what it names may simply
live elsewhere.

A `MOUNTS` edge resolves through the pod's `volumes` list: a container's
`volumeMounts` names a *local* volume name, and the edge points at whatever
backs it — the ConfigMap, Secret or claim. `envFrom` and a single
`valueFrom.configMapKeyRef` are mounts too, with `as` saying which.

A CronJob's pod template sits one level deeper than every other workload's
(`spec.jobTemplate.spec.template`), which is the kind of difference that
silently records nothing; there is a test for it.

## Where the join holds, and where it does not

The thread back to the commit that built something is the image **tag**,
parsed by the SDK (`dr_strange_ext::ops`) so that this plugin and the Compose
one cannot disagree — the same reference is one node under one key, and two
readers writing different `revision` values would leave whichever wrote last.

A revision-shaped tag joins by construction (`sha-9f2c1b7` → commit
`9f2c1b7`); `latest`, `stable` and a version number join to nothing; a digest
is exact and silent about provenance; a third-party image never joins, which
is correct. The full table is in [the Compose plugin's
README](../compose/README.md#where-the-join-holds-and-where-it-does-not) and
in `arch/10-ops.md` §5.

## Config and secrets

**A ConfigMap's values are recorded.** Choosing `ConfigMap` over `Secret` is
the author stating the contents are not sensitive, and a query that can see
them answers most config questions. The consequence is worth stating: those
values land in the plane and therefore in every backup and export of it, and
a ConfigMap in a real repository sometimes carries what should have been a
Secret. `binaryData` is recorded by key name only.

**A Secret's values are never recorded**, in any encoding. The node carries
the key names and the `type`. base64 is not encryption, and a plugin that
read it would put real credentials into a graph that gets copied around.

A container's `env` is recorded by **name only**, for the same reason the
Compose reader does it: an inline `value:` is as likely to be a password as a
log level, and nothing in the manifest says which.

## Known limits

- **Declared state, not runtime.** No live cluster, no Pods that exist, no
  node assignments, no metrics or alerts. Those arrive with the Prometheus and
  OpenTelemetry work; `arch/10-ops.md` §1 draws the line and §3 reserves the
  labels.
- **Nothing is rendered.** A Helm chart or a Kustomize overlay must be
  rendered first — `helm template`, `kustomize build` — and the rendered YAML
  digested. Running either is not a parser's job.
- **A document needs `apiVersion` *and* `kind`.** Anything else is declined
  and named, so a `values.yaml` routed here is not half-read into a shape that
  looks like a topology.
- **`SELECTS` is label matching, not endpoint truth.** A Service whose
  selector matches two workloads gets two edges, which is what the cluster
  would do. `matchLabels` under a workload's own `spec.selector` is not used:
  the pod template's labels are what a Service actually matches.
- **Cross-namespace references are not followed.** An Ingress naming a
  service in another namespace points at a key in its own, because that is
  what the manifest says.

## Catalog entry

```json
{
  "name": "k8s",
  "version": "1.1.0",
  "claims": "Kubernetes manifests",
  "url": "https://github.com/wangyingsm/dr-strange-extension/releases/download/k8s-v1.1.0/k8s.wasm",
  "contract": "1.0.0",
  "min_drsg": "2.7.0"
}
```

## Build & test

```console
cargo test                                      # both phases, including across chunks
cargo build --release --target wasm32-wasip2    # or `just k8s-plugin`
```
