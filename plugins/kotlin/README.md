# plugin: kotlin

English · [简体中文](README_CN.md)

Parses Kotlin into graph facts. Manifest `kotlin@1`, claims `.kt` and
`.kts`. Built on
[tree-sitter-kotlin-ng](https://github.com/tree-sitter-grammars/tree-sitter-kotlin)
— no pure-Rust Kotlin frontend exists, and the grammar an editor already
trusts is a better foundation than one we would have to earn trust for. The
C runtime and grammar compile to `wasm32-wasip2` under wasi-sdk's clang, the
same toolchain the Java and C plugins use.

Answers [issue #45](https://github.com/wangyingsm/dr-strange/issues/45).

## Layout

```
parser/     drsg-kotlin-parser — the language logic, 56 native tests
component/  drsg-plugin-kotlin — Guest impl + rmp-serde partials (needs wasi-sdk to build)
```

## Keys

Kotlin's own qualified names. **A file is not a node**: the declaration is
the unit and the package is the container — which is why a top-level
function sits directly under its package rather than inside the `FileKt`
class the JVM invents for it.

```
com.acme.core                         a package
com.acme.core.Engine                  a type
com.acme.core.Engine.start            a member
com.acme.core.Engine.Companion        a companion object, under the name Kotlin gives it
com.acme.core.Engine.Builder          a nested type chains through the outer
com.acme.core.helper                  a top-level function
```

## Nodes

| Label | Emitted for | Props beyond `doc_comment` (KDoc) / `visibility` / `file` / `line` / `end_line` |
|---|---|---|
| `Package` | one per `package` declaration | `name`. Packages nest by `CONTAINS` where both ends were parsed |
| `Class` / `Interface` / `Enum` / `Annotation` / `Object` | type declarations — an `object`, a `companion object` and an anonymous one are all `Object` | `fields`: described `name: type` list (a primary constructor's `val`/`var` parameters are properties and count here); `variants` for enum entries; `modifiers` as written (`data`, `sealed`, `value`, `abstract`, …) |
| `TypeAlias` | `typealias` | `signature` (`Jobs = List<Job>`) |
| `Method` | member functions, secondary constructors (under the type's own name) and `init` blocks — overloads share the key: one node, the first (with the docs) wins | `signature` (`connect(timeout: Int): Boolean`), `receiver` for an extension function |
| `Function` | top-level functions | the same |
| `Property` | a type's properties — a Kotlin property carries a type and often a getter with code in it, so it is a declaration a reader navigates to | `signature` (`helper: Helper`) |
| `Const` / `Var` | top-level `val` / `var` | `signature` |
| stand-ins | foreign types/members | `Class` / `Interface` / `Annotation` / `Function` (what the reference proves) + `External` |

Every declaration also carries `end_line`: where it stops, so `snippet`
reads exactly the declaration instead of a fixed number of lines after its
first, and a reader sees whether a thing is six lines or two hundred without
opening it. `line` still points at the **name**, so documentation above a
declaration cannot move where the graph says it starts.

## Edges

| Type | Meaning | `line` |
|---|---|---|
| `CONTAINS` | package → declaration → member, outer type → nested type, a type → its properties and its companion object | declaration site |
| `HAS_METHOD` | interface → its demanded functions | member |
| `CALLS` | function → callee; `Foo(…)` is a call to the type, because that is how Kotlin spells construction; `super.m()` walks the one supertype a constructor invocation names | call site |
| `IMPORTS` | each of the file's top-level declarations → what the file imports (Kotlin's import is file-scoped; the declaration is what a reader navigates by) | import statement |
| `EXTENDS` / `IMPLEMENTS` | a type → its supertypes. Kotlin's syntax only says whether the supertype was **constructed** — a superclass must be, an interface cannot be — so `: Base()` extends and `: Runner` implements, and where the tree holds the target its own declaration decides | type declaration |
| `ANNOTATED_BY` | declaration → its annotations — on a Spring codebase the annotations *are* the architecture (`@Service`, `@Transactional`, `@GetMapping`), and on Android they carry the framework (`@Composable`, `@HiltViewModel`, `@Entity`); compiler directives and JVM interop markers (`@Override`, `@Suppress`, `@JvmStatic`, …) are noise and stay out. `args` on the edge carries the arguments **as written** (`@Qualifier("fast")`, `@GetMapping("/orders")`), uninterpreted: what they mean is the framework's business | annotation site |
| `USES_TYPE` | declaration → a type it is **typed by**, `role` on the edge (`property`, `param`, `return`, `receiver`, `alias`). Generic arguments are walked — `List<Job>` is a dependency on `Job` — and only types this tree declares get an edge. A dependency a container wires also carries `injected` and `_injected_confidence` (below) | — |

## Resolution — reading references the way the compiler does

In order: **already-qualified as written** → **same package** (no import
needed) → **imports**, the alias where one is written → **star imports**,
resolved against what the tree actually holds → **Kotlin's default imports**
(`String`, `List`, the usual exceptions — known without an import).

- A **capitalized receiver** (`Helper.create()`, `com.acme.Helper.create()`)
  is a type reference, written down. A **lowercase receiver** is a value,
  typed where the source states its type — a declared local, a parameter, a
  property, or a typed `for` variable — and counted unresolved where only a
  compiler would know. `val h = Helper()` states its type through the
  constructor it calls.
- A **companion object** answers a call through its type's name:
  `Engine.create()` reaches `Engine.Companion.create`, which is where Kotlin
  puts what Java writes `static`. A named companion keeps its name.
- **Inherited calls** — `helper()` with no receiver, and `super.helper()` —
  walk the in-tree supertypes to the type that declares the function:
  superclasses and interfaces for a bare call, so an interface's default
  implementation answers one, and the **constructed** supertype alone for
  `super.helper()`.
- A **bare call** also reaches a **top-level function of the same package**,
  and a **member import** (`import a.b.spin`) binds the name itself.
- Misses that resolve nowhere are counted, never guessed.

Report notes: unresolved value-receiver calls · external calls · merged
declarations · injection points · ports wired to a bean · ports left
ambiguous.

## Spring injection — the container's own wiring, read from the source

Under dependency injection the type at a call site is the **port**, never
the implementation: `orderService.place(id)` names an interface, and which
bean answers is the container's decision. A graph that stops there says
every `@Service` is dead code and ends every call chain at the interface.

That decision is written down, so it is read rather than guessed — and in
Kotlin it is written in the **primary constructor**, which is how nearly
every Kotlin bean declares what it needs.

**An injection point** is a dependency the container supplies, marked on the
`USES_TYPE` edge as ranked evidence, the way `test_flag` is:

| `injected` | `_injected_confidence` | From |
|---|---|---|
| `annotation` | `definitive` | `@Autowired` / `@Inject` / `@Resource` on the property or the parameter |
| `constructor` | `strong` | a stereotyped bean's **only** constructor, which Spring injects unannotated — in Kotlin the primary constructor, `val` parameters and all |

**A bean** is a type carrying `@Service`, `@Component`, `@Repository`,
`@Controller`, `@RestController`, `@Configuration` or either
`ControllerAdvice`, or the declared return type of a `@Bean` function. Its
name is the annotation's own argument, else the simple name decapitalized by
Java's `Introspector` rule, where two leading capitals stay as written
(`URLFetcher`).

**A call through an injected port** gets a second `CALLS` edge, to the
bean's member, stamped `_resolved_by` with the rule that picked it,
`_confidence`, and `_via` naming the port member the source wrote. The
port's own edge stays beside it: one is what the source says, the other what
the container does.

| `_resolved_by` | `_confidence` | The rule |
|---|---|---|
| `spring-qualifier` | `high` | the injection point's `@Qualifier` / `@Resource` names a bean |
| `spring-bean` | `high` | one bean implements the port and answers the member |
| `spring-primary` | `high` | several do, and exactly one is `@Primary` |
| `spring-bean-collection` | `medium` | the type asks for a `List` / `Set` / `Collection` / `Iterable` / `Map` / `Sequence` of the port, so Spring hands it **every** bean — which one runs at a given moment is nowhere written |

The edge targets the type that **declares** the member, which may be an
abstract base: that is where the code a reader wants is.

Nothing is emitted when several beans implement the port and no `@Primary`
or `@Qualifier` chooses — the report says how many, and that is the case
Spring would fail to start on. Nothing is emitted for a port value that is
not an injection point either: whoever built it chose the implementation.
Only an **interface** is dispatched through; an abstract class's member has
a body, so the recorded edge already points at code that runs.

## Routes

An HTTP route is the one reference this plugin cannot turn into an edge: the
handler or the caller is in another language, often another repository, and
between the two sides there is only a string. So the plugin records what the
source *states* and leaves the match to drsg's `wire`, which computes it per
call and never stores it.

On the symbol that already exists:

| Property | Set on | Shape |
|---|---|---|
| `serves` | the handler, and the class whose mapping it composes with | `GET /api/orders/{}` — the verb, and the path with every variable segment reduced to `{}` |
| `requests` | the function making the call | the same shape; `*` where the source states no verb |
| `serves_as_written` / `requests_as_written` | both | the path exactly as the source wrote it, before normalising |

**Declarations** come from the annotations the plugin already captures with
their arguments as written. A class-level `@RequestMapping("/bridge")`
composes with a function's `@GetMapping("/check/{id}")`, and the verb is
read from the annotation's own name — `GetMapping`, `POST`, `HttpGet` —
wherever the framework puts the verb word, so Spring, JAX-RS and Micronaut
need no separate table. A mapping that states **no** verb records `*`, which
matches every verb on that path and says so in the answer.

**Requests** are any call with a path-shaped string argument, whatever the
callee is called — `RestTemplate`, `WebClient`, Ktor, Retrofit and a
project-local wrapper alike. An absolute URL or a query string reduces to
the path. A string template is not a literal and states nothing.

**A declarative client declares nothing.** `@HttpExchange` / `@GetExchange`
(and `@FeignClient`) look exactly like a controller and mean the opposite:
the interface *asks for* the path, it does not serve it. Recording `serves`
there would invent a handler that does not exist, so the plugin records
neither side for now rather than the wrong one.

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
| `annotation` | `definitive` | a JUnit / TestNG / kotlin.test annotation — `@Test`, `@ParameterizedTest`, `@RepeatedTest`, and the `@Before*`/`@After*` hooks |
| `build-layout` | `strong` | under a test source set: `src/test`, `src/androidTest` (the Android plugin's instrumentation tests) or `src/commonTest` |

Ranked, so the strongest evidence is what remains: a source set is a layout
Gradle and Maven enforce and a build file can still redraw, while the
annotation is JUnit's own definition of a test. It reaches the type holding
the function as well as the function — a class with a `@Test` in it is a
test class — so an annotated test in `src/main` is still flagged, and an
unannotated helper in `src/test` keeps the weaker flag.

## Options (`[plugins.kotlin]`)

| Key | Effect |
|---|---|
| `include_source = "true"` | attach `_code` to declarations |

## Build & test

```console
$ cd parser && cargo test             # 56 tests — native, the C grammar compiles for the host too
$ just kotlin-plugin                  # needs wasi-sdk; WASI_SDK env overrides the default path
$ drsg plugin install component/target/wasm32-wasip2/release/drsg_plugin_kotlin.wasm
```

## Known limits

**The grammar reads two shapes wrongly, and both are recovered rather than
lost.** A bare annotation above a class is re-read as an expression of its
own when a second declaration follows it later in the file — the class would
otherwise lose `@RestController` and every argument with it — and `fun
interface` and a one-member named `companion object` come back wrapped in an
error node. The plugin reattaches the detached annotations to the
declaration they were written for, and walks the subtree under an error node
rather than dropping it. Both are covered by tests, so a grammar release
that fixes them will not break anything here.

Overload resolution is by name (all overloads share one node); the supertype
chain stops at types the tree does not hold; lambdas' bodies attribute their
calls to the enclosing function (where a reader looks), so
`handlers.forEach { it.handle() }` stays unresolved where `for (h: Handler
in handlers)` resolves — `it` has no written type. An extension function is
recorded as a `USES_TYPE` edge on its receiver rather than as a member of
it, because it is not one. Delegated properties (`by lazy`, `by
viewModels()`) resolve no further than the delegate's own call. `.kts` build
scripts parse, but a Gradle DSL block is a lambda with an implicit receiver,
so little in one resolves.

Spring's own edges: `@ComponentScan`, `@Profile` and the `@ConditionalOn*`
family are not read, so a stereotyped class outside the scanned packages
counts as a bean; a Spring Data repository has no implementation in the tree
— Spring generates it — so those chains end at the port, correctly. A
constructor parameter binds to the property of the **same name**, which is
what `val` makes it in Kotlin and the convention everywhere else.

Routes: a servlet context path and a gateway's rewrite are deployment
configuration, so a declaration is recorded as the source states it and the
prefix the request actually carries is not known here.
