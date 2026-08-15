# dr-strange-ext — the Rust SDK

English · [简体中文](README_CN.md)

The one dependency a Rust plugin author takes. It carries the generated WIT
bindings for the `drsg:preprocess` contract, an ergonomic layer over them,
and nothing else of ours: the dependency list is `wit-bindgen` and
`serde_json`, full stop. A guest has no business compiling a storage engine
to name a property.

## What is in the crate

| Item | What it is |
|---|---|
| `bindings` | `wit_bindgen::generate!` output for the `plugin` world — the raw contract types (`Manifest`, `Input`, `Node`, `Edge`, `Output`, `Report`) and the `Guest` trait a component exports |
| `export_plugin!` | the wit-bindgen export macro, re-exported at the root: `export_plugin!(MyType)` wires a `Guest` impl to the component's exports |
| `host` | the host interface, callable directly: `host::list(suffix)`, `host::read(path)`, `host::label()` — the **entire** capability grant |
| `Simple` + `simple_plugin!` | the one-function facade for formats with no cross-file structure (below) |
| `node(key, label)` / `edge(src, ty, dst)` | builders: `.prop(k, v)`, `.described(k, desc, v)`, `.extra_label(l)`, `.build()` — properties are rendered to the JSON object string the contract carries |
| `output()` + `OutputExt::finish()` | an `Output` accumulator; `finish()` fills the report's fact counts from what you pushed |
| `partial` | helpers for `Simple`: encode/decode/merge an `Output` through the opaque-partial channel |

## Two ways to write a plugin

**The facade** — for anything where one input's facts do not depend on
another input:

```rust
use dr_strange_ext::{Input, Manifest, Output, OutputExt, Simple, host, node, output, simple_plugin};

struct Mine;

impl Simple for Mine {
    fn describe() -> Manifest {
        Manifest { name: "mine".into(), version: "1".into(), extensions: vec!["xyz".into()] }
    }

    fn process(subject: Input, _options: &[(String, String)]) -> Result<Output, String> {
        let mut out = output();
        if let Input::Files(paths) = subject {
            for path in paths {
                let bytes = host::read(&path)?;
                out.nodes
                    .push(node(&path, "Thing").prop("bytes", bytes.len() as i64).build());
            }
        }
        Ok(out.finish())
    }
}

simple_plugin!(Mine);
```

The SDK derives both contract phases from `process`: `parse` runs it per
chunk and serializes the result as the partial; `assemble` concatenates the
partials in chunk order. You never see the two-phase machinery.

**The full contract** — for real language parsers, implement the generated
`Guest` trait yourself:

```rust
use dr_strange_ext::bindings::exports::drsg::preprocess::preprocessor::{Guest, Input, Manifest, Output};
use dr_strange_ext::export_plugin;

struct Mine;

impl Guest for Mine {
    fn describe() -> Manifest { /* … */ }

    /// One chunk → an opaque partial. May run concurrently in instances
    /// that share nothing, so depend only on the input and the host.
    fn parse(subject: Input, options: Vec<(String, String)>) -> Result<Vec<u8>, String> {
        // parse each file, serialize per-file facts however you like
        // (the official plugins use rmp-serde: binary, self-describing)
    }

    /// Every partial, in chunk order → the result. Cross-file resolution
    /// lives here — the result must not depend on where chunk boundaries
    /// fell.
    fn assemble(partials: Vec<Vec<u8>>, options: Vec<(String, String)>) -> Result<Output, String> {
        // decode, resolve across files, emit nodes/edges/notes
    }
}

export_plugin!(Mine);
```

Every official plugin in this repository keeps the language logic in a plain
`parser/` library crate (tested natively, no wasm toolchain needed) under a
thin `component/` wrapper that only crosses this boundary — copy that split.

## Properties and the described form

A node's `properties` cross the contract as one JSON object string. The
builders render it for you. Two shapes matter:

- plain values: `.prop("signature", "fn parse(text: &str) -> Ast")`
- **described** values: `.described("fields", "the fields it declares", vec![…])`
  becomes `{"$desc": …, "$value": …}` — the database's self-explaining
  property form; the dashboard renders the value and shows the description
  on demand.

Convention from the official plugins, worth following: keys are the
language's own qualified names, every definition carries `file` and `line`
(1-based), `_`-prefixed props (like `_code`) are retrieval-only and stay out
of embeddings, and whatever your parser cannot know for certain is counted
in a note, never guessed.

## Building

```console
$ cargo build --release --target wasm32-wasip2
$ drsg plugin install target/wasm32-wasip2/release/<name>.wasm
```

The crate type must be `cdylib`. No wasi-sdk, no adapters: a pure-Rust guest
builds with the stock `wasm32-wasip2` target.

## The contract copy

`wit/preprocess.wit` here is a **vendored copy** of the canonical contract
at the repository root (a crate cannot publish a file outside itself). CI
fails if they drift; `just vendor-wit` refreshes every copy after a
canonical change.
