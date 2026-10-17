# plugin: terraform

English · [简体中文](README_CN.md)

Reads **Terraform configuration** into graph facts: the resources a tree
declares, the modules it uses, the variables that configure them, and — the
part that matters — the dependencies between them. Manifest `terraform@1`,
claims `.tf` (and `.tfvars`/`.tfstate`, in order to decline them). Pure Rust:
`hcl-rs`, `rmp-serde` and the SDK.

The third reader of the ops graph (`arch/10-ops.md` in the main repository),
and the layer underneath the other two: [Compose](../compose) and
[Kubernetes](../k8s) describe what runs, Terraform describes what it runs
*on* — the cluster, the database, the load balancer, the DNS record.

## Where its graph lives: references

Terraform's dependency graph is not written down anywhere. It is **derived**
from references: `subnet_id = aws_subnet.main.id` is a dependency on
`aws_subnet.main`, and `terraform plan` works it out the same way.

So the edges here come from walking every expression for a traversal whose
root names something the configuration declares — a resource type, a data
source, a module, a variable. An explicit `depends_on` is recorded too, and
the edge says which it was (`via: reference` or `via: depends_on`), but it is
the *less* interesting half of the answer.

A reference naming something no file in the tree declares is **dropped, not
drawn**: it would point at a node nobody minted. The report says how many,
because the usual cause is a module's internals, which live in another
directory.

## Dispatch

`.tf` belongs to Terraform and to nothing else, so unlike the Kubernetes
reader this plugin claims its extension outright and a tree is read without
being told to.

`.tfvars` and `.tfstate` are claimed **in order to decline them**. The host
gives each file to exactly one reader with no fallback, so claiming an
extension is the only way to stop the built-in document reader taking it and
putting its contents into the prose — which for a `.tfvars` means a
credential in the graph. Not claiming them was a bug, found by digesting a
real tree and reading the report: *"no installed plugin claims .tfvars —
these files were read as plain text"*.

One gap remains and cannot be closed the same way:
`terraform.tfstate.backup` ends in `.backup`, which is far too generic an
extension to claim. Terraform's own recommended `.gitignore` excludes
`*.tfstate*`, and a tree that keeps state in version control has a larger
problem than this plugin.

## Nodes and edges

| Node | Key | What it is |
|---|---|---|
| `Namespace` | the directory | The root module — the unit `terraform apply` is run in. Terraform has no namespace of its own |
| `Infra` | `<type>.<name>` | A managed resource, keyed by Terraform's own address: `aws_instance.web`. The type is a property, because there are thousands of them |
| `DataSource` | `data.<type>.<name>` | Something read rather than managed |
| `Module` | `module.<name>` | A module block, with its `source` and `version` |
| `Variable` | `var.<name>` | An input. Its `default` is governed below |
| `Output` | `output.<name>` | What the module publishes |
| `Provider` | `provider.<name>` | A configured provider, with its `region` |

`Infra` rather than `Resource` on purpose: the Kubernetes reader already uses
`Resource` for a kind it does not model, and one label meaning both would
make every query over either one wrong.

| Edge | From → to | Carries |
|---|---|---|
| `IN_NAMESPACE` | anything → `Namespace` | — |
| `DEPENDS_ON` | `Infra` → `Infra` \| `DataSource` | `via` (`reference`, `depends_on`) |
| `USES_VARIABLE` | `Infra` \| `Module` → `Variable` | `via` |
| `USES_MODULE` | `Namespace` → `Module` | — |
| `PROVIDED_BY` | `Infra` → `Provider` | — |
| `EXPOSES` | `Output` → `Infra` | — |
| `USES_IMAGE` | `Infra` → `Image` | — |

## The join, which pays off twice here

A container image named in Terraform — an ECS task definition, a Cloud Run
service, a `docker_container` — mints **the same `Image` node** the Kubernetes
and Compose readers mint, under the same key, with the same `revision` parsed
by the same SDK code (`dr_strange_ext::ops`). So a revision-tagged image
reaches a commit from here exactly as it does from a pod template, and a
tree that deploys through Terraform is not a blind spot.

`image`, `image_uri` and `container_image` are read, at up to four levels of
nested block, because providers put them at different depths. An
*interpolated* image (`image = "${var.registry}/api:${var.tag}"`) is not an
image reference but a set of references, and is recorded as dependencies
instead.

## Sensitive, as the author declared it

A `variable` or `output` marked `sensitive = true` has its value **withheld**;
anything else is recorded. That is the same rule the Kubernetes reader applies
to a ConfigMap over a Secret, and for the same reason: the author said which
it was, so nothing has to be guessed. A withheld default reads `(sensitive)`
with a description saying why, rather than being silently absent.

## Known limits

- **Nothing is evaluated.** `count`, `for_each`, conditionals and functions
  are recorded as written, not resolved. `aws_instance.web` is one node
  whether `count = 1` or `count = 50`, because the configuration declares one
  resource block.
- **Interpolations inside a template string are not followed.**
  `"${aws_vpc.main.id}/24"` is a template, and `hcl` hands it over as a
  string; the reference inside it is therefore missed. Stated as a limit
  rather than guessed at with a regex over the text.
- **A module's contents are not read from its source.** A `module` block is a
  node with its source and version; what the module declares is in another
  directory or a registry, and nothing here fetches either.
- **`locals` are not nodes.** A local is a name for an expression, and a
  reference through one is dropped rather than followed to what it names.
- **One root module per directory.** A directory is the namespace, which is
  how `terraform apply` treats it; a tree with `infra/prod` and
  `infra/staging` gets two.
- **Declared state, not reality.** What the configuration says it builds, not
  what exists in the account. `arch/10-ops.md` §1 draws that line.

## Catalog entry

```json
{
  "name": "terraform",
  "version": "1.0.0",
  "claims": "Terraform configuration",
  "url": "https://github.com/wangyingsm/dr-strange-extension/releases/download/terraform-v1.0.0/terraform.wasm",
  "contract": "1.0.0",
  "min_drsg": "2.7.0"
}
```

## Build & test

```console
cargo test                                      # both phases, including across chunks
cargo build --release --target wasm32-wasip2    # or `just terraform-plugin`
```
