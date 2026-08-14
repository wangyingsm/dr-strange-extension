# dr-strange-extensions

The **official extension repository** for
[dr-strange](https://github.com/wangyingsm/dr-strange): the preprocessor plugins
we maintain, and the SDKs for writing your own (ROADMAP §11).

A plugin turns a format-specific input into **facts** — nodes and edges it is
certain about — and **prose**, the residue that still needs a model. An input
that yields only facts is digested with **no model call at all**: an AST does
not infer that `parse()` calls `lex()`, it knows.

```
wit/preprocess.wit     the contract — language-neutral, and canonical
sdk/rust/              dr-strange-ext, published to crates.io
sdk/go/                the Go SDK: generated bindings and the ext package
plugins/rust/          the Rust parser (parser/ native + component/ wrapper)
plugins/go/            the Go parser, same split
plugins/ts/            TypeScript *and* JavaScript — one swc parser, both
plugins/py/            Python, on ruff's parser
plugins/java/          Java, on tree-sitter's grammar
plugins/c/             C, on tree-sitter's grammar — flat-namespace keys
plugins/toml/          the smallest plugin that is still a plugin
```

All five planned language ecosystems now ship official parsers — Rust, Go,
TS/JS, Python, and Java. Each ecosystem gets an SDK under `sdk/` publishing
to its own registry; the `ts`, `py` and `java` plugins themselves are
written in Rust (`swc`, ruff's parser, and tree-sitter respectively) — the
per-ecosystem SDK proofs are their own later slices, so SDK risk never
touches a flagship parser. The Java grammar is C: wasi-sdk's clang compiles
it for the sandbox, and the same toolchain opens the door to C and zig
plugins later.

Every SDK generates from `wit/preprocess.wit`. Copies exist where packaging
demands one (a crate cannot publish a file outside itself); `just check-wit`
fails if any has drifted, and `just vendor-wit` refreshes them.

## Why its own repository

Official does not mean lock-step. A plugin ships a fix without waiting for a
database release, and the database releases without waiting for five language
toolchains. The contract is the only thing that has to move in step, which is
why it is small and versioned.

## What a plugin can reach

Exactly `host.list`, `host.read` and `host.label` — no network, no
environment, and no way to write anywhere. Reads are rooted at the directory
the host was pointed at, checked on the resolved path.

A component may *import* `wasi:filesystem` — the TinyGo, Python and JS
runtimes plant that import before a plugin's first line runs — but the grant
behind it is an empty preopen table: there is no directory handle to read,
probe, or enumerate. `wasi:sockets` is refused at install by name, because no
runtime needs sockets to start; that import is intent. Clocks are frozen and
`wasi:random` deals a fixed byte sequence, so a runtime that seeds map order
from entropy (Go does) still emits the same facts on every run.

## Building a plugin

Rust (stable, `wasm32-wasip2` target):

```
just rust-plugin
drsg plugin install plugins/rust/component/target/wasm32-wasip2/release/drsg_plugin_rust.wasm
```

Go (TinyGo ≥ 0.41 and wit-bindgen-go):

```
just go-plugin
drsg plugin install plugins/go/component/go.wasm
```

TypeScript/JavaScript, Python and Java (stable Rust; Java needs wasi-sdk
for its C grammar — see the justfile):

```
just ts-plugin && just py-plugin && just java-plugin
drsg plugin install plugins/ts/component/target/wasm32-wasip2/release/drsg_plugin_ts.wasm
drsg plugin install plugins/py/component/target/wasm32-wasip2/release/drsg_plugin_py.wasm
drsg plugin install plugins/java/component/target/wasm32-wasip2/release/drsg_plugin_java.wasm
```

The Go build pins `-scheduler=none -gc=leaking` — the reasons are written on
the recipe in the `justfile`, and they are load-bearing. The host runs every
call in a fresh store, which is what makes a leaking collector acceptable:
what leaks dies with the call.
