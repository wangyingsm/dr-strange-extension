# plugin: csharp

English · [简体中文](README_CN.md)

Parses C# into graph facts. Manifest `csharp@1`, claims `.cs`. Built on
[tree-sitter-c-sharp](https://github.com/tree-sitter/tree-sitter-c-sharp) —
no pure-Rust C# frontend exists, and the grammar every editor already trusts
is a better foundation than one we would have to earn trust for. The C
runtime and grammar compile to `wasm32-wasip2` under wasi-sdk's clang, the
same toolchain the Java, Kotlin and C plugins use.

## Layout

```
parser/     drsg-csharp-parser — the language logic, 60 native tests
component/  drsg-plugin-csharp — Guest impl + rmp-serde partials (needs wasi-sdk to build)
```

## Keys

C#'s own qualified names. **A file is not a node**: the type is the unit and
the namespace is the container — and a `partial` type is one declaration
written in several files, which reaches one node holding every half's
members.

```
Acme.Core                             a namespace
Acme.Core.Engine                      a type
Acme.Core.Engine.Start                a member
Acme.Core.Engine.Engine               a constructor, under the type's own name
Acme.Core.Engine.Builder              a nested type chains through the outer
```

## Nodes

| Label | Emitted for | Props beyond `doc_comment` (`///`) / `visibility` / `file` / `line` / `end_line` |
|---|---|---|
| `Namespace` | one per `namespace` declaration, braced or file-scoped | `name`. Namespaces nest by `CONTAINS` where both ends were declared |
| `Class` / `Interface` / `Struct` / `Record` / `Enum` | type declarations | `fields`: described `name: type` list (a record's positional parameters are its properties and count here); `variants` for enum members; `modifiers` as written (`partial`, `abstract`, `sealed`, `static`, …) |
| `Delegate` | `delegate` declarations | `signature` (`Job Handler(Job input)`) |
| `Method` | methods, operators and constructors (under the type's own name) — overloads share the key: one node, the first (with the docs) wins | `signature` (`bool Connect(int timeout)`) |
| `Property` | properties and indexers — a C# property carries a type and often an accessor with code in it, so it is a declaration a reader navigates to | `signature` (`Helper Dep`) |
| stand-ins | foreign types/members | `Class` / `Interface` / `Annotation` / `Function` (what the reference proves) + `External` |

A **field** carries only a value, so it is a line in its type's `fields` and
not a node; a **property** can hold code, so it is both.

Every declaration also carries `end_line`: where it stops, so `snippet`
reads exactly the declaration instead of a fixed number of lines after its
first, and a reader sees whether a thing is six lines or two hundred without
opening it. `line` still points at the **name**, so documentation above a
declaration cannot move where the graph says it starts. XML documentation is
kept as its text, with the markup (`<summary>`, `<param>`) dropped — that is
a doc tool's structure, and the words between the tags are what a reader
wanted.

## Edges

| Type | Meaning | `line` |
|---|---|---|
| `CONTAINS` | namespace → type → member, outer type → nested type | declaration site |
| `HAS_METHOD` | interface → its demanded members | member |
| `CALLS` | member → callee; `new Foo(…)` is a call to the type, and `Foo f = new()` names the type it was declared as; `base.M()` walks the base chain; `x?.M()` reaches the same member as `x.M()`, because whether it runs is not a fact about the graph | call site |
| `IMPORTS` | each of the file's top-level types → what the file `using`s (C#'s `using` is file-scoped; the type is what a reader navigates by) | using directive |
| `EXTENDS` / `IMPLEMENTS` | a type → its base types. C# writes the base class first and nowhere else, so where the tree holds the target its own declaration decides, and where it does not, .NET's own naming convention does: `IRepo` is an interface, `ControllerBase` is a class | type declaration |
| `ANNOTATED_BY` | declaration → its attributes — on an ASP.NET codebase the attributes *are* the architecture (`[ApiController]`, `[Route]`, `[Authorize]`), and the `Attribute` suffix C# lets the source omit is tried too, so `[Marker]` reaches `MarkerAttribute`; compiler and debugger markers (`[CompilerGenerated]`, `[DebuggerStepThrough]`, …) are noise and stay out. `args` on the edge carries the arguments **as written** (`[Route("api/[controller]")]`), uninterpreted | attribute site |
| `USES_TYPE` | declaration → a type it is **typed by**, `role` on the edge (`field`, `property`, `param`, `return`). Generic arguments are walked — `List<Job>` is a dependency on `Job` — and only types this tree declares get an edge. A predefined type (`int`, `string`) is the language's own, never this tree's. A dependency the container wires also carries `injected` and `_injected_confidence` (below) | — |

## Resolution — reading references the way the compiler does

In order: **already-qualified as written** → the **enclosing namespace and
then outward through its ancestors** (which is why `Acme.Core.Deep` sees
`Acme.Core`'s types) → **`using` aliases** → **`using` namespaces** →
the compilation's **`global using`** namespaces → **`System`** (the types
every file uses without importing them in its head) → **the name as
written**.

That last step is where C# differs from Java, and deliberately. A C# file
imports whole namespaces, so nothing in it says which of them a bare
`ControllerBase` came from. Recording the name the source wrote keeps the
reference — as an external stand-in, with no namespace on it — where
inventing a namespace for it would be a guess and dropping it would lose
every base class, attribute and static call into the framework.

- A **PascalCase receiver** (`Helper.Create()`, `Acme.Util.Deep.Thing()`) is
  a type reference, written down. A **camelCase or `_`-prefixed receiver**
  is a value, typed where the source states its type — a declared local, a
  parameter, a field, a property, or a typed `foreach` variable — and
  counted unresolved where only a compiler would know. `var h = new
  Helper()` states its type through the constructor it calls.
- **Inherited calls** — `Helper()` with no receiver, and `base.Helper()` —
  walk the in-tree base chain to the type that declares the member.
- `using static Acme.Util.Helper` puts that type's members in scope under
  their own names, and a bare call reaches them.
- A **`partial` type** is one type: a call from one half into the other
  resolves, because the halves' member surfaces are merged.
- Misses that resolve nowhere are counted, never guessed.

Report notes: unresolved value-receiver calls · external calls · merged
declarations · service registrations · injection points · services wired to
an implementation · services left ambiguous.

## Dependency injection — the container's own registry, read from the source

Under dependency injection the type at a call site is the **service**, never
the implementation: `_orders.Find(id)` names an interface, and which class
answers is the container's decision. A graph that stops there says every
implementation is dead code and ends every call chain at the interface.

In .NET that decision is not configuration a parser has to guess at — it is
a **call**, with both types written down in it:

```csharp
services.AddScoped<IOrderService, DefaultOrderService>();
```

So the plugin reads it. `AddSingleton`, `AddScoped`, `AddTransient`, their
`TryAdd` and `AddKeyed` spellings, and the one-type self-registration
(`AddScoped<Engine>()`) are the four words the container's own API is built
from.

**An injection point** is a constructor parameter of a type the container
builds — constructor injection is the only kind ASP.NET Core does — marked
on the `USES_TYPE` edge as ranked evidence, the way `test_flag` is:

| `injected` | `_injected_confidence` | From |
|---|---|---|
| `attribute` | `definitive` | `[FromServices]` / `[FromKeyedServices]` on the parameter |
| `constructor` | `strong` | a **registered** type's constructor, which the container calls — a C# 12 primary constructor included |

A constructor of a type nothing registers is not an injection point: the
container never calls it, and whoever does chose the argument.

**A call through an injected service** gets a second `CALLS` edge, to the
implementation's member, stamped `_resolved_by` with the rule that picked
it, `_confidence`, and `_via` naming the service member the source wrote.
The service's own edge stays beside it: one is what the source says, the
other what the container does.

| `_resolved_by` | `_confidence` | The rule |
|---|---|---|
| `di-keyed` | `high` | the injection point's `[FromKeyedServices("fast")]` names the key a registration was made under |
| `di-registration` | `high` | one unkeyed registration answers the service and the member |
| `di-registration-collection` | `medium` | the type asks for an `IEnumerable` / `IList` / `ICollection` / `IReadOnlyList` of the service, so the container hands it **every** registration — which one runs at a given moment is nowhere written |

The edge targets the type that **declares** the member, which may be a base
class: that is where the code a reader wants is.

Nothing is emitted when several registrations answer and nothing chooses
between them — the report says how many. .NET resolves the **last**
registration to run, and the order registrations run in is not written in
any one file, so a parser that named one would be guessing. Nothing is
emitted for a service value that is not an injection point either: whoever
built it chose the implementation. A keyed registration answers only the key
it was registered under. Only an **interface** is dispatched through; a
concrete type's method has a body, so the recorded edge already points at
code that runs.

## Routes

An HTTP route is the one reference this plugin cannot turn into an edge: the
handler or the caller is in another language, often another repository, and
between the two sides there is only a string. So the plugin records what the
source *states* and leaves the match to drsg's `wire`, which computes it per
call and never stores it.

On the symbol that already exists:

| Property | Set on | Shape |
|---|---|---|
| `serves` | the handler, and the type whose route it composes with | `GET /api/Orders/{}` — the verb, and the path with every variable segment reduced to `{}` |
| `requests` | the member making the call | the same shape; `*` where the source states no verb |
| `serves_as_written` / `requests_as_written` | both | the path exactly as the source wrote it, before normalising |

**Declarations** come from the attributes the plugin already captures with
their arguments as written. A type-level `[Route("api/[controller]")]`
composes with a method's `[HttpGet("{id}")]`, and the verb is read from the
attribute's own name — `HttpGet`, `HttpPost` — wherever the framework puts
the verb word. **ASP.NET's route tokens are filled in the way ASP.NET fills
them**: `[controller]` is the type's name without its `Controller` suffix
and `[action]` is the method's. A mapping that states **no** verb records
`*`, which matches every verb on that path and says so in the answer.

**A path written without a leading slash is still a path**, which is how
ASP.NET writes every one of them — so a slashless literal counts where the
attribute or the callee has already named a verb or a route. An
`"application/json"` beside a `[Produces]` names neither and stays prose.

**Requests** are any call with a path-shaped string argument, whatever the
callee is called — `HttpClient`, Refit, and a project-local wrapper alike.
An absolute URL or a query string reduces to the path. An interpolated
string is not a literal and states nothing.

**A declarative client declares nothing.** A Refit interface looks exactly
like a controller and means the opposite: it *asks for* the path, it does
not serve it. Recording `serves` there would invent a handler that does not
exist, so the plugin records neither side for now rather than the wrong one.

## Test code

Test code answers "who calls this" differently from production code, and a
reader that cannot tell them apart counts a test as a user. Two properties
say so: `test_flag` carries **what the evidence was**, and
`_test_flag_confidence` carries **how much that evidence is worth** — the
`_` keeps the second out of the compact renderers and out of embeddings,
where it would be noise; `cypher`, `get_node` and `export` still return it,
which is where a reader weighing the flag is looking. Nothing is flagged
when nothing is written down.

| `test_flag` | `_test_flag_confidence` | From |
|---|---|---|
| `attribute` | `definitive` | an xUnit / NUnit / MSTest attribute — `[Fact]`, `[Theory]`, `[Test]`, `[TestCase]`, `[TestMethod]`, and the setup/teardown hooks, with or without the `Attribute` suffix |
| `project-layout` | `strong` | under a test project directory — an `Acme.Tests` / `Acme.Test` project, or a `tests/` tree |

Ranked, so the strongest evidence is what remains: the `.csproj` that would
settle it names no `.cs` file, so the directory is the layout evidence this
plugin has, and the attribute is xUnit's own definition of a test. It
reaches the type holding the method as well as the method — a class with a
`[Fact]` in it is a test class.

## Options (`[plugins.csharp]`)

| Key | Effect |
|---|---|
| `include_source = "true"` | attach `_code` to declarations |

## Build & test

```console
$ cd parser && cargo test             # 60 tests — native, the C grammar compiles for the host too
$ just csharp-plugin                  # needs wasi-sdk; WASI_SDK env overrides the default path
$ drsg plugin install component/target/wasm32-wasip2/release/drsg_plugin_csharp.wasm
```

## Known limits

Overload resolution is by name (all overloads share one node); the base
chain stops at types the tree does not hold; lambdas' bodies attribute their
calls to the enclosing member (where a reader looks), so
`handlers.ForEach(h => h.Handle())` stays unresolved where `foreach
(IHandler h in handlers)` resolves — a lambda parameter has no written type.
Extension methods are resolved as the static methods they are declared as,
not at the call sites that use them as members. A file declaring several
namespaces resolves its references against all of them, which is one scope
more than the compiler would use. Generic type parameters are names like any
other, so a `T` that happens to match a declared type resolves to it.

The container's own edges: a registration made through a factory lambda
(`AddScoped<IRepo>(sp => new JpaRepo(…))`), by `typeof` pair, by assembly
scanning (Scrutor) or in a third-party container's own DSL is not read — the
type is not written in the call's type arguments. Decorators and
`IOptions<T>` resolve no further than the wrapper. A constructor parameter
binds to the member of the **same name** under .NET's own spelling — `repo`
fills `_repo` or `Repo` — which is the convention every codebase follows and
none guarantees; where the names differ beyond that, the type alone decides.

Routes: a path base, a reverse proxy's rewrite and `MapControllers`'s own
conventions are deployment and startup configuration, so a declaration is
recorded as the source states it. Minimal APIs (`app.MapGet("/x", …)`) are
recorded as **requests** rather than declarations, because they are calls
with a path-shaped argument and nothing in the call says it is a server;
that is the next thing to add.
