# plugin: py

English · [简体中文](README_CN.md)

Parses Python into graph facts. Manifest `py@2`, claims `.py .pyi .pyw`.
Built on [ruff](https://github.com/astral-sh/ruff)'s `ruff_python_parser` —
the parser behind ruff and uv, current with the language (3.12 `type`
statements included) — parse-only: no inference.

## Layout

```
parser/     drsg-py-parser — the language logic, 39 native tests
component/  drsg-plugin-py — Guest impl + rmp-serde partials
```

## Keys — the language's own rule

Module identity follows Python's import system, not a convention of ours:
walking up from a file, **every directory holding an `__init__.py` is a
package, and the first without one is the `sys.path` root** — so a `src/`
layout needs no special case at all, and `__init__.py` names its directory
the way `mod.rs` names its parent.

```
mypkg.core.utils                      src/mypkg/core/utils.py
mypkg.core.utils.parse_row            a function
mypkg.core.utils.Config.load          a method
deploy.main                           a loose script is just its stem
```

## Nodes

| Label | Emitted for | Props beyond `doc_comment` (docstring) / `visibility` / `file` / `line` / `end_line` |
|---|---|---|
| `Module` | each file | `path`, `imports`; a package's docstring lands on its `__init__` module node |
| `Function` / `Method` | `def` / class-body `def` | `signature` as written (`def fetch(url: str, timeout: float = 5.0) -> bytes`), `is_async` |
| `Class` | class declarations | `fields`: described list read **both ways Python writes them** — class-level annotations, then what `__init__` assigns onto `self` (`url: str`, `open`) |
| `Const` / `Var` | module-level assignments — **PEP 8's own rule decides**: ALL_CAPS is a `Const` | annotation under `signature`, value under `value` as written |
| `TypeAlias` | 3.12 `type X = …` | aliased type under `signature` |
| stand-ins | foreign packages/members | `Package` / `Function` / `Class` (a base proves its kind) + `External` |

`visibility: "exported"` follows Python's star-import rule: `__all__` when
the module declares one, else every name not underscore-prefixed.

Every declaration also carries `end_line`: where it stops, so `snippet` reads
exactly the declaration instead of a fixed number of lines after its first,
and a reader sees whether a thing is six lines or two hundred without opening
it. `line` still points at the **name**, so documentation above a declaration
cannot move where the graph says it starts.

## Edges

| Type | Meaning | `line` |
|---|---|---|
| `CONTAINS` | package → module (both ends parsed) → decl, class → method | declaration site |
| `CALLS` | function → callee; **a decorator is a call written down** (`@app.route` names the router). Carries `concurrent` when a site departs from waiting: `scheduled` (handed to `create_task`, `gather`, `to_thread`) or `unawaited` (a coroutine called as a statement, which nothing here waits for) | call / decorator site |
| `IMPORTS` | module → module (absolute target; relative imports resolved at parse, where the module is known) | import statement |
| `REFERENCES` | function → a function it **passes as a value** rather than calls; carries `concurrent: scheduled` when a scheduler was handed the callable itself | the argument |
| `EXTENDS` | class → base — syntax, with a subscripted base extending what it subscripts (`Generic[T]` → `typing.Generic`) | class line |
| `USES_TYPE` | declaration → a type it is **annotated with**, `role` on the edge (`field`, `param`, `return`). Subscripts, unions and string forward references are walked — `list[Job]` is a dependency on `Job`. **Only where an annotation was written**: an unannotated parameter states no type, and its silence is not the absence of a dependency | — |

## Routes

An HTTP route is the one reference this plugin cannot turn into an edge: the
handler or the caller is in another language, often another repository, and
between the two sides there is only a string. So the plugin records what the
source *states* and leaves the match to drsg's `wire`, which computes it per
call and never stores it.

On the symbol that already exists:

| Property | Set on | Shape |
|---|---|---|
| `serves` | the decorated function, or the view a URL conf names | `GET /api/orders/{}` — the verb, and the path with every variable segment reduced to `{}` |
| `requests` | the function making the call | the same shape; `*` where the source states no verb |
| `serves_as_written` / `requests_as_written` | both | the path exactly as the source wrote it, before normalising |

**Declarations** come in both of Python's idioms. A decorator carrying a path
records on the function below it — `@app.get("/x")`, `@router.post(...)`,
`@app.route(...)` — with the verb read from the decorator's own name. And a
URL conf records on the **view it names**: `path("orders/<int:pk>", views.one)`
in a `urlpatterns` list puts the route on `views.one`, which is Django's shape
and needs no decorator at all. A registry that is no router declares nothing.

**Prefixes resolve further here than elsewhere**, because Django and FastAPI
both state them away from the route:

- `APIRouter(prefix="/api")` reaches every path stated through that router.
- A prefix written as a **name** resolves across modules — `API_PREFIX` imported
  from `app.config` is followed to its literal.
- An `include()`d URL conf sits under the path that included it, and a router
  included under a prefix is under **both**.

**Requests** are any call with a path-shaped string argument, whatever the
callee is called — `requests`, `httpx`, `urllib`, and a project-local wrapper
alike. An f-string keeps its hole (`f"/orders/{pk}"` → `/orders/{}`), and an
absolute URL or a query string reduces to the path. A `mock.patch` target is a
qualified name, not a path, so it is not mistaken for one.

**A view the tree cannot resolve keeps its route.** If `path("health/",
elsewhere.check)` names something this plugin cannot bind, the route is
recorded on the URL conf's own module rather than dropped — a fact that is
still true, attached to the only symbol that is certain.

## Resolution — the certainty line

- `from pkg.mod import name` binds through the module set, aliases included;
  `import pkg.util` binds the root and **dotted chains walk modules step by
  step**, stopping the moment a step lands on a value.
- **Concurrency is a fact about the call site, not the declaration.**
  `is_async` says a function may suspend; only the call says whether anyone
  waited, and `await f()`, `create_task(f())` and a bare `f()` are three
  different programs. Only the departures are recorded — inside an async
  body an `await` is the expectation. One caller reaching one callee several
  ways folds to one edge carrying the **union** of what it does, so an
  awaited site cannot swallow a scheduled one by being written first.
  `unawaited` is claimed only of a coroutine: `log(x)` is a statement whose
  result is discarded too, and there was nothing there to await.
- A **local typed by an external constructor** resolves its methods to
  external stand-ins: `q = asyncio.Queue()` makes `q.put(...)` a call on
  `asyncio.Queue.put`, and an annotated parameter (`q: asyncio.Queue`) is
  typed the same way — which is what joins a consumer to the queue a
  producer fills. The node is the **type**, not the instance: two queues in
  one program share it. An unannotated parameter states no type and stays in
  the ledger, which is what Python's own checkers say of it.
- **Relative imports are package geometry** (`from ..a import helper`),
  resolved where the file sits.
- **Star imports** reach the target's export surface (`__all__`, else
  public names).
- `self.m()` / `cls.m()` resolve lexically to the class's own methods — the
  one receiver a parser knows.
- A method or attribute on a value is a checker's business: counted.
- Builtins (`print`, `len`, `ValueError`, …) are nobody's edge.
- One judgement call, commented where it lives: a plain value assignment
  **yields to an import binding of the same name**, because
  `try: from x import y / except ImportError: y = None` is the fallback
  idiom and the import is the primary.
- `if TYPE_CHECKING:` imports and `try:` fallbacks are module structure —
  walked, not skipped. Overload stubs share the name; the first (with the
  docs) wins. A `.pyi` beside its `.py` merges, counted.

Report notes: unresolved member/attribute calls · external calls · merged
declarations.

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
| `base-class` | `definitive` | a `unittest.TestCase` subclass (also `IsolatedAsyncioTestCase`, `FunctionTestCase`), through whatever name the file imported it under |
| `filename` | `strong` | `test_*.py`, `*_test.py`, `conftest.py` |

Ranked, so the strongest evidence is what remains: a filename is what the
runners collect by *default* and `pyproject.toml` can move it, while a
`TestCase` subclass is unittest's own definition of a test — so it overwrites
the filename flag on the class it covers and on that class's methods. A bare
`TestCase` base counts only when an import in the same file says it came from
`unittest`; a local class of that name is somebody else's `TestCase`, and
flagging its subclasses would call production code a test.

## Options (`[plugins.py]`)

| Key | Effect |
|---|---|
| `include_source = "true"` | attach `_code` to declarations |

## Build & test

```console
$ cd parser && cargo test             # 39 tests
$ just py-plugin
$ drsg plugin install component/target/wasm32-wasip2/release/drsg_plugin_py.wasm
```

## Known limits

Namespace packages (no `__init__.py`) break the chain at the root by
design — that directory *is* a `sys.path` root to the rule; dynamic
attribute access and monkey-patching are invisible; decorators that
manufacture functions (`@functools.wraps` chains) record the decorator
call, not the manufactured surface.
