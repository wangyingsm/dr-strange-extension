# plugin: ts

English · [简体中文](README_CN.md)

Parses TypeScript **and** JavaScript into graph facts. Manifest `ts@1`,
claims `.ts .tsx .mts .cts .js .jsx .mjs .cjs` — one parser covers the whole
ecosystem, so a mixed repository digests as facts instead of half prose.
Built on [swc](https://swc.rs)'s `swc_ecma_parser` (the parser behind
Next.js), parse-only: no transforms, no checker — what a checker would have
to infer is exactly what this refuses to guess.

## Layout

```
parser/     drsg-ts-parser — the language logic, 30 native tests
component/  drsg-plugin-ts — Guest impl + rmp-serde partials
```

## Keys — logical module identity

```
acme                                  pkg root (index.ts collapses into it)
acme/src/util.fmt                     a declaration
acme/src/api.Client.connect           a class member
@scope/web/src/app.render             scoped packages keep both segments
```

The nearest `package.json` above a file names its package (monorepos resolve
to the nearest manifest, like workspace crates); the module id is the
manifest-relative path without its extension, with `/index` collapsing to
its directory — what `index.ts` *means*, as `mod.rs` means it in Rust. No
manifest → the host's label.

## Nodes

| Label | Emitted for | Props beyond `doc_comment` (JSDoc) / `visibility` / `file` / `line` |
|---|---|---|
| `Module` | each file | `path` (as handed), `imports` (specifiers as written); no file/line of its own |
| `Function` | declarations **and `const f = (…) =>` arrows** — the arrow initializer *is* the function, labelled so | `signature` (source slice, never re-printed), `is_async` |
| `Class` | class declarations | `fields`: described list of `name: type` property declarations, declaration order |
| `Method` | class members (accessibility as written: `private`/`protected`), constructors, and interface method signatures | `signature` |
| `Interface` | interface declarations | property signatures as `fields`; methods become nodes via `HAS_METHOD` |
| `TypeAlias` / `Enum` | type aliases, enums | aliased type under `signature` / `variants` as `Name = value-as-written` |
| `Const` / `Var` | `const` / `let`+`var` with non-function initializers | annotation under `signature`, initializer under `value`, as written |
| stand-ins | other packages and their members | `Package` / `Function` (+ `Interface`/`Class` when a clause proves it) + `External` |

`visibility: "exported"` for exported top-level declarations. A default
export keys under its declared name when it has one (`export default
function boot` → `….boot`, reachable as `default`), else `default`.

## Edges

| Type | Meaning | `line` |
|---|---|---|
| `CONTAINS` | package → module → decl, class → member | declaration site |
| `HAS_METHOD` | interface → its method nodes | member |
| `CALLS` | fn → callee; `new Foo()` counts as a call to the class; a rendered JSX component (`<Foo />`, uppercase) is a call | call site |
| `IMPORTS` | module → module (relative) or → external package (bare specifier) | import statement |
| `IMPLEMENTS` / `EXTENDS` | `class C implements I` / class→class, interface→interface — **syntactic** in TS, so certain where Go's structural check could not be | class/interface declaration |

## Resolution — the certainty line

- **Relative specifiers** resolve against the parsed file set only — no
  filesystem guessing. `./x.js` probes `x.ts`, `x.tsx`, … (ESM writes the
  emitted extension), then `x/index.*`.
- **Named / default / aliased / namespace imports** all bind; `ns.foo()`
  resolves through a namespace import — the one member call whose receiver a
  parser does know. **Re-export chains** (`export { x } from './y'`,
  `export *`) are chased through barrel files, cycle-guarded.
- **CommonJS is read, not just ESM** — the first pure-JS corpus had 524
  `require()` sites and a graph with no imports at all, so: `require` in
  all its forms (whole-module, destructured, `.member`, lazy-in-body) is an
  import wearing a call's syntax; `module.exports` / `exports.foo` are the
  export list (object literals, aliases, module-is-a-function included).
- `this.m()` inside a class body resolves to the class's own method —
  lexical, certain.
- A **member call on a value** is a checker's business: counted, never
  guessed. Bare specifiers name a package's surface (`zod.z`,
  `@babel/traverse`); scoped packages keep two segments.
- TypeScript **declaration merging** across files keeps the first seen,
  counted.

Report notes: unresolved member calls · external calls · import specifiers
naming files the digest never saw (assets, styles) · merged declarations.

## Options (`[plugins.ts]`)

| Key | Effect |
|---|---|
| `include_source = "true"` | attach `_code` to declarations |

## Build & test

```console
$ cd parser && cargo test             # 30 tests
$ just ts-plugin
$ drsg plugin install component/target/wasm32-wasip2/release/drsg_plugin_ts.wasm
```

## Known limits

Decorators are skipped in v1 (one comment in the source says so);
`tsconfig` path aliases (`@/…`) are build configuration a parser does not
have — counted as missed specifiers; dynamic `import()` with non-literal
arguments is opaque.
