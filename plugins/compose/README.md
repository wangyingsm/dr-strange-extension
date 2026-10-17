# plugin: compose

English · [简体中文](README_CN.md)

Reads a **Docker Compose** file into graph facts: the services a project
declares, the images they run, and the config, volumes and networks they are
wired to. Manifest `compose@1`, claims **no file extension** — see
[Dispatch](#dispatch). Pure Rust: `yaml-rust2` and the SDK.

This is the first half of the **ops graph** — the model is specified in
`arch/10-ops.md` in the main repository, and this plugin implements the
Compose dialect of it. Compose before Kubernetes because it is the small
case: one file, no templating, and a service's identity is the key it is
written under.

## Dispatch

Routing everywhere else is by extension, because that is what the input is. A
Compose file is a **filename**: claiming `.yml` would take every Kubernetes
manifest, CI workflow and Helm chart in the tree from whatever reads them.

So this plugin declares no extensions, and the **host** routes the names it
knows — `compose.yaml`, `compose.yml`, `docker-compose.yaml`,
`docker-compose.yml`, and their `.override.` variants — to whatever is
installed under the name `compose`, the same statement it already makes
sending a build manifest to `deps`. With this plugin absent, those files are
read as prose exactly as before. Nothing is guessed.

A file the host routes here that has no `services:` block is **declined**,
counted as skipped, and named in the notes. A Kubernetes manifest that
happens to be called `compose.yml` is not half-parsed into something that
looks like a topology.

## Nodes and edges

| Node | Key | What it is |
|---|---|---|
| `Namespace` | `<project>` | The Compose project: its own `name:`, else the directory, else the file stem — what `docker compose` itself does. `name_source` says which |
| `Workload` | `<project>/service/<name>` | One service. `kind: service`, plus `ports`, `command` and `replicas` where written |
| `Container` | `<project>/service/<name>/<name>` | The service's container. Its own node so that `USES_IMAGE` and `MOUNTS` mean here what they will mean for a Kubernetes pod template with a sidecar in it |
| `Image` | the reference as written | What the container runs — and the join carrier, below |
| `ConfigMap` | `<project>/configmap/<file>` | An `env_file`, by path. Its contents are not read |
| `Volume` | `<project>/volume/<name>` | A named volume |
| `Network` | `<project>/network/<name>` | A declared or referenced network |

| Edge | From → to | Carries |
|---|---|---|
| `IN_NAMESPACE` | anything → `Namespace` | — |
| `RUNS` | `Workload` → `Container` | — |
| `USES_IMAGE` | `Container` → `Image` | — |
| `MOUNTS` | `Container` → `Volume` \| `ConfigMap` | `as` (`file`, `envFrom`), `path` for a file |
| `DEPENDS_ON` | `Workload` → `Workload` | `condition`, where the long form gives one |
| `ATTACHED_TO` | `Workload` → `Network` | — |

Scalars are kept **as written**. `${TAG:-latest}` stays that string, because
a tag resolved against an environment this parser cannot see is a fact about
nothing.

## Where the join holds, and where it does not

The thread from a running thing back to the commit that built it is the
image's **tag**, because that is where a build pipeline usually puts the
revision. A revision-shaped tag component becomes a `revision` property, and
a query joins it to a history plane (`arch/10-ops.md` §5 — a property and a
query, never an edge, because a cross-plane edge is rejected by the storage
layer).

- **A revision tag joins by construction.** `api:sha-9f2c1b7` and commit
  `9f2c1b7` are the same thing, because a pipeline put one in the other.
  `git-`, `rev-` and `sha256-` prefixes and a bare hex string are read the
  same way.
- **A floating tag joins to nothing.** `latest`, `stable`, `v2.1.0` name no
  revision. The image is recorded; `revision` is absent. An all-digit tag is
  a version far more often than a revision, so it is not treated as one.
- **A digest is exact and silent.** `api@sha256:…` pins the bytes and says
  nothing about provenance; it is recorded as `digest`, not as a revision.
- **A third-party image never joins, correctly.** `postgres:17` is not built
  from this repository and nothing should connect it to a commit here.
- **A `build:` service has no reference at all.** It is marked
  `source: build` with its `build_context`, because an image that does not
  exist yet cannot be named.

## Known limits

- **Environment values are not recorded, only their names.** A Compose
  `environment:` block is a ConfigMap and a Secret in one place with nothing
  distinguishing them, and `POSTGRES_PASSWORD` sits in them routinely.
  Recording the names answers "what is this service configured by" without
  copying credentials into a graph that gets exported and backed up. Where
  the ops model *does* record a value — a Kubernetes ConfigMap — it is
  because the author chose `ConfigMap` over `Secret` and thereby said it was
  not sensitive. Compose offers no such statement, so this plugin infers
  none.
- **An `env_file`'s contents are not read.** The path is a node; what is in
  it is not. Same reason.
- **A bind mount is not an entity.** `./local:/mnt/local` is a path on
  whoever's machine ran this, so it rides the container as `host_mounts`
  rather than becoming a `Volume` node.
- **No `extends`, no profiles, no interpolation.** A file that composes
  another through `extends` is read as itself. Profiles are ignored, so every
  service in the file is recorded whether or not a given profile would start
  it.
- **Multi-document YAML reads the first document only.** Compose does not use
  `---` separators; a file that does is not a Compose file.
- **Declared state, not runtime.** No containers, no health, no ports in use
  — none of that is in the file. See `arch/10-ops.md` §1.

## Catalog entry

```json
{
  "name": "compose",
  "version": "1.0.0",
  "claims": "Docker Compose files",
  "url": "https://github.com/wangyingsm/dr-strange-extension/releases/download/compose-v1.0.0/compose.wasm",
  "contract": "1.0.0",
  "min_drsg": "2.7.0"
}
```

## Build & test

```console
cargo test                                      # the model, and the limits above
cargo build --release --target wasm32-wasip2    # or `just compose-plugin`
```
