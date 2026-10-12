# plugin: rust

English · [简体中文](README_CN.md)

Parses Rust source into graph facts. Manifest `rust@3`, claims `.rs`.
Built on [syn](https://crates.io/crates/syn) — the parser the macro
ecosystem itself runs on — parse-only: no type inference, no macro
expansion, which is the point. `@2` is the **fact-format version** (the
shape of the facts changed once, from the in-database prototype); it is
independent of the release tag.

## Layout

```
parser/     drsg-rust-parser — the language logic, a plain library, 71 native tests
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

| Label | Emitted for | Props beyond `doc_comment` / `visibility` / `file` / `line` / `end_line` |
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

Every declaration also carries `end_line`: where it stops, so `snippet` reads
exactly the declaration instead of a fixed number of lines after its first,
and a reader sees whether a thing is six lines or two hundred without opening
it. `line` still points at the **name**, so documentation above a declaration
cannot move where the graph says it starts.

## Edges

| Type | Meaning | `line` |
|---|---|---|
| `CONTAINS` | module → item, type → variant | declaration site |
| `HAS_METHOD` | trait/type → its methods | method's line |
| `CALLS` | function → what it calls. Carries `concurrent` when the site departs from waiting: `spawned` for work handed to another task or thread, `blocking` for `spawn_blocking`. One caller reaching one callee both ways folds to one edge carrying the union, so a site that awaits cannot swallow one that spawns by being written first | **call site** |
| `IMPLEMENTS` | type → trait, from an `impl` block **or a `#[derive(...)]`** — the same fact either way, with `derived` on the edge to say which spelling the source used. `From<i64>` rides the edge as an `impl` prop rather than minting a second `From` node | the `impl` keyword, or the derive |
| `IMPORTS` | module → what its `use` statements name (with `as_written` when aliased) | the `use` statement |
| `INSTANTIATES` | function → a type it **builds**, with `variant` on the edge when it built one of an enum's. Construction is not a call: `Ok(v)`, `Mine::A(v)`, `Meters(1.0)` and `Widget { .. }` are all spelled like calls or literals and all name a *type*, so none of them mints a `Function` node for a constructor that is no item | construction site |
| `INVOKES` | module → an item-position macro invocation, `arguments` described on the edge — a **marked blind spot**: nothing expands macros, so the items they define are absent, but where they are defined is findable | invocation site |
| `REFERENCES` | function → a function it **passes as a value** rather than calls | the argument |
| `ANNOTATED_BY` | item → an attribute on it — `#[tokio::main]`, `#[get("/health")]`, `#[serde(...)]`. The *path* is the node and the whole attribute rides on the edge as `arguments`, so `get` stays one node with a route per edge instead of a node per route. `derive` leaves by the other door; `doc`, `non_exhaustive`, the `#[test]` family, lint and codegen attributes and `cfg` stay out | the attribute |
| `USES_TYPE` | declaration → a type it is **typed by**, `role` on the edge saying which position (`field`, `param`, `return`, `variant`, `alias`) and the union of them when one pair is written twice. Generic arguments are walked — `mpsc::Sender<Job>` is a dependency on `Job` — and only types this tree declares get an edge; a foreign one stays text in `signature`/`fields`. Never a self-loop | — |

## Routes

An HTTP route is the one reference this plugin cannot turn into an edge: the
handler or the caller is in another language, often another repository, and
between the two sides there is only a string. So the plugin records what the
source *states* and leaves the match to drsg's `wire`, which computes it per
call and never stores it.

On the symbol that already exists:

| Property | Set on | Shape |
|---|---|---|
| `serves` | the handler an attribute marks, or that a router's `.route` names | `GET /api/orders/{}` — the verb, and the path with every variable segment reduced to `{}` |
| `requests` | the function making the call | the same shape; `*` where the source states no verb |
| `serves_as_written` / `requests_as_written` | both | the path exactly as the source wrote it, before normalising |

**Declarations** come in both of Rust's idioms. An attribute carrying a path
records on the function below it — `#[get("/x")]` (actix, rocket), with the
verb read from the attribute's own name. And axum's builder records on the
**handler it names**: `.route("/orders/{id}", get(list).post(create))` puts the
route on `list` and on `create`, with one verb each, which is how axum says it
and needs no attribute at all.

**A nested router's prefix reaches its routes.** `.nest("/api", api())` gives a
router a prefix that belongs to every route hung on it and to none of them yet,
so the nesting function serves nothing — the prefix reaches the handlers
instead.

**Requests** are any call with a path-shaped string argument, whatever the
callee is called — `reqwest` and a project-local wrapper alike. An absolute URL
or a query string reduces to the path.

**A path-shaped literal that is not a route is recorded anyway.**
`std::fs::read_to_string("/etc/app/config.toml")` looks exactly like a request
and the parser cannot tell the difference. It is recorded, matches no
declaration, and is counted as a request that reached none — a named miss
rather than a wrong edge, which is the trade this whole shape takes.

## Resolution — the certainty line

- A call written as a **path** (`fs::read(…)`, `Vec::new()`) is expanded
  against the file's own `use` list — by the name each `use` **introduced**,
  so `use a::b as c` puts `c` in scope and not `b` — and binds exactly; paths nothing here
  declares become external stand-ins (that is what "this crate uses that"
  needs).
- A **bare name** binds by scope proximity; a name with two equally-close
  candidates is **ambiguous — counted, not guessed**.
- A **method call** (`.read()`) names no path; it resolves as far as the
  receiver's type can be **read off the body**. Type arguments carry through
  every binding a body writes: a parameter or `let` annotation, a field's
  declared type, a constructor path (`Vec::new()`), a declared return, a
  `type` alias with its parameters filled in, a constant, and chains of
  those (`self.items.iter().map(…)`) through a table of what std's own
  methods return — so `for n in &v`, `if let Some(n) = …`, `match` arms on
  the tree's own enums, `let (a, b) = …`, `Node { key, .. }` and the
  parameters of `v.iter().map(|n| …)` are typed, `?`/`.unwrap()` reach a
  `Result<T>`'s `T`, a closure whose body is a chain says what `map`
  yields, and `collect::<Vec<_>>()` is a `Vec`. A **channel constructor** is the pair
  it returns — `let (tx, rx) = mpsc::channel()` types both halves, so
  `tx.send(v)` and `rx.recv()` land on `mpsc::Sender::send` and
  `mpsc::Receiver::recv` instead of the ledger; keyed by the constructor's
  own module (`std::sync::mpsc`, `tokio::sync::mpsc`, `crossbeam::channel`),
  so it is the same node an annotated `let rx: mpsc::Receiver<T>` resolves
  to rather than a second spelling of one type. A type this tree declares
  lands on its own method; `T: Tr`, `impl Tr` and `dyn Tr` land on the
  trait's; a type the tree does not declare — std's, a dependency's — lands
  on an external stand-in keyed by the type that **declares** the method —
  `Vec::push`, `str::trim`, `Clone::clone`, and `Iterator::collect` for every
  concrete iterator (`Range`, `Lines`, `Chars`) that answers through the
  trait — which is all that is known about it. A
  receiver whose type nothing states — an unbounded generic, the result of
  a foreign function no table knows — is **counted, never guessed**, and
  the ledger edge says which hop stopped it.
- **Re-exports** (`pub use`, including `pub(crate) use`) create the facade
  paths later references resolve through.
- A key seen twice is nearly always two `#[cfg]` alternatives of one item —
  settled here (first wins) and counted, not treated as a collision.

Every count lands in the report notes, so a thin graph explains itself:
unresolved method calls, external calls, ambiguous names, unexpanded macro
invocations.

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
| `attribute` | `definitive` | a `#[test]`-family attribute — the bare one, a runtime's own (`#[tokio::test]`, `#[async_std::test]`), `#[bench]`, `#[rstest]` |
| `build-rule` | `definitive` | inside a `#[cfg(test)]` module, or a file under `tests/` / `benches/` |

Both are cargo's rule rather than a convention: a `#[cfg(test)]` module is
compiled out of the library, and `tests/`/`benches/` are targets of their own
that never link into it. Both are *scopes*, so they reach the `impl` blocks
written inside them, whose methods are keyed only at assemble. First writer
wins, and the narrowest rule runs first: a `#[test]` fn inside a
`#[cfg(test)]` module keeps `attribute`. The cfg is read as tokens, not text,
which settles both traps — `cfg(feature = "test")` carries a string and not
the identifier, and `cfg(not(test))` has its group skipped. A
`#[cfg(test)] mod tests;` whose body is another *file* is flagged only if
that file is itself under a flagged scope: which module declared it is not
known until assemble.

## Options (`[plugins.rust]` in drsg.toml)

| Key | Effect |
|---|---|
| `include_source = "true"` | attach `_code` to items |

## Build & test

```console
$ cd parser    && cargo test          # native, no wasm toolchain
$ DRSG_EVAL_ROOT=/path/to/a/tree cargo test -- --ignored eval_resolution --nocapture
                                      # how calls resolve over a real tree, and what leads the ledger
$ just rust-plugin                    # → component/target/wasm32-wasip2/release/drsg_plugin_rust.wasm
$ drsg plugin install …/drsg_plugin_rust.wasm
```

Partials cross the phase boundary as **MessagePack** (`rmp-serde`): binary
because the partials for a large tree are megabytes, self-describing because
the facts carry `serde_json::Value` properties — the partial format is the
plugin's own business; the host never looks inside.

## Known limits

`join!`/`select!` are macros, and syn hands the parser an opaque token
stream, so the calls inside them are not seen at all — a concurrency blind
spot that shares its cause with the one below.

Macro-generated items are absent (marked by `INVOKES`); trait-method calls
on generic receivers are method calls, hence counted; a chain through a std
method whose return the parser has no fact for (`map.get(k)?.m()`) stops
there; `#[cfg]` selection is not evaluated — both arms' items exist,
duplicates counted.
