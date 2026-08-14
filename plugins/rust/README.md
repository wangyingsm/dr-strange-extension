# plugin: rust

Parses Rust source into graph facts. Manifest `rust@2`, claims `.rs`.
Built on [syn](https://crates.io/crates/syn) — the parser the macro
ecosystem itself runs on — parse-only: no type inference, no macro
expansion, which is the point. `@2` is the **fact-format version** (the
shape of the facts changed once, from the in-database prototype); it is
independent of the release tag.

## Layout

```
parser/     drsg-rust-parser — the language logic, a plain library, 37 native tests
component/  drsg-plugin-rust — the wasm wrapper: Guest impl + rmp-serde partials
```

## Keys

An item's identity is its **module path** — what a Rust programmer calls it
and what a model will recognise:

```
my_crate                              lib.rs (names the crate root, not "lib")
my_crate::api::cache                  a module (file or inline)
my_crate::api::cache::brute_force_search
my_crate::Thing::read                 an inherent method
<my_crate::Thing as core::fmt::Display>::fmt    a trait-impl method — real
                                      qualified-path syntax, the only thing
                                      keeping six `impl From<…>` blocks from
                                      all claiming one key
```

The crate name comes from the nearest `Cargo.toml`'s `[package] name`
(`-` → `_`), read through the host — so a digest rooted at `…/foo/src`
still keys as `foo::…`, and two crates' `api::Thing` never merge.

## Nodes

| Label | Emitted for | Props beyond `doc_comment` / `visibility` / `file` / `line` |
|---|---|---|
| `Module` | each file and each inline `mod` | `path` (crate-root-relative: `src/compute/cache.rs`), `imports` (resolved use-targets, joined) |
| `Function` / `Method` | free fns, impl fns (`Method` iff it takes `self`) | `signature`, `returns`, `receiver`, `local_bindings`, `is_async` (present only when true) |
| `Struct` / `Enum` / `Union` | type declarations | `fields` (described list of `vis name: type`, declaration order) / `variants` (described list, `Unit`, `Lit(i64)`, `A = 1`); `non_exhaustive` when marked |
| `Trait` | trait declarations | its items become nodes reached by `HAS_METHOD` |
| `Const` / `Static` | consts and statics | type under `signature`, initializer under `value` — **as written, never evaluated** (`256 * 1024` stays an expression) |
| `TypeAlias` | `type X = …` | aliased type under `signature` |
| `Macro` | `macro_rules!` definitions | — |
| stand-ins | anything referenced but not declared here | label says what the reference proved (`Function`, `Trait`, `Type`, bare `External` when only a `use` was seen) + extra label `External`; **no props** — for a stand-in the key *is* the fact |

With `include_source = "true"` (from `[plugins.rust]`), each item also
carries `_code`: the source as written, described as retrieval-only — the
`_` prefix keeps it out of embeddings and the schema summary.

## Edges

| Type | Meaning | `line` |
|---|---|---|
| `CONTAINS` | module → item, type → variant | declaration site |
| `HAS_METHOD` | trait/type → its methods | method's line |
| `CALLS` | function → what it calls | **call site** |
| `IMPLEMENTS` | type → trait (`impl` blocks); `From<i64>` rides the edge as an `impl` prop rather than minting a second `From` node | the `impl` keyword |
| `IMPORTS` | module → what its `use` statements name (with `as_written` when aliased) | the `use` statement |
| `INVOKES` | module → an item-position macro invocation, `arguments` described on the edge — a **marked blind spot**: nothing expands macros, so the items they define are absent, but where they are defined is findable | invocation site |

## Resolution — the certainty line

- A call written as a **path** (`fs::read(…)`, `Vec::new()`) is expanded
  against the file's own `use` list and binds exactly; paths nothing here
  declares become external stand-ins (that is what "this crate uses that"
  needs).
- A **bare name** binds by scope proximity; a name with two equally-close
  candidates is **ambiguous — counted, not guessed**.
- A **method call** (`.read()`) names no path, and the receiver's type is
  what a parser cannot know: counted, never guessed.
- **Re-exports** (`pub use`, including `pub(crate) use`) create the facade
  paths later references resolve through.
- A key seen twice is nearly always two `#[cfg]` alternatives of one item —
  settled here (first wins) and counted, not treated as a collision.

Every count lands in the report notes, so a thin graph explains itself:
unresolved method calls, external calls, ambiguous names, unexpanded macro
invocations.

## Options (`[plugins.rust]` in drsg.toml)

| Key | Effect |
|---|---|
| `include_source = "true"` | attach `_code` to items |

## Build & test

```console
$ cd parser    && cargo test          # 37 tests, no wasm toolchain
$ just rust-plugin                    # → component/target/wasm32-wasip2/release/drsg_plugin_rust.wasm
$ drsg plugin install …/drsg_plugin_rust.wasm
```

Partials cross the phase boundary as **MessagePack** (`rmp-serde`): binary
because the partials for a large tree are megabytes, self-describing because
the facts carry `serde_json::Value` properties — the partial format is the
plugin's own business; the host never looks inside.

## Known limits

Macro-generated items are absent (marked by `INVOKES`); trait-method calls
on generic receivers are method calls, hence counted; `#[cfg]` selection is
not evaluated — both arms' items exist, duplicates counted.
