# plugin: java

English · [简体中文](README_CN.md)

Parses Java into graph facts. Manifest `java@2`, claims `.java`. Built on
[tree-sitter-java](https://github.com/tree-sitter/tree-sitter-java) — no
mature pure-Rust Java frontend exists, and the grammar every editor already
trusts is a better foundation than one we would have to earn trust for. The
C runtime and grammar compile to `wasm32-wasip2` under wasi-sdk's clang.

## Layout

```
parser/     drsg-java-parser — the language logic, 42 native tests
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

| Label | Emitted for | Props beyond `doc_comment` (javadoc) / `visibility` / `file` / `line` / `end_line` |
|---|---|---|
| `Package` | one per `package` declaration | `name`; javadoc from `package-info.java`. Packages nest by `CONTAINS` where both ends were parsed |
| `Class` / `Interface` / `Enum` / `Record` / `Annotation` | type declarations | `fields`: described `name: type` list (a record's come from its header); `variants` for enum constants |
| `Method` | methods and constructors — overloads share the key: one node, the first (with the docs) wins | `signature` (`void connect(int timeout)`) |
| stand-ins | foreign types/members | `Class` / `Interface` / `Annotation` / `Function` (what the reference proves) + `External` |

Every declaration also carries `end_line`: where it stops, so `snippet` reads
exactly the declaration instead of a fixed number of lines after its first,
and a reader sees whether a thing is six lines or two hundred without opening
it. `line` still points at the **name**, so documentation above a declaration
cannot move where the graph says it starts.

## Edges

| Type | Meaning | `line` |
|---|---|---|
| `CONTAINS` | package → type → member, outer type → nested type | declaration site |
| `HAS_METHOD` | interface/annotation → its demanded methods | member |
| `CALLS` | method → callee; `new Foo(…)` is a call to the type; `super.m()` walks the extends chain | call site |
| `IMPORTS` | each of the file's top-level types → what the file imports (Java's import is file-scoped; the type is what a reader navigates by) | import statement |
| `EXTENDS` / `IMPLEMENTS` | class → superclass / interfaces; interface → extended interfaces; generic bases extend what they subscript (`ArrayList<Double>` → `java.util.ArrayList`) | type declaration |
| `ANNOTATED_BY` | type/method → its annotations — on a Spring codebase the annotations *are* the architecture (`@Service`, `@Transactional`, `@GetMapping`); `java.lang`'s own markers (`@Override`, `@Deprecated`, …) are noise and stay out. `args` on the edge carries the arguments **as written** (`@Qualifier("fast")`, `@PostMapping("/orders")`), uninterpreted: what they mean is the framework's business | annotation site |
| `USES_TYPE` | declaration → a type it is **typed by**, `role` on the edge (`field`, `param`, `return`), a record's components counted as fields. Generic arguments are walked — `List<Job>` is a dependency on `Job` — and only types this tree declares get an edge. A dependency a container wires also carries `injected` and `_injected_confidence` (below) | — |

## Routes

An HTTP route is the one reference this plugin cannot turn into an edge: the
handler or the caller is in another language, often another repository, and
between the two sides there is only a string. So the plugin records what the
source *states* and leaves the match to drsg's `wire`, which computes it per
call and never stores it.

On the symbol that already exists:

| Property | Set on | Shape |
|---|---|---|
| `serves` | the handler method, and the class whose mapping it composes with | `GET /api/orders/{}` — the verb, and the path with every variable segment reduced to `{}` |
| `requests` | the method making the call | the same shape; `*` where the source states no verb |
| `serves_as_written` / `requests_as_written` | both | the path exactly as the source wrote it, before normalising |

**Declarations** come from the annotations the plugin already captures with
their arguments as written. A class-level `@RequestMapping("/bridge")`
composes with a method's `@GetMapping("/check/{id}")`, and the verb is read
from the annotation's own name — `GetMapping`, `POST`, `HttpGet` — wherever
the framework puts the verb word, so Spring, JAX-RS (`@Path` + `@GET`) and
Micronaut (`@Controller` + `@Get`) need no separate table. A mapping that
states **no** verb records `*`, which matches every verb on that path and says
so in the answer: refusing it would refuse Spring's own `@RequestMapping`.

**Requests** are any call with a path-shaped string argument, whatever the
callee is called — `RestTemplate`, `WebClient`, `HttpClient`, and a
project-local wrapper alike. An absolute URL or a query string reduces to the
path.

**A declarative client declares nothing.** `@HttpExchange` / `@GetExchange`
(and `@FeignClient`) look exactly like a controller and mean the opposite: the
interface *asks for* the path, it does not serve it. Recording `serves` there
would invent a handler that does not exist, so the plugin records neither side
for now rather than the wrong one.

## Resolution — reading references the way javac does

In order: **already-qualified as written** → **same package** (no import
needed — that is the language's rule) → **single-type imports** → **wildcard
imports**, resolved against what the tree actually holds → **`java.lang`**
(known without an import: `String`, `System`, the usual exceptions).

- A **capitalized receiver** (`Helper.create()`, `com.acme.Helper.create()`)
  is a type reference, written down. A **lowercase receiver** is a value,
  typed where the source states its class — a declared local (an
  enhanced-for's variable included), a parameter, or a field — and counted
  unresolved where only a compiler would know.
- **Inherited calls** — `helper()` with no receiver, and `super.helper()` —
  walk the in-tree ancestry to the type that declares the method:
  superclasses and interfaces for a bare call, so an interface `default`
  answers one, and the **superclass alone** for `super.helper()`, which is
  where Java sends it (`Greeter.super.helper()` is the other one, and is not
  resolved).
- **Static imports** bind the method name itself.
- `import static`/wildcards/`java.lang` misses that still resolve nowhere
  are counted, never guessed.

Report notes: unresolved value-receiver calls · external calls · merged
declarations · injection points · ports wired to a bean · ports left
ambiguous.

## Spring injection — the container's own wiring, read from the source

Under dependency injection the type at a call site is the **port**, never the
implementation: `orderService.place(id)` names an interface, and which bean
answers is the container's decision. A graph that stops there says every
`@Service` is dead code and ends every call chain at the interface.

That decision is written down, so it is read rather than guessed. Where Rust
or Go leave the choice to whoever constructs the value — data flow, which this
plugin refuses to guess at — Spring states it in annotations, and **where the
statement is ambiguous Spring itself refuses to start**.

**An injection point** is a dependency the container supplies, marked on the
`USES_TYPE` edge as ranked evidence, the way `test_flag` is:

| `injected` | `_injected_confidence` | From |
|---|---|---|
| `annotation` | `definitive` | `@Autowired` / `@Inject` / `@Resource` on the field, the setter, or the constructor |
| `constructor` | `strong` | a stereotyped bean's **only** constructor, which Spring injects unannotated — Lombok's `@RequiredArgsConstructor` included, since the fields are still declared |

**A bean** is a type carrying `@Service`, `@Component`, `@Repository`,
`@Controller`, `@RestController`, `@Configuration` or either
`ControllerAdvice`, or the declared return type of a `@Bean` method in a
`@Configuration`. Its name is the annotation's own argument, else the simple
name decapitalized by Java's `Introspector` rule, where two leading capitals
stay as written (`URLFetcher`).

**A call through an injected port** gets a second `CALLS` edge, to the bean's
method, stamped `_resolved_by` with the rule that picked it, `_confidence`,
and `_via` naming the port method the source wrote. The port's own edge stays
beside it: one is what the source says, the other what the container does.

| `_resolved_by` | `_confidence` | The rule |
|---|---|---|
| `spring-qualifier` | `high` | the injection point's `@Qualifier` / `@Resource` names a bean |
| `spring-bean` | `high` | one bean implements the port and answers the method |
| `spring-primary` | `high` | several do, and exactly one is `@Primary` |
| `spring-bean-collection` | `medium` | the type asks for a `List` / `Set` / `Collection` / `Iterable` / `Map` / `Stream` of the port, so Spring hands it **every** bean — which one runs at a given moment is nowhere written |

The edge targets the type that **declares** the method, which may be an
abstract base: that is where the code a reader wants is.

Nothing is emitted when several beans implement the port and no `@Primary` or
`@Qualifier` chooses — the report says how many, and that is the case Spring
would fail to start on. Nothing is emitted for a port value that is not an
injection point either: whoever built it chose the implementation. The one
widening is a type that injects a container-full of a port, which has declared
it takes every bean of it, and so dispatches any value of that port inside it.

Only an **interface** is dispatched through. An abstract class's method has a
body, so the recorded edge already points at code that runs; reaching an
override from it is a different question, and not one this plugin answers.

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
| `annotation` | `definitive` | a JUnit/TestNG annotation — `@Test`, `@ParameterizedTest`, `@RepeatedTest`, `@TestFactory`, `@TestTemplate`, and the `@Before*`/`@After*` hooks |
| `build-layout` | `strong` | under `src/test/java` |

Ranked, so the strongest evidence is what remains: the source set is a layout
Maven and Gradle both enforce and a build file can still redraw, while the
annotation is JUnit's own definition of a test. It reaches the type holding
the method as well as the method — a class with an `@Test` in it is a test
class — so an annotated test in `src/main/java` is still flagged, and an
unannotated helper in `src/test/java` keeps the weaker flag.

## Options (`[plugins.java]`)

| Key | Effect |
|---|---|
| `include_source = "true"` | attach `_code` to declarations |

## Build & test

```console
$ cd parser && cargo test             # 42 tests — native, the C grammar compiles for the host too
$ just java-plugin                    # needs wasi-sdk; WASI_SDK env overrides the default path
$ drsg plugin install component/target/wasm32-wasip2/release/drsg_plugin_java.wasm
```

## Known limits

Overload resolution is by name (all overloads share one node); the extends
chain stops at types the tree does not hold; lambdas' bodies attribute
their calls to the enclosing method (where a reader looks); `var` locals
are values, so calls through them are counted.

Spring's own edges: `@ComponentScan`, `@Profile` and the `@ConditionalOn*`
family are not read, so a stereotyped class outside the scanned packages
counts as a bean; XML bean definitions and programmatic registration are not
read at all; a Spring Data repository has no implementation in the tree —
Spring generates it — so those chains end at the port, correctly. A
constructor's or setter's parameter binds to the field of the **same name**,
which is the convention every codebase follows and none guarantees; when the
names differ the type alone decides and the `@Qualifier` is not carried over.
A lambda's parameter is untyped, so `handlers.forEach(h -> h.handle())` stays
unresolved where `for (Handler h : handlers)` resolves. `Optional<Port>` and
`ObjectProvider<Port>` injection resolves no further than the wrapper.

Routes: a servlet context path and a gateway's rewrite are deployment
configuration, so a declaration is recorded as the source states it and the
prefix the request actually carries is not known here. A declarative client
(`@FeignClient`, `@HttpExchange`) is recorded as neither side rather than as a
`requests`, which is the next thing to add.
