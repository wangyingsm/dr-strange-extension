# plugin: py

Parses Python into graph facts. Manifest `py@1`, claims `.py .pyi .pyw`.
Built on [ruff](https://github.com/astral-sh/ruff)'s `ruff_python_parser` —
the parser behind ruff and uv, current with the language (3.12 `type`
statements included) — parse-only: no inference.

## Layout

```
parser/     drsg-py-parser — the language logic, 21 native tests
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

| Label | Emitted for | Props beyond `doc_comment` (docstring) / `visibility` / `file` / `line` |
|---|---|---|
| `Module` | each file | `path`, `imports`; a package's docstring lands on its `__init__` module node |
| `Function` / `Method` | `def` / class-body `def` | `signature` as written (`def fetch(url: str, timeout: float = 5.0) -> bytes`), `is_async` |
| `Class` | class declarations | `fields`: described list read **both ways Python writes them** — class-level annotations, then what `__init__` assigns onto `self` (`url: str`, `open`) |
| `Const` / `Var` | module-level assignments — **PEP 8's own rule decides**: ALL_CAPS is a `Const` | annotation under `signature`, value under `value` as written |
| `TypeAlias` | 3.12 `type X = …` | aliased type under `signature` |
| stand-ins | foreign packages/members | `Package` / `Function` / `Class` (a base proves its kind) + `External` |

`visibility: "exported"` follows Python's star-import rule: `__all__` when
the module declares one, else every name not underscore-prefixed.

## Edges

| Type | Meaning | `line` |
|---|---|---|
| `CONTAINS` | package → module (both ends parsed) → decl, class → method | declaration site |
| `CALLS` | function → callee; **a decorator is a call written down** (`@app.route` names the router) | call / decorator site |
| `IMPORTS` | module → module (absolute target; relative imports resolved at parse, where the module is known) | import statement |
| `EXTENDS` | class → base — syntax, with a subscripted base extending what it subscripts (`Generic[T]` → `typing.Generic`) | class line |

## Resolution — the certainty line

- `from pkg.mod import name` binds through the module set, aliases included;
  `import pkg.util` binds the root and **dotted chains walk modules step by
  step**, stopping the moment a step lands on a value.
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

## Options (`[plugins.py]`)

| Key | Effect |
|---|---|
| `include_source = "true"` | attach `_code` to declarations |

## Build & test

```console
$ cd parser && cargo test             # 21 tests
$ just py-plugin
$ drsg plugin install component/target/wasm32-wasip2/release/drsg_plugin_py.wasm
```

## Known limits

Namespace packages (no `__init__.py`) break the chain at the root by
design — that directory *is* a `sys.path` root to the rule; dynamic
attribute access and monkey-patching are invisible; decorators that
manufacture functions (`@functools.wraps` chains) record the decorator
call, not the manufactured surface.
