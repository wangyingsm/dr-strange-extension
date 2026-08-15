# plugin: go

English · [简体中文](README_CN.md)

Parses Go source into graph facts. Manifest `go@1`, claims `.go`. Built on
**Go's own `go/parser` and `go/ast`** — the canonical frontend — compiled to
a component with TinyGo. Written in Go on purpose: it proves the contract is
language-neutral better than a second Rust plugin ever could.

## Layout

```
parser/     the language logic, plain Go, 29 native tests (go test — no TinyGo)
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

| Label | Emitted for | Props beyond `doc_comment` / `visibility` / `file` / `line` |
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

## Edges

| Type | Meaning | `line` |
|---|---|---|
| `CONTAINS` | package → decl, receiver type → method | declaration site |
| `HAS_METHOD` | interface → its demanded methods | member's line |
| `CALLS` | function → callee | call site |
| `IMPORTS` | package → package (in-tree or external) | import statement |
| `IMPLEMENTS` | type → interface — **no line**, deliberately: satisfaction is structural in Go; nothing is written anywhere |

## Resolution — the certainty line

- An **unqualified call** binds to a function another file of the same
  package declares (assemble's whole reason to exist); `pkg.Type(x)`
  conversions are recognised and are not calls; builtins are nobody's edge.
- A **qualified call** binds through the file's own import table — aliases
  respected, the tree's real package names beating directory names.
- A **method call on a value** names no package, and the receiver's type is
  what a parser cannot know: counted.
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

## Options (`[plugins.go]`)

| Key | Effect |
|---|---|
| `include_source = "true"` | attach `_code` to declarations |

## Build & test

```console
$ cd parser && go test ./...          # 29 tests, plain Go
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
