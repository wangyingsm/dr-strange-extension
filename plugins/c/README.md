# plugin: c

English · [简体中文](README_CN.md)

Parses C into graph facts. Manifest `c@1`, claims `.c .h` — a header is C,
and its declarations carry the documentation. Built on
[tree-sitter-c](https://github.com/tree-sitter/tree-sitter-c) through the
same wasi-sdk toolchain as the Java plugin. The preprocessor is **recorded,
not expanded**.

## Layout

```
parser/     drsg-c-parser — the language logic, 17 native tests
component/  drsg-plugin-c — Guest impl + rmp-serde partials (needs wasi-sdk to build)
```

## Keys — `{file}::{name}`, C++'s own convention for "this file is the scope"

```
src/common.c                          a File node (path as written — util.c and
                                      util.h are two files; an include names the
                                      header with its extension, so that IS its identity)
src/common.c::readVarInt              a function
src/wire/msgtx.c::main                every tool's main coexists — duplicates are
                                      two facts by construction, never a collision
stdio.h                               a <system> header: an external File
```

The flat-namespace key (bare `main`) was the first design and it was wrong
for repositories: one linked *program* has one definition per name, but a
repository is many programs — 146 multiply-defined names in the first
verification corpus (each tool's `main`, a reference implementation per
variant) silently vanished under first-definition-wins. File-namespace keys
keep them apart; **binding** still follows C's linkage model (below).

## Nodes

| Label | Emitted for | Props beyond `doc_comment` / `visibility` / `file` / `line` |
|---|---|---|
| `File` | each `.c`/`.h` | `includes`: the include list **as resolved keys** — in-tree headers by path, `<system>` headers by name — so each entry is followable; unresolvable ones stay as written |
| `Function` | definitions and (unmerged) prototypes | `signature` (return type + declarator, as written); `visibility: "static"` for file-local |
| `Struct` / `Union` / `Enum` | named types with bodies (`struct foo;` forward-declares, defining nothing) | `fields` (`name: type`) / `variants` (`WARN = 10`) |
| `TypeAlias` | typedefs | underlying type under `signature` (inline struct bodies trimmed to their head) |
| `Const` / `Macro` | `#define` — object-like is a `Const` with its body as written; function-like is a `Macro` with its parameter list | `value` / `signature`. Include guards are bookkeeping and stay out |
| `Var` | globals | type under `signature`, initializer under `value`; `extern` declarations yield to the definition |
| stand-ins | libc by name (`memcpy` tells a reader something real), `<system>` headers | `Function` / `File` + `External` |

Documentation: `/** */`, `/* */`, and runs of `//` above a declaration all
count — C documents in all three.

## Edges

| Type | Meaning | `line` |
|---|---|---|
| `CONTAINS` | file → its declarations; moves to the definition when one merges over a prototype | declaration/definition site |
| `CALLS` | function → callee (bare names — see resolution) | call site |
| `IMPORTS` | file → included file: `#include "x.h"` resolved same-directory first, then an unambiguous tail anywhere in the tree — include paths are build configuration a parser does not have, so ambiguity is counted; `<system>` includes point at external File nodes | the `#include` |

## Resolution — the linker's model, nearest first

1. **The caller's own file** — a static shadows a global of the same name
   in its file (the compiler's rule), and a file's own definition is its
   caller's best answer.
2. **The one global definition**, when the tree holds exactly one.
3. **A unique declaration** when nothing defines the name here — the header
   interface is real even when the body is elsewhere.
4. **libc by name** (a curated list): external, because `memcpy` is worth
   seeing.
5. A name several files define is **counted, never guessed** — which
   definition links is build configuration.

A header's declaration **merges into the definition** wherever one exists:
the definition wins the node — its body, its line, its file — and the
prototype's remaining contribution is its file's `IMPORTS` edge.

`#ifdef` arms and `extern "C"` blocks are walked — both `platform_init`
variants are facts. Function pointers and `ops->read()` are a value's
business: counted.

Report notes: unresolved (pointers, unexpanded macros, absent libraries) ·
calls to multiply-defined names left unbound · libc calls · unresolvable
includes · merged declarations · multiply-defined names kept apart.

## Options (`[plugins.c]`)

| Key | Effect |
|---|---|
| `include_source = "true"` | attach `_code` to definitions |

## Build & test

```console
$ cd parser && cargo test             # 17 tests
$ just c-plugin                       # needs wasi-sdk; WASI_SDK env overrides
$ drsg plugin install component/target/wasm32-wasip2/release/drsg_plugin_c.wasm
```

## Known limits

Macros are not expanded, so macro-defined functions are absent and calls to
function-like macros count as unresolved; a C++ header fed as `.h` parses
mostly to errors and is counted as skipped; K&R-style definitions are
whatever the grammar makes of them.
