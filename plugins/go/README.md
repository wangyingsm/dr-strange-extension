# plugin: go

English · [简体中文](README_CN.md)

Parses Go source into graph facts. Manifest `go@2`, claims `.go`. Built on
**Go's own `go/parser` and `go/ast`** — the canonical frontend — compiled to
a component with TinyGo. Written in Go on purpose: it proves the contract is
language-neutral better than a second Rust plugin ever could.

## Layout

```
parser/     the language logic, plain Go, 45 native tests (go test — no TinyGo)
component/  the TinyGo wrapper over sdk/go, plus the wit/ build package
```

## Keys

Go's own qualified names, which is what makes them stable across subtrees
and ingests:

```
example.com/demo                      a package (from go.mod's module line)
example.com/demo/sub.Do               a function
example.com/demo/sub.Counter.Add      a method — path.Type.Method
```

The module path comes from the **nearest `go.mod`** above the file (nested
modules start their own, the way workspace crates do), read through the
host; with no manifest anywhere, the host's label names the tree.

## Nodes

| Label | Emitted for | Props beyond `doc_comment` / `visibility` / `file` / `line` / `end_line` |
|---|---|---|
| `Package` | one per package | `name`, `imports` (union across the package's files, sorted); doc from `doc.go` merges in. **No file/line** — a package spans files, and a single pick would be arbitrary |
| `Function` / `Method` | declarations (`Method` hangs off its receiver type) | `signature` (receiver included, as written), `is_async` never (not a Go thing) |
| `Struct` | type declarations | `fields`: described list of `name: type` in declaration order — Go has no visibility keyword to prepend; the capitalization *is* the visibility and it is already in the name |
| `Interface` | interface declarations | its demanded methods become `Method` nodes reached by `HAS_METHOD` (no visibility: as public as the interface) |
| `Type` / `TypeAlias` | `type X Y` / `type X = Y` | underlying type under `signature` |
| `Const` / `Var` | value specs | type under `signature`, initializer under `value` as written; an **iota ladder** repeats the previous spec's expression — the language's own rule, recorded, never evaluated |
| stand-ins | foreign packages and their members | `Package` / `Function` + `External`, no props |

`visibility: "exported"` follows Go's rule: the capital letter. `init` is
deliberately absent — every `init` in a package shares one name, so as a
node it could only be a key collision, and its calls are wiring, not API.

Every declaration also carries `end_line`: where it stops, so `snippet` reads
exactly the declaration instead of a fixed number of lines after its first,
and a reader sees whether a thing is six lines or two hundred without opening
it. `line` still points at the **name**, so documentation above a declaration
cannot move where the graph says it starts.

## Edges

| Type | Meaning | `line` |
|---|---|---|
| `CONTAINS` | package → decl, receiver type → method | declaration site |
| `HAS_METHOD` | interface → its demanded methods | member's line |
| `CALLS` | function → callee. A `go f()` carries `concurrent: go` on the edge: control does reach the callee, but on another goroutine, and a graph that cannot tell the two apart describes a different program | call site |
| `IMPORTS` | package → package (in-tree or external) | import statement |
| `IMPLEMENTS` | type → interface — **no line**, deliberately: satisfaction is structural in Go; nothing is written anywhere |
| `SENDS` | function → a channel it writes to (`ch <- v`) | the send |
| `RECEIVES` | function → a channel it reads from (`<-ch`, `range ch`, a `select` case) | the receive |
| `USES_TYPE` | declaration → a type it is **typed by**, `role` on the edge (`field`, `param`, `return`) and the union when one pair is written twice. Walked through slices, maps, channels and generic arguments — where [typeRef] stops, because a `[]Job` depends on `Job` even though its methods are not `Job`'s. In-tree types only | — |

## Routes

An HTTP route is the one reference this plugin cannot turn into an edge: the
handler or the caller is in another language, often another repository, and
between the two sides there is only a string. So the plugin records what the
source *states* and leaves the match to drsg's `wire`, which computes it per
call and never stores it.

On the symbol that already exists:

| Property | Set on | Shape |
|---|---|---|
| `serves` | the handler a registration call names | `GET /api/orders/{}` — the verb, and the path with every variable segment reduced to `{}` |
| `requests` | the function making the call | the same shape; `*` where the source states no verb |
| `serves_as_written` / `requests_as_written` | both | the path exactly as the source wrote it, before normalising |

**Declarations** are registration calls, which is Go's only idiom — there are
no annotations to read. `r.GET("/orders/:id", Detail)` records on `Detail`, and
the verb comes from the callee's own name, so gin, echo, and
`http.HandleFunc`'s verbless form all land through the same rule rather than a
framework table. A registration naming a handler in another package records
there, across files.

**A group's prefix reaches the routes on it.** `g := r.Group("/api")` gives a
router a prefix that belongs to every route registered on `g` and to none of
them yet, so the function doing the grouping serves nothing — and a handler
registered on `r` instead of `g` keeps its path whole.

**Requests** are any call with a path-shaped string argument, whatever the
callee is called — `http.Get`, a `client.Do` with a literal URL, and a
project-local wrapper alike. An absolute URL or a query string reduces to the
path.

## Resolution — the certainty line

- An **unqualified call** binds to a function another file of the same
  package declares (assemble's whole reason to exist); `pkg.Type(x)`
  conversions are recognised and are not calls; builtins are nobody's edge.
- A **qualified call** binds through the file's own import table — aliases
  respected, the tree's real package names beating directory names.
- A **method call on a value** names no package, and the receiver's type is
  what a parser cannot know: counted.
- A **channel** is the one value whose purpose is to join code that never
  calls itself, so it becomes a node of its own — keyed under whatever made
  it (`pkg.Producer.ch`, or `pkg.ch` for a package-level var, which the
  channel adopts rather than doubling) and carrying its element type and
  buffer size. `ch <- v` and `<-ch` are `SENDS`/`RECEIVES`; `range x` says
  nothing about being a channel, so it is kept only when the name is one.
  A channel **handed to a function** binds to that function's parameter by
  position, transitively and once per call site — which is what puts a
  producer and a consumer two hops apart instead of leaving them unrelated.
  Only a **bare name** is followed: `s.ch <- v` names a field, and which
  channel that is depends on which `s` — a question this parser cannot
  answer, and answering it wrong would join two goroutines that never meet.
- **Interface satisfaction** is decided structurally, under certainty rules:
  textual signature equality within a package; across packages only when
  both signatures are spelled entirely in predeclared types (a local `Thing`
  spells the same in two packages and means two different things — text
  stops being identity, so the comparison is refused). An interface
  embedding anything the tree does not declare is left unmatched and
  counted — a half-checked satisfaction would be a guess wearing an edge's
  clothes. Receiver pointer-ness is ignored: the edge claims the pointer
  method set.
- A method whose **receiver type sits in a file the run never saw** (a
  build-tag variant, a split subtree) implies a bare `Type` node rather
  than an edge into nothing.
- Same name in two files of one package = **build-tag variants**: first
  seen kept, counted.

## Test code

Test code answers "who calls this" differently from production code, and a
reader that cannot tell them apart counts a test as a user. Two properties
say so: `test_flag` carries **what the evidence was**, and
`_test_flag_confidence` carries **how much that evidence is worth** — the `_`
keeps the second out of the compact renderers and out of embeddings, where it
would be noise; `cypher`, `get_node` and `export` still return it, which is
where a reader weighing the flag is looking. Nothing is flagged when nothing
is written down.

| `test_flag` | `_test_flag_confidence` | From |
|---|---|---|
| `build-rule` | `definitive` | the file's name ends `_test.go` |

Not a convention: the go tool compiles `_test.go` only under `go test`, so
nothing such a file declares can be reached by a production binary — which is
the stronger claim, and the one that makes "is this still used" answerable.
The package is the exception, as it is for `file`: a package holding one test
file is not a test package, and flagging it would say it was.

## Options (`[plugins.go]`)

| Key | Effect |
|---|---|
| `include_source = "true"` | attach `_code` to declarations |

## Build & test

```console
$ cd parser && go test ./...          # 45 tests, plain Go
$ just go-plugin                      # → component/go.wasm
$ drsg plugin install component/go.wasm
```

The TinyGo flags (`-scheduler=none -gc=leaking`) are load-bearing — see
[`sdk/go`](../../sdk/go)'s README and the justfile comment for why, and for
the copy-before-use rule every wasmexport boundary obeys.

## Known limits

Methods promoted from embedded structs are not walked onto the outer type;
generic instantiations are not tracked (a call binds to the declaration);
`init` bodies are uncounted by design.

Routes: gorilla/mux writes the verb in a chained `.Methods("GET")` rather than
in the callee's name, so such a route records `*` and matches every verb on
its path. chi's `Route(path, fn)` is a mount, not a route, but `route` is
deliberately not a mount word — axum's `.route` *is* a route — so chi's prefix
does not reach the handlers under it. A router handed to another function to
register on loses its prefix.
