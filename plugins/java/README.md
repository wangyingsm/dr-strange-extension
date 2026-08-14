# plugin: java

Parses Java into graph facts. Manifest `java@1`, claims `.java`. Built on
[tree-sitter-java](https://github.com/tree-sitter/tree-sitter-java) — no
mature pure-Rust Java frontend exists, and the grammar every editor already
trusts is a better foundation than one we would have to earn trust for. The
C runtime and grammar compile to `wasm32-wasip2` under wasi-sdk's clang.

## Layout

```
parser/     drsg-java-parser — the language logic, 18 native tests
component/  drsg-plugin-java — Guest impl + rmp-serde partials (needs wasi-sdk to build)
```

## Keys

Java's own qualified names. **A file is not a node**: in Java the type is
the unit and the package is the container.

```
com.acme.core                         a package
com.acme.core.Engine                  a type
com.acme.core.Engine.start            a member
com.acme.core.Engine.Builder          a nested type chains through the outer
```

## Nodes

| Label | Emitted for | Props beyond `doc_comment` (javadoc) / `visibility` / `file` / `line` |
|---|---|---|
| `Package` | one per `package` declaration | `name`; javadoc from `package-info.java`. Packages nest by `CONTAINS` where both ends were parsed |
| `Class` / `Interface` / `Enum` / `Record` / `Annotation` | type declarations | `fields`: described `name: type` list (a record's come from its header); `variants` for enum constants |
| `Method` | methods and constructors — overloads share the key: one node, the first (with the docs) wins | `signature` (`void connect(int timeout)`) |
| stand-ins | foreign types/members | `Class` / `Interface` / `Annotation` / `Function` (what the reference proves) + `External` |

## Edges

| Type | Meaning | `line` |
|---|---|---|
| `CONTAINS` | package → type → member, outer type → nested type | declaration site |
| `HAS_METHOD` | interface/annotation → its demanded methods | member |
| `CALLS` | method → callee; `new Foo(…)` is a call to the type; `super.m()` walks the extends chain | call site |
| `IMPORTS` | each of the file's top-level types → what the file imports (Java's import is file-scoped; the type is what a reader navigates by) | import statement |
| `EXTENDS` / `IMPLEMENTS` | class → superclass / interfaces; interface → extended interfaces; generic bases extend what they subscript (`ArrayList<Double>` → `java.util.ArrayList`) | type declaration |
| `ANNOTATED_BY` | type/method → its annotations — on a Spring codebase the annotations *are* the architecture (`@Service`, `@Transactional`, `@GetMapping`); `java.lang`'s own markers (`@Override`, `@Deprecated`, …) are noise and stay out | annotation site |

## Resolution — reading references the way javac does

In order: **already-qualified as written** → **same package** (no import
needed — that is the language's rule) → **single-type imports** → **wildcard
imports**, resolved against what the tree actually holds → **`java.lang`**
(known without an import: `String`, `System`, the usual exceptions).

- A **capitalized receiver** (`Helper.create()`, `com.acme.Helper.create()`)
  is a type reference, written down; a **lowercase receiver** is a value —
  a compiler's business, counted.
- **Inherited calls** — `helper()` with no receiver, and `super.helper()` —
  walk the **in-tree extends chain** to the type that declares the method.
- **Static imports** bind the method name itself.
- `import static`/wildcards/`java.lang` misses that still resolve nowhere
  are counted, never guessed.

Report notes: unresolved value-receiver calls · external calls · merged
declarations.

## Options (`[plugins.java]`)

| Key | Effect |
|---|---|
| `include_source = "true"` | attach `_code` to declarations |

## Build & test

```console
$ cd parser && cargo test             # 18 tests — native, the C grammar compiles for the host too
$ just java-plugin                    # needs wasi-sdk; WASI_SDK env overrides the default path
$ drsg plugin install component/target/wasm32-wasip2/release/drsg_plugin_java.wasm
```

## Known limits

Overload resolution is by name (all overloads share one node); the extends
chain stops at types the tree does not hold; lambdas' bodies attribute
their calls to the enclosing method (where a reader looks); `var` locals
are values, so calls through them are counted.
