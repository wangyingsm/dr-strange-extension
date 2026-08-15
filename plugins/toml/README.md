# plugin: toml

English · [简体中文](README_CN.md)

The smallest plugin that is still a plugin. Manifest `toml@1`, claims
`.toml`; emits one `Manifest` node per file, with the byte count and a
described `path`.

It exists to prove four things at once:

1. the contract is implementable **from the SDK alone** — its dependency
   list is `dr-strange-ext` and the `toml` crate, nothing else of ours;
2. the host's capability grant (`list` / `read` / `label`) is enough to do
   real work;
3. a plugin needs **nothing from the database's own crates** — checked, not
   assumed: `cargo tree` here contains no `dr-strange-core`, no `wasmtime`;
4. a format with no cross-file structure writes **one function** — it
   implements [`Simple`](../../sdk/rust), and the SDK derives both contract
   phases: `parse` runs `process` per chunk, `assemble` concatenates in
   chunk order.

It is also the template: if you are writing your first plugin, start by
reading [`src/lib.rs`](src/lib.rs) top to bottom — it is ~60 lines, and
every line demonstrates a convention (per-file nodes, described props, the
skipped-file accounting that keeps a thin graph explaining itself).

## Build & test

```console
$ just toml-plugin        # cargo build --release --target wasm32-wasip2
$ drsg plugin install target/wasm32-wasip2/release/drsg_plugin_toml.wasm
```

No parser/component split here — there is nothing to test natively that the
SDK's own tests do not already cover.
