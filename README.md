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
plugins/toml/          the smallest plugin that is still a plugin
```

Rust is the first of five language ecosystems planned here — Go, Java,
TypeScript and Python follow, each with an SDK under `sdk/` publishing to its
own registry, and a plugin under `plugins/`.

Every SDK generates from `wit/preprocess.wit`. Copies exist where packaging
demands one (a crate cannot publish a file outside itself); `just check-wit`
fails if any has drifted, and `just vendor-wit` refreshes them.

## Why its own repository

Official does not mean lock-step. A plugin ships a fix without waiting for a
database release, and the database releases without waiting for five language
toolchains. The contract is the only thing that has to move in step, which is
why it is small and versioned.

## What a plugin can reach

Exactly `host.list`, `host.read` and `host.label` — no filesystem, no network,
no environment, and no way to write anywhere. Reads are rooted at the directory
the host was pointed at, checked on the resolved path. A component that imports
`wasi:filesystem` or `wasi:sockets` is refused at install rather than granted an
empty one.

## Building a plugin

```
cargo build --manifest-path plugins/toml/Cargo.toml --target wasm32-wasip2 --release
drsg plugin install plugins/toml/target/wasm32-wasip2/release/drsg_plugin_toml.wasm
```
