<p align="center">
  <img src="assets/logo.svg" alt="dr-strange-extension" width="240">
</p>

<h1 align="center">dr-strange-extension</h1>

<p align="center">
  The <b>official extension repository</b> for the
  <a href="https://github.com/wangyingsm/dr-strange">Dr-STRANGE</a> graph database:
  sandboxed WebAssembly preprocessor plugins that turn source code into graph
  facts before any model reads it, and the SDKs for writing your own.
</p>

<p align="center">
  <a href="https://github.com/wangyingsm/dr-strange-extension/actions/workflows/ci.yml"><img
    src="https://github.com/wangyingsm/dr-strange-extension/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/wangyingsm/dr-strange-extension/releases"><img
    src="https://img.shields.io/github/v/release/wangyingsm/dr-strange-extension?sort=date&label=latest%20release&color=d9a441" alt="Releases"></a>
  <a href="#license-and-contribution"><img
    src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue" alt="License: MIT OR Apache-2.0"></a>
</p>

<p align="center">English · <a href="README_CN.md">简体中文</a></p>

---

## Dr-STRANGE, in one paragraph

[Dr-STRANGE](https://github.com/wangyingsm/dr-strange) is an AI-native embedded
graph database: planes of nodes and edges with a soft schema, hybrid retrieval
fusing vector, keyword and graph proximity, time travel, a change feed,
natural-language querying — and `drsg digest`, which ingests documents and repositories into a
knowledge graph. When `digest` meets **source code**, it does not ask a model to
guess at structure: it routes each file to a **preprocessor plugin** that parses
it into facts a compiler-grade parser is certain of. A repository that yields
only facts is ingested with **no model call at all** — an AST does not infer
that `parse()` calls `lex()`, it knows.

Those plugins live here, apart from the database on purpose: official does not
mean lock-step. A parser ships a fix without waiting for a database release,
and the database releases without waiting for eight toolchains.

## Supported extensions

Every plugin is a sandboxed `wasm32-wasip2` component, installed from its
release URL. drsg pins the artifact's sha256 at install and re-checks it at
every load.

| Plugin | Claims | Parser underneath | Install |
|---|---|---|---|
| `rust` | `.rs` | [syn](https://crates.io/crates/syn) | [latest](https://github.com/wangyingsm/dr-strange-extension/releases?q=rust-v&expanded=true) |
| `go` | `.go` | Go's own `go/parser`, via TinyGo | [latest](https://github.com/wangyingsm/dr-strange-extension/releases?q=go-v&expanded=true) |
| `ts` | `.ts .tsx .mts .cts .js .jsx .mjs .cjs` | [swc](https://swc.rs) — ESM **and** CommonJS | [latest](https://github.com/wangyingsm/dr-strange-extension/releases?q=ts-v&expanded=true) |
| `py` | `.py .pyi .pyw` | [ruff](https://github.com/astral-sh/ruff)'s parser | [latest](https://github.com/wangyingsm/dr-strange-extension/releases?q=py-v&expanded=true) |
| `java` | `.java` | [tree-sitter-java](https://github.com/tree-sitter/tree-sitter-java) | [latest](https://github.com/wangyingsm/dr-strange-extension/releases?q=java-v&expanded=true) |
| `c` | `.c .h` | [tree-sitter-c](https://github.com/tree-sitter/tree-sitter-c) | [latest](https://github.com/wangyingsm/dr-strange-extension/releases?q=c-v&expanded=true) |
| `web` | `.html .htm .css` | tree-sitter html/css/js — one plugin, so `class="btn"` binds to the stylesheet that defines `.btn` | [latest](https://github.com/wangyingsm/dr-strange-extension/releases?q=web-v&expanded=true) |
| `toml` | `.toml` | [toml](https://crates.io/crates/toml) — the smallest plugin that is still a plugin | [latest](https://github.com/wangyingsm/dr-strange-extension/releases?q=toml-v&expanded=true) |

Each *latest* link filters the [releases page](https://github.com/wangyingsm/dr-strange-extension/releases)
to that plugin's tags, newest first; every release carries the `<plugin>.wasm`
and its `.sha256`. The easiest path needs no URL at all: a bare
`drsg plugin install` lists this catalog interactively, pinned to the
versions known-good with your drsg build.

```console
$ drsg plugin install https://github.com/wangyingsm/dr-strange-extension/releases/download/<tag>/rust.wasm
installed rust@2  sha256:8e3c32be0add
  handles: .rs
```

Every parser follows one discipline: keys are the language's *own* qualified
names (`crate::module::fn`, `pkg.Type.Method`, `file.c::func`,
`index.html#map`), every definition carries its `file` and `line`, every edge
the line it is written on — and whatever a parser cannot know for certain
(a method receiver's type, which of two definitions links, a class two
stylesheets define) is **counted in the report, never guessed**.

## The contract

The plugin ↔ host contract is one small [WIT](wit/preprocess.wit) world,
`drsg:preprocess`, canonical in this repository and vendored by drsg:

```wit
interface host {
  %list: func(suffix: string) -> result<list<string>, string>;
  read:  func(path: string) -> result<list<u8>, string>;
  label: func() -> option<string>;
}

interface preprocessor {
  describe: func() -> manifest;                          // name, version, extensions
  parse:    func(subject: input, options: list<tuple<string, string>>)
              -> result<list<u8>, string>;               // one chunk → an opaque partial
  assemble: func(partials: list<list<u8>>, options: list<tuple<string, string>>)
              -> result<output, string>;                 // all partials, in order → facts
}
```

**Two phases, deliberately.** The host splits the routed files into chunks and
runs `parse` over them in parallel — fresh store per call, sharing nothing —
then calls `assemble` once with every partial in chunk order. Cross-file
resolution (imports, headers, barrel re-exports, interface satisfaction) lives
in `assemble`, inside the plugin, because it is language semantics and the
database holds none. The partial is opaque bytes: serialize whatever your own
`assemble` wants to read back.

**Input is pull, not push.** The three `host` functions above are the *entire*
capability grant. A plugin pulls the files around the one it was handed — how a
code parser follows an import — and reads are rooted at the digested directory,
checked on the resolved path. Beyond that the sandbox grants nothing: no
network (`wasi:sockets` is refused at load, by name), an empty filesystem
preopen table, frozen clocks, fixed entropy, instruction and memory budgets —
so re-ingesting a tree yields byte-identical facts, every time. Whatever a
plugin produces comes back as a **return value**; only the host writes to the
database.

## Writing a plugin

### Rust

Depend on the SDK — `wit-bindgen` + `serde_json` and nothing else of ours —
and implement either the two-phase contract or, for anything without
cross-file work, the one-function facade:

```toml
[dependencies]
# crates.io publication pending; until then, the git dependency:
dr-strange-ext = { git = "https://github.com/wangyingsm/dr-strange-extension" }

[lib]
crate-type = ["cdylib"]
```

```rust
use dr_strange_ext::{Input, Manifest, Output, OutputExt, Simple, host, node, output, simple_plugin};

struct MyPlugin;

impl Simple for MyPlugin {
    fn describe() -> Manifest {
        Manifest { name: "mine".into(), version: "1".into(), extensions: vec!["xyz".into()] }
    }

    /// One subject at a time; the SDK derives parse/assemble from this.
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

simple_plugin!(MyPlugin);
```

Build and install:

```console
$ cargo build --release --target wasm32-wasip2
$ drsg plugin install target/wasm32-wasip2/release/my_plugin.wasm
```

For real parsers, implement the generated `Guest` trait directly (see
[`plugins/rust`](plugins/rust) — `parse` returns a serialized partial per
chunk, `assemble` resolves across all of them) and pull neighbouring files
through `dr_strange_ext::bindings::drsg::preprocess::host`.

### Go

Depend on the SDK module, implement the `ext.Plugin` interface, and build
with TinyGo (≥ 0.41, with `wasm-tools` on PATH):

```console
$ go get github.com/wangyingsm/dr-strange-extension/sdk/go
```

```go
package main

import ext "github.com/wangyingsm/dr-strange-extension/sdk/go"

type mine struct{}

func (mine) Describe() ext.Manifest {
    return ext.Manifest{Name: "mine", Version: "1", Extensions: []string{"xyz"}}
}

func (mine) Parse(subject ext.Subject, options map[string]string) ([]byte, error) {
    // Pull files via ext.List / ext.Read; serialize your partial.
    return []byte{}, nil
}

func (mine) Assemble(partials [][]byte, options map[string]string) (ext.Output, error) {
    return ext.Output{Nodes: []ext.Node{{Key: "k", Label: "Thing"}}}, nil
}

func init() { ext.Register(mine{}) }
func main() {}
```

```console
$ tinygo build -target=wasip2 -scheduler=none -gc=leaking \
    --wit-package ./wit --wit-world drsg:preprocess-build/plugin-go -o mine.wasm .
```

The flags are load-bearing (the [`justfile`](justfile) explains why); copy
[`plugins/go/component/wit`](plugins/go/component/wit) for the build world.
One rule runs through the Go SDK: everything lifted from the ABI is copied
before use — a `cm` slice is a view the collector can move out from under you.

Whatever the language: run `just check-wit` before building (the vendored
contract copies must match the canonical one), and test your parser natively —
every official plugin keeps its parser a plain library under a thin component
wrapper, so the tests need no wasm toolchain at all.

## License and contribution

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option — the same terms as the database.

Contributions are welcome:

- **A new language or format** starts as an issue naming the parser you would
  build on (the pattern: a mature, ideally canonical parser — syn, swc, ruff,
  tree-sitter — wrapped as `plugins/<name>/{parser,component}`).
- **Parser fixes** should come with a native test that fails before and passes
  after; CI runs every parser's suite, an uncached `clippy -D warnings`, and
  builds all eight components to `wasm32-wasip2` on every push.
- **Contract changes** are the one thing that moves in lock-step with the
  database — open the discussion on the
  [dr-strange](https://github.com/wangyingsm/dr-strange) side first.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
