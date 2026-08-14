# sdk/go — the Go SDK

The one module a Go plugin author takes:

```console
$ go get github.com/wangyingsm/dr-strange-extension/sdk/go
```

It carries the `wit-bindgen-go`-generated bindings for the
`drsg:preprocess` contract under `bindings/`, and the `ext` package on top —
which is what you actually touch: implement one interface, call one
function, never see a `cm.List`.

## The API

```go
// The two-phase contract, in Go clothing.
type Plugin interface {
    Describe() Manifest
    Parse(subject Subject, options map[string]string) ([]byte, error)
    Assemble(partials [][]byte, options map[string]string) (Output, error)
}

func Register(p Plugin)              // call from init() in package main

// The host — the entire capability grant.
func List(suffix string) ([]string, error)  // readable paths, sorted
func Read(path string) ([]byte, error)      // rooted; ../ is refused resolved
func Label() (string, bool)                 // the tree's name, if it has one
```

Facts are plain structs — `Node{Key, Label, ExtraLabels, Props}`,
`Edge{Src, Dst, Type, Props}` — with `Props map[string]any` marshalled to
the JSON object string the contract carries. `Described(desc, value)` builds
the database's self-explaining `{"$desc": …, "$value": …}` form.

`Parse` sees one chunk and returns an **opaque partial**: serialize whatever
your own `Assemble` wants to read back (the official Go plugin uses
`encoding/json` of its per-file facts). It may run concurrently in instances
that share nothing, so it must depend only on its input and the host.
`Assemble` runs once with every partial in chunk order; cross-file
resolution belongs there, and the result must not depend on where the chunk
boundaries fell.

## The one rule: copy before use

Everything lifted from the canonical ABI is **copied before use**, and every
result is copied before return. A `cm` slice is a *view* into the ABI
buffer; holding one across the allocations a real parse makes invites
TinyGo's collector to move the ground out from under it. This was found the
hard way — a 22-chunk assemble decoding garbage that 20 chunks never
produced — and the `ext` package now does the copying for you at every
boundary. If you drop down to `bindings/` directly, the rule is yours to
keep.

## Building

TinyGo ≥ 0.41 with `wasm-tools` on PATH (TinyGo shells out to it to lift
the module into a component):

```console
$ tinygo build -target=wasip2 -scheduler=none -gc=leaking \
    --wit-package ./wit --wit-world drsg:preprocess-build/plugin-go -o mine.wasm .
```

Both flags are load-bearing:

- `-scheduler=none` — the scheduler otherwise runs between a wasmexport's
  return and the host reading the result, and its GC can collect the very
  buffer being returned;
- `-gc=leaking` — the conservative collector trapped under wasmexport even
  without the scheduler. Leaking is acceptable *because of the host's
  design*: every call runs in a fresh store, so what leaks dies with the
  call, and the store's memory limit is the bound.

Your plugin needs a `wit/` build package that combines the contract with the
WASI imports TinyGo's runtime needs to start — copy
[`plugins/go/component/wit`](../../plugins/go/component/wit) wholesale; its
`world.wit` (`drsg:preprocess-build/plugin-go`) exists only so
`wasm-tools component new` can resolve the runtime's imports.

## Testing

Keep the parsing logic in a plain Go package with its own tests (`go test`,
no TinyGo anywhere near it) and make the `main` package a thin `Plugin`
adapter — the shape [`plugins/go`](../../plugins/go) demonstrates: 29 native
tests on the parser, ~90 lines of component.

## The contract copy

`wit/preprocess.wit` here is a vendored copy of the canonical contract at
the repository root; the bindings under `bindings/` are generated from it
with `wit-bindgen-go` (`just go-bindings` regenerates). CI fails if the copy
drifts from canonical.
