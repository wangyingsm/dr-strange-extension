# plugin: prometheus

English · [简体中文](README_CN.md)

Reads a **Prometheus** configuration into graph facts: the jobs it scrapes,
the addresses they name, the alerting rules it loads, and the metrics those
rules watch. Manifest `prometheus@1`, claims **no file extension** — see
[Dispatch](#dispatch). Pure Rust: `yaml-rust2` and the SDK.

This is the third ops reader after Compose and Kubernetes — the model is
specified in `arch/10-ops.md` in the main repository — and the first whose
subject is not a thing that runs but a thing that *watches* what runs. A
scrape job names an address; a `Service` node already exists at that address;
the edge between them is the point of this plugin.

## Dispatch

Routing everywhere else is by extension, because that is what the input is. A
Prometheus configuration is a **filename**: claiming `.yml` would take every
Kubernetes manifest, CI workflow and Helm chart in the tree from whatever
reads them.

So this plugin declares no extensions, and the **host** routes
`prometheus.yml` and `prometheus.yaml` to whatever is installed under the name
`prometheus`, the same statement it already makes sending a Compose file to
`compose`. With this plugin absent, those files are read as prose exactly as
before. Nothing is guessed.

A file routed here that declares none of `scrape_configs`, `rule_files` or
`groups` is **declined**, counted as skipped, and named in the notes.

**Rule files are followed, not guessed.** A rule file has no fixed name, so no
filename routing can claim it. The configuration declares its own under
`rule_files:`, and this plugin resolves those globs against the tree the host
exposes. A rules tree with no configuration beside it can be read by pointing
the host at it explicitly: `--handler prometheus`.

## Nodes and edges

| Node | Key | What it is |
|---|---|---|
| `ScrapeJob` | `<config>/job/<name>` | One `scrape_configs` entry. `metrics_path`, `scheme`, the intervals, `auth`, `discovery` and `relabelled` where they are written |
| `ScrapeTarget` | `<config>/job/<name>/target/<address>` | One declared address, split into `host` and `port`, plus `names` where the address is a key. Its own node because it carries both the join and — where nothing answers to it — the fact that there was none |
| `Alert` | `<config>/alert/<group>/<name>` | One alerting rule: `expr` as written, plus `for`, `severity` and `summary` where given |
| `Metric` | `metric/<name>` | A metric a rule names. Keyed globally on purpose: a metric name means the same thing wherever it is written, so two configurations watching one metric meet on it. A recording rule's own metric carries `recorded: true` and its `expr` |

| Edge | From → to | Carries |
|---|---|---|
| `HAS_TARGET` | `ScrapeJob` → `ScrapeTarget` | — |
| `SCRAPES` | `ScrapeTarget` → `Service` \| `Workload` | Drawn by the **host**, not here: `matched_by` (`dns` from the target's `names`, or `name` from a name match), `confidence` |
| `ALERTS_ON` | `Alert` → `Metric` \| `ScrapeJob` | — |
| `DERIVES_FROM` | `Metric` → `Metric` | A recording rule's metric to the metrics it is computed over |
| `IN_NAMESPACE` | `ScrapeJob` → `Namespace` | Where a Kubernetes SD config names the namespaces it looks in |

The `<config>` prefix is the directory holding the file, else its stem — the
rule the Compose reader uses for projects, and for the same reason: two
configurations in one repository describe two deployments, and their jobs must
not collide. Rules followed from a configuration inherit *its* prefix, so an
alert can reach the job it watches.

## Where the join holds, and where it does not

One address form names a service and no other: Kubernetes cluster DNS.
`api.prod.svc.cluster.local` is `prod/Service/api` — the key the Kubernetes
reader mints, derived rather than guessed. The target records it as `names`,
and the **host** draws the edge, with `matched_by: dns` and
`confidence: exact`.

The edge is the host's rather than this plugin's for a reason found by running
it: whether the tree *declares* that service is not something a parser holding
one file can know, and an edge onto a key nothing mints fails the whole digest
(`bulk edge endpoint … not found`). So the reader states what the address
names, and the host — which can see the plane — decides whether anything
answers to it. Where nothing does, the target is simply unmatched.

A bare `api:9090` — how a Compose stack and a short-name Kubernetes target are
both written — names no namespace, and so no key. **This plugin draws nothing
for it.** The host can see every ops node in the plane and match on the name;
what it finds is marked `matched_by: name` with a lower confidence, and where
two services answer to one name it lists the candidates instead of choosing.
Guessing a key here would produce an edge indistinguishable from the certain
one, which is the one thing the ops model will not do.

An IP address, an external domain, and a target a `relabel_configs` block
rewrites are all left unjoined for the same reason. The `ScrapeTarget` node is
still there, with the address as written, so what did not match is visible
rather than absent.

## Known limits

- **Metric names are read lexically, not parsed.** An identifier counts when
  it is not a function call, a keyword, a grouping label, a duration, or
  inside a label matcher or a string. A name assembled at query time —
  `{__name__=~"http_.*"}` — is not a selection this can see.
- **Only an exact `job="…"` matcher names a job.** `job=~"api.*"` is a pattern
  over jobs that may not exist, and a pattern is not a name.
- **Discovered targets are not targets.** `kubernetes_sd_configs` and its
  siblings produce their target set at run time. The mechanism is recorded on
  the job as `discovery`, and a job that declares no static target says so in
  the notes rather than appearing to scrape nothing.
- **No credential is recorded** — not a password, a bearer token, a
  credentials file or a TLS key. Which authentication a job uses is recorded,
  as `auth: basic`, because that is a fact about the topology and the secret
  is not.
- **A rule file edited alone** — with its configuration untouched — is re-read
  at the next full digest rather than by an incremental fold, because a fold
  re-parses the files that changed and that file is claimed by no plugin.
- **`--code-only` hides rule files from this plugin.** That flag leaves every
  file no handler claims out of the tree the host exposes, and a rule file is
  exactly that — so the glob matches nothing and the notes say so. Measured on
  the same tree: 16 facts in an ordinary digest, 10 with `--code-only`.
- **Alertmanager's own configuration is out of scope.** Where an alert *goes*
  is routing, not topology.
- **Declared state, not runtime.** No series, no active alerts, no scrape
  health. See `arch/10-ops.md` §1.

## Catalog entry

```json
{
  "name": "prometheus",
  "version": "1.0.0",
  "claims": "Prometheus configurations and rule files",
  "url": "https://github.com/wangyingsm/dr-strange-extension/releases/download/prometheus-v1.0.0/prometheus.wasm",
  "contract": "1.0.0",
  "min_drsg": "2.31.0"
}
```

`min_drsg` is the release that routes `prometheus.yml` by name and sends these
facts to the ops plane. An older host leaves the file to whatever read it
before.

## Build & test

```console
cargo test                                      # the model, and the limits above
cargo build --release --target wasm32-wasip2    # or `just prometheus-plugin`
```
