//! The parser's own tests, running natively — the reason it is a plain
//! library under the wasm component rather than one crate with it.

use super::*;

struct MapFiles {
    files: Vec<(&'static str, &'static str)>,
}

impl Files for MapFiles {
    fn list(&self, suffix: &str) -> Result<Vec<String>, String> {
        let mut out: Vec<String> = self
            .files
            .iter()
            .filter(|(p, _)| p.ends_with(suffix))
            .map(|(p, _)| p.to_string())
            .collect();
        out.sort();
        Ok(out)
    }

    fn read(&self, path: &str) -> Result<Vec<u8>, String> {
        self.files
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, s)| s.as_bytes().to_vec())
            .ok_or_else(|| format!("not found: {path}"))
    }

    fn label(&self) -> Option<String> {
        None
    }
}

fn run(files: Vec<(&'static str, &'static str)>) -> Assembled {
    let m = MapFiles { files };
    let paths = m.list(".kt").unwrap();
    assemble(parse_chunk(&m, &paths, false))
}

fn node<'a>(a: &'a Assembled, key: &str) -> &'a Node {
    a.nodes.iter().find(|n| n.key == key).unwrap_or_else(|| {
        panic!(
            "no node {key} in {:?}",
            a.nodes.iter().map(|n| &n.key).collect::<Vec<_>>()
        )
    })
}

fn has_edge(a: &Assembled, src: &str, ty: &str, dst: &str) -> bool {
    a.edges
        .iter()
        .any(|e| e.src == src && e.ty == ty && e.dst == dst)
}

fn edge<'a>(a: &'a Assembled, ty: &str, dst: &str) -> &'a Edge {
    a.edges
        .iter()
        .find(|e| e.ty == ty && e.dst == dst)
        .unwrap_or_else(|| {
            panic!(
                "no {ty} edge to {dst} in {:?}",
                a.edges
                    .iter()
                    .map(|e| format!("{} -{}-> {}", e.src, e.ty, e.dst))
                    .collect::<Vec<_>>()
            )
        })
}

fn note_containing<'a>(a: &'a Assembled, want: &str) -> &'a str {
    a.notes
        .iter()
        .find(|n| n.contains(want))
        .unwrap_or_else(|| panic!("no note containing {want:?} in {:?}", a.notes))
}

fn text(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

fn value(n: &Node, prop: &str) -> Value {
    n.props
        .get(prop)
        .unwrap_or_else(|| panic!("no {prop} on {}: {:?}", n.key, n.props))
        .get("$value")
        .cloned()
        .unwrap_or_else(|| n.props[prop].clone())
}

// ---- keys and shapes ------------------------------------------------------

/// Keys are Kotlin's own qualified names: package, then declaration — and
/// the package contains its declarations, packages nest where both were
/// parsed.
#[test]
fn keys_are_qualified_names() {
    let a = run(vec![
        (
            "core/Engine.kt",
            "package com.acme.core\n\nclass Engine {\n    fun start() {}\n}\n",
        ),
        ("acme/Root.kt", "package com.acme\n\nclass Root\n"),
    ]);
    node(&a, "com.acme.core.Engine");
    node(&a, "com.acme.core.Engine.start");
    assert!(has_edge(
        &a,
        "com.acme.core",
        "CONTAINS",
        "com.acme.core.Engine"
    ));
    assert!(has_edge(&a, "com.acme", "CONTAINS", "com.acme.core"));
    assert_eq!(node(&a, "com.acme.core").label, "Package");
}

/// A nested type chains through the outer one, and the outer contains it.
#[test]
fn a_nested_type_chains_through_its_outer() {
    let a = run(vec![(
        "E.kt",
        "package p\nclass Engine {\n    class Builder {\n        fun build() {}\n    }\n}\n",
    )]);
    node(&a, "p.Engine.Builder.build");
    assert!(has_edge(&a, "p.Engine", "CONTAINS", "p.Engine.Builder"));
}

/// A class carries its KDoc, its visibility where it writes one, its
/// properties as `name: type`, and its functions with signatures as written.
#[test]
fn a_class_has_properties_functions_and_docs() {
    let a = run(vec![(
        "Client.kt",
        r#"package db

/** A connection. */
class Client(private val url: String) {
    private var retries: Int = 3

    /** Opens. */
    fun connect(timeout: Int): Boolean {
        log("open")
        return true
    }

    private fun log(msg: String) {}
}
"#,
    )]);
    let client = node(&a, "db.Client");
    assert_eq!(text(&client.props["doc_comment"]), "A connection.");
    assert_eq!(
        value(client, "fields"),
        serde_json::json!(["url: String", "retries: Int"])
    );
    let connect = node(&a, "db.Client.connect");
    assert_eq!(
        text(&connect.props["signature"]),
        "connect(timeout: Int): Boolean"
    );
    assert_eq!(text(&connect.props["doc_comment"]), "Opens.");
    assert_eq!(
        text(&node(&a, "db.Client.log").props["visibility"]),
        "private"
    );
    assert!(has_edge(&a, "db.Client.connect", "CALLS", "db.Client.log"));
}

/// `line` points at the name and `end_line` at where the declaration stops,
/// so documentation above it cannot move where the graph says it starts.
#[test]
fn a_declaration_says_where_it_begins_and_ends() {
    let a = run(vec![(
        "E.kt",
        "package p\n\n/** Doc. */\nclass Engine {\n    fun start() {\n    }\n}\n",
    )]);
    let engine = node(&a, "p.Engine");
    assert_eq!(engine.props["line"], Value::from(4u64));
    assert_eq!(engine.props["end_line"], Value::from(7u64));
}

/// Each kind of declaration gets the label its keyword names.
#[test]
fn every_declaration_kind_has_its_own_label() {
    let a = run(vec![(
        "K.kt",
        r#"package p
class C
interface I
enum class E { A, B }
annotation class M
object O
data class D(val x: Int)
typealias H = (String) -> Unit
fun top() {}
val CONST: Int = 1
var mutable: Int = 2
"#,
    )]);
    assert_eq!(node(&a, "p.C").label, "Class");
    assert_eq!(node(&a, "p.I").label, "Interface");
    assert_eq!(node(&a, "p.E").label, "Enum");
    assert_eq!(node(&a, "p.M").label, "Annotation");
    assert_eq!(node(&a, "p.O").label, "Object");
    assert_eq!(node(&a, "p.H").label, "TypeAlias");
    assert_eq!(node(&a, "p.top").label, "Function");
    assert_eq!(node(&a, "p.CONST").label, "Const");
    assert_eq!(node(&a, "p.mutable").label, "Var");
    assert_eq!(
        value(node(&a, "p.E"), "variants"),
        serde_json::json!(["A", "B"])
    );
    assert_eq!(
        value(node(&a, "p.D"), "modifiers"),
        serde_json::json!(["data"])
    );
}

/// A top-level function and property hang off the package, because in Kotlin
/// the declaration is the unit and the package is the container.
#[test]
fn top_level_declarations_hang_off_the_package() {
    let a = run(vec![(
        "T.kt",
        "package p\nfun helper(): Int = 1\nval limit: Int = 3\n",
    )]);
    assert!(has_edge(&a, "p", "CONTAINS", "p.helper"));
    assert!(has_edge(&a, "p", "CONTAINS", "p.limit"));
    assert_eq!(
        text(&node(&a, "p.helper").props["signature"]),
        "helper(): Int"
    );
}

/// An interface demands its functions; a class contains its own.
#[test]
fn an_interface_demands_its_functions() {
    let a = run(vec![(
        "R.kt",
        "package p\ninterface Runner {\n    fun start()\n}\nclass Engine {\n    fun stop() {}\n}\n",
    )]);
    assert!(has_edge(&a, "p.Runner", "HAS_METHOD", "p.Runner.start"));
    assert!(has_edge(&a, "p.Engine", "CONTAINS", "p.Engine.stop"));
}

/// A property is a node of its own — it carries a type, and often a getter
/// with code in it — as well as a line in its type's `fields`.
#[test]
fn a_property_is_a_node_and_a_field() {
    let a = run(vec![(
        "P.kt",
        "package p\nclass Holder {\n    val helper: Helper = Helper()\n}\nclass Helper\n",
    )]);
    let prop = node(&a, "p.Holder.helper");
    assert_eq!(prop.label, "Property");
    assert_eq!(text(&prop.props["signature"]), "helper: Helper");
    assert!(has_edge(&a, "p.Holder", "CONTAINS", "p.Holder.helper"));
    // The initialiser's call is the property's.
    assert!(has_edge(&a, "p.Holder.helper", "CALLS", "p.Helper"));
}

// ---- supertypes -----------------------------------------------------------

/// Kotlin's syntax only says whether a supertype was constructed. The
/// target's own declaration says what it is, and that is what the edge
/// records.
#[test]
fn supertypes_are_named_by_what_they_are() {
    let a = run(vec![(
        "E.kt",
        r#"package p
abstract class Base
interface Runner
class Engine : Base(), Runner
"#,
    )]);
    assert!(has_edge(&a, "p.Engine", "EXTENDS", "p.Base"));
    assert!(has_edge(&a, "p.Engine", "IMPLEMENTS", "p.Runner"));
}

/// An interface's own supertypes are interfaces, written without a
/// constructor invocation because one is impossible.
#[test]
fn an_interface_extends_what_it_lists() {
    let a = run(vec![(
        "I.kt",
        "package p\ninterface Base\ninterface Runner : Base\n",
    )]);
    assert!(has_edge(&a, "p.Runner", "IMPLEMENTS", "p.Base"));
}

/// A supertype this tree does not hold keeps what the syntax said: a
/// constructed one is a class, a bare one an interface.
#[test]
fn a_foreign_supertype_keeps_what_the_syntax_said() {
    let a = run(vec![(
        "E.kt",
        "package p\nimport ext.Base\nimport ext.Runner\nclass Engine : Base(), Runner\n",
    )]);
    assert!(has_edge(&a, "p.Engine", "EXTENDS", "ext.Base"));
    assert!(has_edge(&a, "p.Engine", "IMPLEMENTS", "ext.Runner"));
    assert_eq!(node(&a, "ext.Base").label, "Class");
    assert_eq!(node(&a, "ext.Runner").label, "Interface");
    assert_eq!(node(&a, "ext.Runner").extra_labels, vec!["External"]);
}

/// An inherited call walks the in-tree supertypes to the type that declares
/// the function, interfaces and their default implementations included.
#[test]
fn an_inherited_call_walks_the_supertypes() {
    let a = run(vec![(
        "E.kt",
        r#"package p
interface Greeter {
    fun hello() {}
}
abstract class Base {
    fun helper() {}
}
class Engine : Base(), Greeter {
    fun start() {
        helper()
        hello()
        super.helper()
    }
}
"#,
    )]);
    assert!(has_edge(&a, "p.Engine.start", "CALLS", "p.Base.helper"));
    assert!(has_edge(&a, "p.Engine.start", "CALLS", "p.Greeter.hello"));
}

// ---- companion objects ----------------------------------------------------

/// A companion object is what Kotlin writes where Java writes `static`, so a
/// call through the outer type's name reaches it.
#[test]
fn a_companion_object_answers_through_its_type() {
    let a = run(vec![(
        "E.kt",
        r#"package p
class Engine {
    companion object {
        fun create(): Engine = Engine()
    }
}
class Caller {
    fun go() {
        Engine.create()
    }
}
"#,
    )]);
    node(&a, "p.Engine.Companion");
    assert!(has_edge(
        &a,
        "p.Caller.go",
        "CALLS",
        "p.Engine.Companion.create"
    ));
}

/// A named companion object keeps its name, and still answers through the
/// outer type.
#[test]
fn a_named_companion_keeps_its_name() {
    let a = run(vec![(
        "E.kt",
        r#"package p
class Engine {
    companion object Factory {
        fun create(): Engine = Engine()
        val limit: Int = 1
    }
}
class Caller {
    fun go() {
        Engine.create()
    }
}
"#,
    )]);
    node(&a, "p.Engine.Factory.create");
    assert!(has_edge(
        &a,
        "p.Caller.go",
        "CALLS",
        "p.Engine.Factory.create"
    ));
}

// ---- calls ----------------------------------------------------------------

/// Kotlin spells construction as a call to the type, and so does the graph.
#[test]
fn construction_is_a_call_to_the_type() {
    let a = run(vec![(
        "E.kt",
        "package p\nclass Helper\nclass Engine {\n    fun go() {\n        Helper()\n    }\n}\n",
    )]);
    assert!(has_edge(&a, "p.Engine.go", "CALLS", "p.Helper"));
}

/// A qualified or nested constructor call names the type it constructs.
#[test]
fn a_qualified_construction_names_its_type() {
    let a = run(vec![
        ("B.kt", "package com.acme\nclass Box\n"),
        (
            "E.kt",
            "package p\nclass Engine {\n    fun go() {\n        com.acme.Box()\n    }\n}\n",
        ),
    ]);
    assert!(has_edge(&a, "p.Engine.go", "CALLS", "com.acme.Box"));
}

/// A value receiver resolves where the source states its type — a declared
/// local, a parameter, or a property.
#[test]
fn a_value_receiver_resolves_where_the_source_types_it() {
    let a = run(vec![(
        "E.kt",
        r#"package p
class Helper {
    fun spin() {}
}
class Engine(private val held: Helper) {
    fun go(param: Helper) {
        val local: Helper = Helper()
        local.spin()
        param.spin()
        held.spin()
        this.held.spin()
    }
}
"#,
    )]);
    let hits = a
        .edges
        .iter()
        .filter(|e| e.src == "p.Engine.go" && e.dst == "p.Helper.spin")
        .count();
    assert_eq!(hits, 1, "one CALLS edge, however many sites reached it");
    let stamped = edge(&a, "CALLS", "p.Helper.spin");
    assert!(
        ["receiver", "self-attr"].contains(&text(&stamped.props["_resolved_by"])),
        "{:?}",
        stamped.props
    );
    assert_eq!(text(&stamped.props["_confidence"]), "high");
}

/// `val x = Helper()` states its type through the constructor it calls.
#[test]
fn an_inferred_local_is_typed_by_what_it_constructs() {
    let a = run(vec![(
        "E.kt",
        r#"package p
class Helper {
    fun spin() {}
}
class Engine {
    fun go() {
        val h = Helper()
        h.spin()
    }
}
"#,
    )]);
    assert!(has_edge(&a, "p.Engine.go", "CALLS", "p.Helper.spin"));
}

/// A typed loop variable states its type as plainly as a local does.
#[test]
fn a_typed_loop_variable_resolves() {
    let a = run(vec![(
        "E.kt",
        r#"package p
interface Handler {
    fun handle()
}
class Engine {
    fun go(handlers: List<Handler>) {
        for (h: Handler in handlers) {
            h.handle()
        }
    }
}
"#,
    )]);
    assert!(has_edge(&a, "p.Engine.go", "CALLS", "p.Handler.handle"));
}

/// A static factory chain is typed by the factory's declared return.
#[test]
fn a_factory_chain_is_typed_by_its_return() {
    let a = run(vec![(
        "E.kt",
        r#"package p
class Helper {
    fun spin() {}
    companion object {
        fun create(): Helper = Helper()
    }
}
class Engine {
    fun go() {
        Helper.create().spin()
    }
}
"#,
    )]);
    assert!(has_edge(&a, "p.Engine.go", "CALLS", "p.Helper.spin"));
    assert_eq!(
        text(&edge(&a, "CALLS", "p.Helper.spin").props["_resolved_by"]),
        "chain"
    );
}

/// A bare call reaches a top-level function of the same package, which is
/// how Kotlin resolves one.
#[test]
fn a_bare_call_reaches_a_top_level_function() {
    let a = run(vec![
        ("H.kt", "package p\nfun helper() {}\n"),
        (
            "E.kt",
            "package p\nclass Engine {\n    fun go() {\n        helper()\n    }\n}\n",
        ),
    ]);
    assert!(has_edge(&a, "p.Engine.go", "CALLS", "p.helper"));
}

/// A call into another library is recorded as an external node carrying the
/// qualified name and nothing else.
#[test]
fn a_foreign_call_is_an_external_node() {
    let a = run(vec![(
        "E.kt",
        "package p\nimport ext.Helper\nclass Engine {\n    fun go() {\n        Helper.spin()\n    }\n}\n",
    )]);
    assert!(has_edge(&a, "p.Engine.go", "CALLS", "ext.Helper.spin"));
    assert_eq!(node(&a, "ext.Helper.spin").label, "Function");
    assert_eq!(node(&a, "ext.Helper.spin").extra_labels, vec!["External"]);
    note_containing(&a, "call(s) into other libraries");
}

/// A receiver whose type only a compiler would know is counted, never
/// guessed.
#[test]
fn an_untyped_receiver_is_counted() {
    let a = run(vec![(
        "E.kt",
        "package p\nclass Engine {\n    fun go(x: Any) {\n        mystery.spin()\n    }\n}\n",
    )]);
    note_containing(&a, "left unresolved");
}

// ---- imports --------------------------------------------------------------

/// Kotlin's import is file scoped, so the edge hangs off each of the file's
/// top-level declarations — the ones a reader navigates by.
#[test]
fn imports_hang_off_the_files_top_level_declarations() {
    let a = run(vec![(
        "E.kt",
        "package p\nimport ext.Helper\nimport ext.util.*\nclass Engine\nfun top() {}\n",
    )]);
    assert!(has_edge(&a, "p.Engine", "IMPORTS", "ext.Helper"));
    assert!(has_edge(&a, "p.top", "IMPORTS", "ext.Helper"));
    assert!(has_edge(&a, "p.Engine", "IMPORTS", "ext.util"));
}

/// An alias renames what the file calls a type; the target is unchanged.
#[test]
fn an_aliased_import_binds_the_alias() {
    let a = run(vec![
        (
            "H.kt",
            "package ext\nclass Helper {\n    fun spin() {}\n}\n",
        ),
        (
            "E.kt",
            "package p\nimport ext.Helper as Aide\nclass Engine {\n    fun go() {\n        Aide.spin()\n    }\n}\n",
        ),
    ]);
    assert!(has_edge(&a, "p.Engine.go", "CALLS", "ext.Helper.spin"));
}

/// A lowercase last segment imports a member, and binds the name itself —
/// the way Java's `import static` does.
#[test]
fn a_member_import_binds_the_name() {
    let a = run(vec![
        ("H.kt", "package ext\nfun spin() {}\n"),
        (
            "E.kt",
            "package p\nimport ext.spin\nclass Engine {\n    fun go() {\n        spin()\n    }\n}\n",
        ),
    ]);
    assert!(has_edge(&a, "p.Engine.go", "CALLS", "ext.spin"));
}

// ---- types a declaration is typed by --------------------------------------

/// A declared type is a dependency: a property, a parameter, a return and an
/// extension receiver each get a `USES_TYPE` edge with the position on it.
#[test]
fn a_declared_type_is_a_dependency() {
    let a = run(vec![(
        "E.kt",
        r#"package p
class Job
class Helper
class Engine(private val helper: Helper) {
    fun run(job: Job): Helper = helper
}
fun Job.describe(): String = "x"
"#,
    )]);
    assert_eq!(
        text(&edge(&a, "USES_TYPE", "p.Helper").props["name"]),
        "helper"
    );
    let roles: Vec<String> = a
        .edges
        .iter()
        .filter(|e| e.ty == "USES_TYPE" && e.dst == "p.Job")
        .map(|e| text(&e.props["role"]["$value"]).to_string())
        .collect();
    assert!(roles.contains(&"param".to_string()), "{roles:?}");
    assert!(roles.contains(&"receiver".to_string()), "{roles:?}");
    note_containing(&a, "type reference(s) recorded as `USES_TYPE`");
}

/// Generic arguments are walked: a property of type `List<Job>` depends on
/// `Job`, and only types this tree declares get an edge.
#[test]
fn generic_arguments_are_walked() {
    let a = run(vec![(
        "E.kt",
        "package p\nclass Job\nclass Engine {\n    val jobs: Map<String, List<Job>> = mapOf()\n}\n",
    )]);
    assert!(has_edge(&a, "p.Engine", "USES_TYPE", "p.Job"));
    assert!(!a.nodes.iter().any(|n| n.key == "String"));
}

/// A nullable type names the type it makes nullable.
#[test]
fn a_nullable_type_names_its_type() {
    let a = run(vec![(
        "E.kt",
        "package p\nclass Helper\nclass Engine {\n    val helper: Helper? = null\n}\n",
    )]);
    assert!(has_edge(&a, "p.Engine", "USES_TYPE", "p.Helper"));
}

/// A `typealias` is a dependency on what it names, and nothing more.
#[test]
fn a_typealias_depends_on_what_it_names() {
    let a = run(vec![(
        "E.kt",
        "package p\nclass Job\ntypealias Jobs = List<Job>\n",
    )]);
    assert!(has_edge(&a, "p.Jobs", "USES_TYPE", "p.Job"));
    assert_eq!(
        text(&node(&a, "p.Jobs").props["signature"]),
        "Jobs = List<Job>"
    );
}

// ---- annotations ----------------------------------------------------------

/// An annotation is a type reference with its arguments carried as written:
/// what they mean is the framework's business.
#[test]
fn an_annotation_carries_its_arguments_as_written() {
    let a = run(vec![(
        "E.kt",
        r#"package p

import org.spring.Service
import org.spring.Transactional

@Service("fast")
class Engine {
    @Transactional(readOnly = true)
    fun go() {}
}
"#,
    )]);
    assert_eq!(
        text(&edge(&a, "ANNOTATED_BY", "org.spring.Service").props["args"]["$value"]),
        "\"fast\""
    );
    assert_eq!(
        text(&edge(&a, "ANNOTATED_BY", "org.spring.Transactional").props["args"]["$value"]),
        "readOnly = true"
    );
    assert_eq!(node(&a, "org.spring.Service").label, "Annotation");
}

/// Compiler directives and JVM interop markers say nothing about structure
/// and stay out of the graph.
#[test]
fn noise_annotations_stay_out() {
    let a = run(vec![(
        "E.kt",
        "package p\nimport kotlin.jvm.JvmStatic\nclass Engine {\n    @Override\n    @JvmStatic\n    @Suppress(\"unused\")\n    fun go() {}\n}\n",
    )]);
    assert!(!a.edges.iter().any(|e| e.ty == "ANNOTATED_BY"));
}

/// A use-site target changes where the annotation applies, not what it is.
#[test]
fn a_use_site_target_is_still_the_annotation() {
    let a = run(vec![(
        "E.kt",
        "package p\nimport javax.inject.Inject\nclass Engine {\n    @field:Inject\n    lateinit var repo: Repo\n}\n",
    )]);
    assert!(has_edge(
        &a,
        "p.Engine.repo",
        "ANNOTATED_BY",
        "javax.inject.Inject"
    ));
}

// ---- test code ------------------------------------------------------------

/// A test source set is strong evidence; a test annotation is definitive,
/// and reaches the type holding the function as well as the function.
#[test]
fn test_code_says_what_the_evidence_was() {
    let a = run(vec![
        (
            "src/test/kotlin/p/EngineTest.kt",
            "package p\nclass EngineTest {\n    fun helper() {}\n}\n",
        ),
        (
            "src/main/kotlin/p/Live.kt",
            "package p\nclass Live {\n    @Test\n    fun checks() {}\n}\n",
        ),
    ]);
    let helper = node(&a, "p.EngineTest.helper");
    assert_eq!(text(&value(helper, "test_flag")), "build-layout");
    assert_eq!(text(&helper.props["_test_flag_confidence"]), "strong");
    let checks = node(&a, "p.Live.checks");
    assert_eq!(text(&value(checks, "test_flag")), "annotation");
    assert_eq!(text(&checks.props["_test_flag_confidence"]), "definitive");
    // A class holding an annotated test is a test class.
    assert_eq!(text(&value(node(&a, "p.Live"), "test_flag")), "annotation");
}

/// An Android module's instrumentation tests are a test source set too.
#[test]
fn android_test_sources_are_test_code() {
    let a = run(vec![(
        "app/src/androidTest/java/p/FlowTest.kt",
        "package p\nclass FlowTest\n",
    )]);
    assert_eq!(
        text(&value(node(&a, "p.FlowTest"), "test_flag")),
        "build-layout"
    );
}

/// Nothing is flagged when nothing is written down.
#[test]
fn production_code_carries_no_flag() {
    let a = run(vec![(
        "src/main/kotlin/p/E.kt",
        "package p\nclass Engine\n",
    )]);
    assert!(!node(&a, "p.Engine").props.contains_key("test_flag"));
}

// ---- routes ---------------------------------------------------------------

/// A class-level mapping is the prefix every handler under it hangs off, and
/// the verb is read from the annotation's own name.
#[test]
fn a_handler_declares_the_route_its_annotations_state() {
    let a = run(vec![(
        "C.kt",
        r#"package p
@RestController
@RequestMapping("/api/orders")
class Orders {
    @GetMapping("/{id}")
    fun one(id: String): String = "x"
}
"#,
    )]);
    assert_eq!(
        value(node(&a, "p.Orders.one"), "serves"),
        serde_json::json!(["GET /api/orders/{}"])
    );
    assert_eq!(
        value(node(&a, "p.Orders.one"), "serves_as_written"),
        serde_json::json!(["/api/orders/{id}"])
    );
}

/// A call with a path-shaped argument is asking for a route, whatever the
/// callee is called.
#[test]
fn a_path_shaped_argument_is_a_request() {
    let a = run(vec![(
        "E.kt",
        r#"package p
class Engine {
    fun go() {
        callBridge("/bridge/check")
    }
}
"#,
    )]);
    assert_eq!(
        value(node(&a, "p.Engine.go"), "requests"),
        serde_json::json!(["* /bridge/check"])
    );
}

/// The verb comes from the callee's name where the client puts it there.
#[test]
fn a_clients_name_states_the_verb() {
    let a = run(vec![(
        "E.kt",
        r#"package p
class Engine(private val rest: Rest) {
    fun go() {
        rest.getForObject("/orders/7")
    }
}
"#,
    )]);
    assert_eq!(
        value(node(&a, "p.Engine.go"), "requests"),
        serde_json::json!(["GET /orders/7"])
    );
}

/// A declarative client declares nothing: the interface *asks for* the path,
/// it does not serve it, and claiming a handler there would invent one.
#[test]
fn a_declarative_client_serves_nothing() {
    let a = run(vec![(
        "C.kt",
        r#"package p
@HttpExchange("/api")
interface Client {
    @GetExchange("/orders")
    fun orders(): String
}
"#,
    )]);
    assert!(!node(&a, "p.Client.orders").props.contains_key("serves"));
}

// ---- recovery and failure -------------------------------------------------

/// A file that is not Kotlin at all is skipped and counted, never guessed
/// at.
#[test]
fn an_unparsable_file_is_skipped() {
    let a = run(vec![("junk.kt", "\u{feff}%%% not kotlin %%%\n")]);
    assert_eq!(a.skipped + a.nodes.len(), a.skipped.max(a.nodes.len()));
}

/// `fun interface` is read wrongly by the grammar, and the declaration it
/// recovers is still the one the source wrote.
#[test]
fn a_functional_interface_still_lands() {
    let a = run(vec![(
        "F.kt",
        "package p\nfun interface Handler { fun handle() }\n",
    )]);
    assert_eq!(node(&a, "p.Handler").label, "Interface");
    assert!(has_edge(&a, "p.Handler", "HAS_METHOD", "p.Handler.handle"));
}

/// Two files declaring the same key keep the first and say so.
#[test]
fn a_shared_key_keeps_the_first() {
    let a = run(vec![
        ("a/E.kt", "package p\nclass Engine\n"),
        ("b/E.kt", "package p\nclass Engine\n"),
    ]);
    note_containing(&a, "shared a key across files");
}

/// `include_source` attaches the declaration's own text, for retrieval
/// rather than for indexing.
#[test]
fn include_source_attaches_the_text() {
    let m = MapFiles {
        files: vec![("E.kt", "package p\nclass Engine {\n    fun go() {}\n}\n")],
    };
    let paths = m.list(".kt").unwrap();
    let a = assemble(parse_chunk(&m, &paths, true));
    assert!(text(&value(node(&a, "p.Engine.go"), "_code")).contains("fun go()"));
}

/// Chunk boundaries do not change the answer: the facts are resolved once,
/// over every chunk, in the order the host gives them.
#[test]
fn chunking_does_not_change_the_answer() {
    let files = vec![
        ("H.kt", "package p\nclass Helper {\n    fun spin() {}\n}\n"),
        (
            "E.kt",
            "package p\nclass Engine {\n    fun go() {\n        Helper().spin()\n    }\n}\n",
        ),
    ];
    let m = MapFiles {
        files: files.clone(),
    };
    let whole = assemble(parse_chunk(&m, &m.list(".kt").unwrap(), false));
    let split = {
        let mut facts = parse_chunk(&m, &["E.kt".to_string()], false);
        facts.extend(parse_chunk(&m, &["H.kt".to_string()], false));
        assemble(facts)
    };
    let keys = |a: &Assembled| {
        let mut k: Vec<String> = a.nodes.iter().map(|n| n.key.clone()).collect();
        k.sort();
        k
    };
    assert_eq!(keys(&whole), keys(&split));
}

// ---- Spring injection -----------------------------------------------------

/// Kotlin's primary constructor is where a bean states its dependencies, and
/// Spring injects it unannotated. The `USES_TYPE` edge carries that as
/// ranked evidence.
#[test]
fn a_primary_constructor_is_an_injection_point() {
    let a = run(vec![(
        "E.kt",
        r#"package p

import org.spring.Service

interface Repo
@Service
class Engine(private val repo: Repo)
"#,
    )]);
    let e = edge(&a, "USES_TYPE", "p.Repo");
    assert_eq!(text(&e.props["injected"]["$value"]), "constructor");
    assert_eq!(text(&e.props["_injected_confidence"]), "strong");
    note_containing(&a, "injection points");
}

/// An annotation someone wrote outranks a constructor Spring injects by
/// convention.
#[test]
fn an_annotated_injection_point_outranks_a_constructor() {
    let a = run(vec![(
        "E.kt",
        r#"package p

import org.spring.Autowired
import org.spring.Service

interface Repo
@Service
class Engine {
    @Autowired
    lateinit var repo: Repo
}
"#,
    )]);
    let e = edge(&a, "USES_TYPE", "p.Repo");
    assert_eq!(text(&e.props["injected"]["$value"]), "annotation");
    assert_eq!(text(&e.props["_injected_confidence"]), "definitive");
}

/// A call through an injected port also reaches the bean the container
/// wires, stamped with the rule that picked it — and the call to the port
/// stays beside it, as the source wrote it.
#[test]
fn a_call_through_an_injected_port_reaches_its_bean() {
    let a = run(vec![(
        "E.kt",
        r#"package p

import org.spring.Service

interface Repo {
    fun save()
}

@Service
class JpaRepo : Repo {
    override fun save() {}
}

@Service
class Engine(private val repo: Repo) {
    fun go() {
        repo.save()
    }
}
"#,
    )]);
    assert!(has_edge(&a, "p.Engine.go", "CALLS", "p.Repo.save"));
    let wired = a
        .edges
        .iter()
        .find(|e| e.src == "p.Engine.go" && e.dst == "p.JpaRepo.save")
        .expect("the bean's own method");
    assert_eq!(text(&wired.props["_resolved_by"]), "spring-bean");
    assert_eq!(text(&wired.props["_via"]["$value"]), "p.Repo.save");
    note_containing(&a, "reach the bean Spring wires");
}

/// Several beans implement the port and exactly one is `@Primary`: that is
/// the one Spring starts with.
#[test]
fn primary_breaks_a_tie() {
    let a = run(vec![(
        "E.kt",
        r#"package p

import org.spring.Primary
import org.spring.Service

interface Repo {
    fun save()
}

@Service
class JpaRepo : Repo {
    override fun save() {}
}

@Primary
@Service
class FastRepo : Repo {
    override fun save() {}
}

@Service
class Engine(private val repo: Repo) {
    fun go() {
        repo.save()
    }
}
"#,
    )]);
    let wired = a
        .edges
        .iter()
        .find(|e| e.src == "p.Engine.go" && e.dst == "p.FastRepo.save")
        .expect("the primary bean");
    assert_eq!(text(&wired.props["_resolved_by"]), "spring-primary");
    assert!(!has_edge(&a, "p.Engine.go", "CALLS", "p.JpaRepo.save"));
}

/// A `@Qualifier` names the bean, and the name a stereotype's argument
/// states is the one it matches.
#[test]
fn a_qualifier_names_the_bean() {
    let a = run(vec![(
        "E.kt",
        r#"package p

import org.spring.Qualifier
import org.spring.Service

interface Repo {
    fun save()
}

@Service("fast")
class FastRepo : Repo {
    override fun save() {}
}

@Service
class SlowRepo : Repo {
    override fun save() {}
}

@Service
class Engine(@Qualifier("fast") private val repo: Repo) {
    fun go() {
        repo.save()
    }
}
"#,
    )]);
    let wired = a
        .edges
        .iter()
        .find(|e| e.src == "p.Engine.go" && e.dst == "p.FastRepo.save")
        .expect("the qualified bean");
    assert_eq!(text(&wired.props["_resolved_by"]), "spring-qualifier");
}

/// Several beans and nothing choosing between them is what Spring itself
/// refuses to start on: the call stops at its port, and the report says how
/// many did.
#[test]
fn an_ambiguous_port_stops_at_the_port() {
    let a = run(vec![(
        "E.kt",
        r#"package p

import org.spring.Service

interface Repo {
    fun save()
}

@Service
class JpaRepo : Repo {
    override fun save() {}
}

@Service
class MemRepo : Repo {
    override fun save() {}
}

@Service
class Engine(private val repo: Repo) {
    fun go() {
        repo.save()
    }
}
"#,
    )]);
    assert!(has_edge(&a, "p.Engine.go", "CALLS", "p.Repo.save"));
    assert!(!has_edge(&a, "p.Engine.go", "CALLS", "p.JpaRepo.save"));
    note_containing(&a, "stop at their port");
}

/// A type that asks for a container-full of a port has declared it takes
/// every bean of it, so a call through one reaches them all.
#[test]
fn a_container_full_of_a_port_reaches_every_bean() {
    let a = run(vec![(
        "E.kt",
        r#"package p

import org.spring.Service

interface Handler {
    fun handle()
}

@Service
class First : Handler {
    override fun handle() {}
}

@Service
class Second : Handler {
    override fun handle() {}
}

@Service
class Engine(private val handlers: List<Handler>) {
    fun go(one: Handler) {
        one.handle()
    }
}
"#,
    )]);
    for bean in ["p.First.handle", "p.Second.handle"] {
        let wired = a
            .edges
            .iter()
            .find(|e| e.src == "p.Engine.go" && e.dst == bean)
            .unwrap_or_else(|| panic!("no edge to {bean}"));
        assert_eq!(text(&wired.props["_resolved_by"]), "spring-bean-collection");
        assert_eq!(text(&wired.props["_confidence"]), "medium");
    }
}

/// A port value that is not an injection point is left alone: whoever built
/// it chose the implementation, and that is data flow a parser must not
/// guess at.
#[test]
fn a_port_that_is_not_injected_is_left_alone() {
    let a = run(vec![(
        "E.kt",
        r#"package p

import org.spring.Service

interface Repo {
    fun save()
}

@Service
class JpaRepo : Repo {
    override fun save() {}
}

class Engine {
    fun go(repo: Repo) {
        repo.save()
    }
}
"#,
    )]);
    assert!(has_edge(&a, "p.Engine.go", "CALLS", "p.Repo.save"));
    assert!(!has_edge(&a, "p.Engine.go", "CALLS", "p.JpaRepo.save"));
}

/// A `@Bean` function makes a bean of what it returns, under the function's
/// own name.
#[test]
fn a_bean_function_declares_what_it_returns() {
    let a = run(vec![(
        "C.kt",
        r#"package p

import org.spring.Bean
import org.spring.Configuration
import org.spring.Service

interface Repo {
    fun save()
}

class JpaRepo : Repo {
    override fun save() {}
}

@Configuration
class Wiring {
    @Bean
    fun repo(): JpaRepo = JpaRepo()
}

@Service
class Engine(private val repo: Repo) {
    fun go() {
        repo.save()
    }
}
"#,
    )]);
    let wired = a
        .edges
        .iter()
        .find(|e| e.src == "p.Engine.go" && e.dst == "p.JpaRepo.save")
        .expect("the bean the @Bean function declares");
    assert_eq!(text(&wired.props["_resolved_by"]), "spring-bean");
}

/// Only an interface is dispatched through: an abstract class's method has a
/// body, so the recorded edge already points at code that runs.
#[test]
fn an_abstract_base_is_not_dispatched_through() {
    let a = run(vec![(
        "E.kt",
        r#"package p

import org.spring.Service

abstract class Base {
    fun save() {}
}

@Service
class Impl : Base()

@Service
class Engine(private val base: Base) {
    fun go() {
        base.save()
    }
}
"#,
    )]);
    assert!(has_edge(&a, "p.Engine.go", "CALLS", "p.Base.save"));
    assert_eq!(
        a.edges
            .iter()
            .filter(|e| e.src == "p.Engine.go" && e.ty == "CALLS")
            .count(),
        1
    );
}

/// A bare annotation above a class is the class's, however the grammar
/// parsed it: with a second declaration later in the file it re-reads the
/// annotation list as an expression of its own and leaves the class without
/// it, arguments and all.
#[test]
fn a_detached_annotation_still_belongs_to_its_class() {
    let a = run(vec![(
        "C.kt",
        r#"package p

import org.spring.GetMapping
import org.spring.RequestMapping
import org.spring.RestController

/** The orders a shop serves. */
@RestController
@RequestMapping("/api/orders")
class Orders(private val service: OrderService) {
    @GetMapping("/{id}")
    fun one(id: String): String = "x"
}

data class OrderView(val id: String)
"#,
    )]);
    assert!(has_edge(
        &a,
        "p.Orders",
        "ANNOTATED_BY",
        "org.spring.RestController"
    ));
    assert_eq!(
        text(&edge(&a, "ANNOTATED_BY", "org.spring.RequestMapping").props["args"]["$value"]),
        "\"/api/orders\""
    );
    assert_eq!(
        value(node(&a, "p.Orders.one"), "serves"),
        serde_json::json!(["GET /api/orders/{}"])
    );
    // And the KDoc above the detached group is still the class's.
    assert_eq!(
        text(&node(&a, "p.Orders").props["doc_comment"]),
        "The orders a shop serves."
    );
}
