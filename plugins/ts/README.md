# plugin: ts

English · [简体中文](README_CN.md)

Parses TypeScript **and** JavaScript into graph facts. Manifest `ts@2`,
claims `.ts .tsx .mts .cts .js .jsx .mjs .cjs` — one parser covers the whole
ecosystem, so a mixed repository digests as facts instead of half prose.
Built on [swc](https://swc.rs)'s `swc_ecma_parser` (the parser behind
Next.js), parse-only: no transforms, no checker — what a checker would have
to infer is exactly what this refuses to guess.

## Layout

```
parser/     drsg-ts-parser — the language logic, 43 native tests
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

| Label | Emitted for | Props beyond `doc_comment` (JSDoc) / `visibility` / `file` / `line` / `end_line` |
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

Every declaration also carries `end_line`: where it stops, so `snippet` reads
exactly the declaration instead of a fixed number of lines after its first,
and a reader sees whether a thing is six lines or two hundred without opening
it. `line` still points at the **name**, so documentation above a declaration
cannot move where the graph says it starts.

## Edges

| Type | Meaning | `line` |
|---|---|---|
| `CONTAINS` | package → module → decl, class → member | declaration site |
| `HAS_METHOD` | interface → its method nodes | member |
| `CALLS` | fn → callee; `new Foo()` counts as a call to the class; a rendered JSX component (`<Foo />`, uppercase) is a call. Carries `concurrent` when a site departs from waiting: `scheduled` (handed to `Promise.all`/`race`/`any`, `setTimeout`, `queueMicrotask`) or `unawaited` (a promise called as a statement, which nothing here awaits and no `.then` follows) | call site |
| `IMPORTS` | module → module (relative) or → external package (bare specifier) | import statement |
| `REFERENCES` | fn → a function it **passes as a value** rather than calls; carries `concurrent: scheduled` when a scheduler was handed the function itself | the argument |
| `IMPLEMENTS` / `EXTENDS` | `class C implements I` / class→class, interface→interface — **syntactic** in TS, so certain where Go's structural check could not be | class/interface declaration |
| `USES_TYPE` | declaration → a type it is **typed by**, `role` on the edge (`field`, `param`, `return`). Array elements, unions and generic arguments are walked — `Job[]` is a dependency on `Job`. **Only where an annotation was written**: an unannotated parameter states no type, and its silence is not the absence of a dependency | — |

## Routes

An HTTP route is the one reference this plugin cannot turn into an edge: the
handler is in another language, often another repository, and between the two
sides there is only a string. So the plugin records what the source *states*
and leaves the match to drsg's `wire`, which computes it per call and never
stores it.

On the symbol that already exists:

| Property | Set on | Shape |
|---|---|---|
| `serves` | the handler a registration or a decorator names | `GET /api/orders/{}` — the verb, and the path with every variable segment reduced to `{}` |
| `requests` | the function making the call | the same shape; `*` where the source states no verb |
| `serves_as_written` / `requests_as_written` | both | the path exactly as the source wrote it, before normalising |

**Declarations.** A registration call whose callee names the verb —
`app.get('/x', h)`, `router.post(...)`, a fluent `.get(...).post(...)` chain —
records on the handler it names, an arrow written inline included. A decorated
class records too: `@Controller('/api')` on the class composes with `@Get(':id')`
on the method, and a class decorator whose only argument is path-shaped needs no
recognised name. A decorator that declares no route declares nothing.

**Requests.** Any call with a path-shaped string argument, whatever the callee
is called — which is the only way a project-local `request('/x')` wrapper is
ever visible. A template literal keeps its hole (`` `/orders/${id}` `` →
`/orders/{}`), and an absolute URL or a query string reduces to the path. A
client's own base reaches the paths it asks for: `axios.create({ baseURL })`
and a module-level literal const both join.

**Mounts are not routes.** `app.use('/api', router)` gives a router a prefix
that belongs to every route hung on it and to none of them yet, so the mounting
function serves nothing and asks for nothing. The prefix reaches the routes
instead — through two levels of mounting as well — and a router nobody mounts
states its paths whole.

## Resolution — the certainty line

- **Concurrency is a fact about the call site, not the declaration.**
  `is_async` says a function returns a promise; only the call says whether
  anyone awaited it, and `await f()`, `Promise.all([f()])` and a bare `f()`
  are three different programs. Only the departures are recorded — inside an
  async body an `await` is the expectation. One caller reaching one callee
  several ways folds to one edge carrying the **union** of what it does, so
  an awaited site cannot swallow a floating one by being written first.
  `unawaited` is claimed only of an async callee: `log(x)` is a statement
  whose result is discarded too, and there was nothing there to await; a
  `.then` handles the promise, so the call it follows is not floating.

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
| `filename` | `strong` | `*.test.*`, `*.spec.*`, or any path under `__tests__/` |

`strong` and not `definitive` because a jest/vitest config can redraw the
patterns, and a parser does not read build configuration. `describe`/`it`
would be the other candidate and are worse evidence: the runners inject them
as globals, so they arrive as unresolved calls, and a module that merely
*defines* a function called `it` would read the same.

## Options (`[plugins.ts]`)

| Key | Effect |
|---|---|
| `include_source = "true"` | attach `_code` to declarations |

## Build & test

```console
$ cd parser && cargo test             # 43 tests
$ just ts-plugin
$ drsg plugin install component/target/wasm32-wasip2/release/drsg_plugin_ts.wasm
```

## Known limits

A decorator is read for the routes it declares and is not otherwise recorded
as a fact — its name and arguments reach no node, unlike the java plugin's
annotations. `tsconfig` path aliases (`@/…`) are build configuration a parser
does not have — counted as missed specifiers; dynamic `import()` with
non-literal arguments is opaque. A base assembled at runtime
(`process.env.API_URL`) is not a literal, so the paths under it stay relative
and match nothing; a router handed to another function to mount loses its
prefix.
