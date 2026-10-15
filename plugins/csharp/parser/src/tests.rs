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
    let paths = m.list(".cs").unwrap();
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

fn wired<'a>(a: &'a Assembled, src: &str, dst: &str) -> &'a Edge {
    a.edges
        .iter()
        .find(|e| e.src == src && e.dst == dst && e.props.contains_key("_resolved_by"))
        .unwrap_or_else(|| {
            panic!(
                "no stamped edge {src} -> {dst} in {:?}",
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

/// Keys are C#'s own qualified names: namespace, type, member — and the
/// namespace contains its types, namespaces nest where both were declared.
#[test]
fn keys_are_qualified_names() {
    let a = run(vec![
        (
            "Core/Engine.cs",
            "namespace Acme.Core;\npublic class Engine {\n    public void Start() {}\n}\n",
        ),
        ("Root.cs", "namespace Acme { class Root {} }\n"),
    ]);
    node(&a, "Acme.Core.Engine");
    node(&a, "Acme.Core.Engine.Start");
    assert!(has_edge(&a, "Acme.Core", "CONTAINS", "Acme.Core.Engine"));
    assert!(has_edge(&a, "Acme", "CONTAINS", "Acme.Core"));
    assert_eq!(node(&a, "Acme.Core").label, "Namespace");
}

/// A braced namespace holds its own body; a file-scoped one holds the rest
/// of the file.
#[test]
fn both_namespace_forms_contain_their_types() {
    let a = run(vec![
        ("A.cs", "namespace A { class One {} }\n"),
        ("B.cs", "namespace B;\nclass Two {}\n"),
    ]);
    assert!(has_edge(&a, "A", "CONTAINS", "A.One"));
    assert!(has_edge(&a, "B", "CONTAINS", "B.Two"));
}

/// A nested type chains through the outer one, and the outer contains it.
#[test]
fn a_nested_type_chains_through_its_outer() {
    let a = run(vec![(
        "E.cs",
        "namespace N;\nclass Engine {\n    class Builder {\n        public void Build() {}\n    }\n}\n",
    )]);
    node(&a, "N.Engine.Builder.Build");
    assert!(has_edge(&a, "N.Engine", "CONTAINS", "N.Engine.Builder"));
}

/// A class carries its XML documentation, its access as written, its fields
/// as `name: type`, and its methods with signatures as written.
#[test]
fn a_class_has_fields_methods_and_docs() {
    let a = run(vec![(
        "Client.cs",
        r#"namespace Db;

/// <summary>A connection.</summary>
/// <remarks>And a remark.</remarks>
public class Client
{
    private readonly string _url;
    public int Retries;

    /// <summary>Opens it.</summary>
    public bool Connect(int timeout)
    {
        Log("open");
        return true;
    }

    private void Log(string msg) {}
}
"#,
    )]);
    let client = node(&a, "Db.Client");
    assert_eq!(
        text(&client.props["doc_comment"]),
        "A connection.\nAnd a remark."
    );
    assert_eq!(
        value(client, "fields"),
        serde_json::json!(["_url: string", "Retries: int"])
    );
    // A field carries only a value, so it is a line here and not a node.
    assert!(!a.nodes.iter().any(|n| n.key == "Db.Client.Retries"));
    assert_eq!(text(&client.props["visibility"]), "public");
    let connect = node(&a, "Db.Client.Connect");
    assert_eq!(
        text(&connect.props["signature"]),
        "bool Connect(int timeout)"
    );
    assert_eq!(text(&connect.props["doc_comment"]), "Opens it.");
    assert_eq!(
        text(&node(&a, "Db.Client.Log").props["visibility"]),
        "private"
    );
    assert!(has_edge(&a, "Db.Client.Connect", "CALLS", "Db.Client.Log"));
}

/// Each kind of declaration gets the label its keyword names.
#[test]
fn every_declaration_kind_has_its_own_label() {
    let a = run(vec![(
        "K.cs",
        r#"namespace N;
public class C {}
public interface I { void Go(); }
public struct S { public int X; }
public record R(int A, string B);
public enum E { One, Two }
public delegate void H(string s);
"#,
    )]);
    assert_eq!(node(&a, "N.C").label, "Class");
    assert_eq!(node(&a, "N.I").label, "Interface");
    assert_eq!(node(&a, "N.S").label, "Struct");
    assert_eq!(node(&a, "N.R").label, "Record");
    assert_eq!(node(&a, "N.E").label, "Enum");
    assert_eq!(node(&a, "N.H").label, "Delegate");
    assert_eq!(
        value(node(&a, "N.E"), "variants"),
        serde_json::json!(["One", "Two"])
    );
    // A record's positional parameters are the properties it declares.
    assert_eq!(
        value(node(&a, "N.R"), "fields"),
        serde_json::json!(["A: int", "B: string"])
    );
}

/// An interface demands its members; a class contains its own.
#[test]
fn an_interface_demands_its_members() {
    let a = run(vec![(
        "R.cs",
        "namespace N;\ninterface IRunner { void Start(); }\nclass Engine { public void Stop() {} }\n",
    )]);
    assert!(has_edge(&a, "N.IRunner", "HAS_METHOD", "N.IRunner.Start"));
    assert!(has_edge(&a, "N.Engine", "CONTAINS", "N.Engine.Stop"));
}

/// A property is a node of its own — it carries a type, and often an
/// accessor with code in it — where a field is a line in its type's
/// `fields`.
#[test]
fn a_property_is_a_node_of_its_own() {
    let a = run(vec![(
        "P.cs",
        r#"namespace N;
class Helper { public void Spin() {} }
class Holder
{
    public Helper Dep { get; init; } = new Helper();
    public int Doubled => Compute();
    private int Compute() => 1;
}
"#,
    )]);
    let prop = node(&a, "N.Holder.Dep");
    assert_eq!(prop.label, "Property");
    assert_eq!(text(&prop.props["signature"]), "Helper Dep");
    assert!(has_edge(&a, "N.Holder", "CONTAINS", "N.Holder.Dep"));
    // The initialiser's call, and an accessor's, are the property's.
    assert!(has_edge(&a, "N.Holder.Dep", "CALLS", "N.Helper"));
    assert!(has_edge(
        &a,
        "N.Holder.Doubled",
        "CALLS",
        "N.Holder.Compute"
    ));
}

/// `line` points at the name and `end_line` at where the declaration stops,
/// so documentation above it cannot move where the graph says it starts.
#[test]
fn a_declaration_says_where_it_begins_and_ends() {
    let a = run(vec![(
        "E.cs",
        "namespace N;\n\n/// <summary>Doc.</summary>\nclass Engine\n{\n    void Go() {}\n}\n",
    )]);
    let engine = node(&a, "N.Engine");
    assert_eq!(engine.props["line"], Value::from(4u64));
    assert_eq!(engine.props["end_line"], Value::from(7u64));
}

/// A `partial` type is one declaration written in several files: the halves
/// are one node, and a call into either half resolves.
#[test]
fn a_partial_type_is_one_type() {
    let a = run(vec![
        (
            "A.cs",
            "namespace N;\npartial class Engine {\n    public void Start() { Helper(); }\n}\n",
        ),
        (
            "B.cs",
            "namespace N;\npartial class Engine {\n    private void Helper() {}\n}\n",
        ),
    ]);
    assert!(has_edge(&a, "N.Engine.Start", "CALLS", "N.Engine.Helper"));
    note_containing(&a, "which is what a `partial` type is");
}

// ---- base types -----------------------------------------------------------

/// C#'s syntax only says which base came first. The target's own
/// declaration says what it is, and that is what the edge records.
#[test]
fn base_types_are_named_by_what_they_are() {
    let a = run(vec![(
        "E.cs",
        r#"namespace N;
public abstract class Base {}
public interface IRunner {}
public class Engine : Base, IRunner {}
"#,
    )]);
    assert!(has_edge(&a, "N.Engine", "EXTENDS", "N.Base"));
    assert!(has_edge(&a, "N.Engine", "IMPLEMENTS", "N.IRunner"));
}

/// A base this tree does not hold is read by .NET's own naming convention:
/// `IRepo` is an interface, `ControllerBase` is a class.
#[test]
fn a_foreign_base_is_read_by_its_name() {
    let a = run(vec![(
        "E.cs",
        "namespace N;\nusing Ext;\npublic class Engine : ControllerBase, IExternal {}\n",
    )]);
    assert!(has_edge(&a, "N.Engine", "EXTENDS", "ControllerBase"));
    assert!(has_edge(&a, "N.Engine", "IMPLEMENTS", "IExternal"));
    assert_eq!(node(&a, "ControllerBase").label, "Class");
    assert_eq!(node(&a, "IExternal").label, "Interface");
    assert_eq!(node(&a, "IExternal").extra_labels, vec!["External"]);
}

/// An inherited call walks the in-tree bases to the type that declares the
/// member, and `base.M()` walks the same chain.
#[test]
fn an_inherited_call_walks_the_bases() {
    let a = run(vec![(
        "E.cs",
        r#"namespace N;
public abstract class Base
{
    protected void Helper() {}
}
public class Engine : Base
{
    public void Start()
    {
        Helper();
        base.Helper();
    }
}
"#,
    )]);
    assert!(has_edge(&a, "N.Engine.Start", "CALLS", "N.Base.Helper"));
}

/// A record's base is written with its constructor arguments, and is still
/// a base.
#[test]
fn a_records_base_is_a_base() {
    let a = run(vec![(
        "R.cs",
        "namespace N;\npublic record Base(int A);\npublic record Pair(int A, int B) : Base(A);\n",
    )]);
    assert!(has_edge(&a, "N.Pair", "EXTENDS", "N.Base"));
}

// ---- calls ----------------------------------------------------------------

/// `new Foo()` is a call to the type.
#[test]
fn construction_is_a_call_to_the_type() {
    let a = run(vec![(
        "E.cs",
        "namespace N;\nclass Helper {}\nclass Engine { void Go() { var h = new Helper(); } }\n",
    )]);
    assert!(has_edge(&a, "N.Engine.Go", "CALLS", "N.Helper"));
}

/// `Helper h = new();` constructs what it declares, and says so nowhere
/// else.
#[test]
fn a_target_typed_new_names_its_type() {
    let a = run(vec![(
        "E.cs",
        "namespace N;\nclass Helper {}\nclass Engine { void Go() { Helper h = new(); } }\n",
    )]);
    assert!(has_edge(&a, "N.Engine.Go", "CALLS", "N.Helper"));
}

/// A value receiver resolves where the source states its type — a declared
/// local, a parameter, a field, or a property.
#[test]
fn a_value_receiver_resolves_where_the_source_types_it() {
    let a = run(vec![(
        "E.cs",
        r#"namespace N;
class Helper { public void Spin() {} }
class Engine
{
    private readonly Helper _held;
    public Helper Prop { get; set; }

    void Go(Helper param)
    {
        Helper local = new Helper();
        local.Spin();
        param.Spin();
        _held.Spin();
        this._held.Spin();
        Prop.Spin();
    }
}
"#,
    )]);
    let hits = a
        .edges
        .iter()
        .filter(|e| e.src == "N.Engine.Go" && e.dst == "N.Helper.Spin")
        .count();
    assert_eq!(hits, 1, "one CALLS edge, however many sites reached it");
    let stamped = edge(&a, "CALLS", "N.Helper.Spin");
    assert!(
        ["receiver", "self-attr"].contains(&text(&stamped.props["_resolved_by"])),
        "{:?}",
        stamped.props
    );
}

/// `var h = new Helper()` states its type through the constructor it calls.
#[test]
fn an_inferred_local_is_typed_by_what_it_constructs() {
    let a = run(vec![(
        "E.cs",
        r#"namespace N;
class Helper { public void Spin() {} }
class Engine { void Go() { var h = new Helper(); h.Spin(); } }
"#,
    )]);
    assert!(has_edge(&a, "N.Engine.Go", "CALLS", "N.Helper.Spin"));
}

/// `x?.M()` reaches the same member as `x.M()`; whether it runs is not a
/// fact about the graph.
#[test]
fn a_null_conditional_call_is_a_call() {
    let a = run(vec![(
        "E.cs",
        r#"namespace N;
class Helper { public void Spin() {} }
class Engine
{
    private readonly Helper _h;
    void Go() { _h?.Spin(); }
}
"#,
    )]);
    assert!(has_edge(&a, "N.Engine.Go", "CALLS", "N.Helper.Spin"));
}

/// A typed `foreach` variable states its type as plainly as a local does.
#[test]
fn a_typed_foreach_variable_resolves() {
    let a = run(vec![(
        "E.cs",
        r#"namespace N;
interface IHandler { void Handle(); }
class Engine
{
    void Go(IHandler[] handlers)
    {
        foreach (IHandler h in handlers) { h.Handle(); }
    }
}
"#,
    )]);
    assert!(has_edge(&a, "N.Engine.Go", "CALLS", "N.IHandler.Handle"));
}

/// A static factory chain is typed by the factory's declared return.
#[test]
fn a_factory_chain_is_typed_by_its_return() {
    let a = run(vec![(
        "E.cs",
        r#"namespace N;
class Helper
{
    public void Spin() {}
    public static Helper Create() => new Helper();
}
class Engine { void Go() { Helper.Create().Spin(); } }
"#,
    )]);
    assert!(has_edge(&a, "N.Engine.Go", "CALLS", "N.Helper.Spin"));
    assert_eq!(
        text(&edge(&a, "CALLS", "N.Helper.Spin").props["_resolved_by"]),
        "chain"
    );
}

/// A qualified static call reaches the type it names, wherever in the tree
/// that type was declared.
#[test]
fn a_qualified_static_call_resolves() {
    let a = run(vec![
        (
            "H.cs",
            "namespace Acme.Util;\npublic class Deep { public static void Thing() {} }\n",
        ),
        (
            "E.cs",
            "namespace N;\nclass Engine { void Go() { Acme.Util.Deep.Thing(); } }\n",
        ),
    ]);
    assert!(has_edge(&a, "N.Engine.Go", "CALLS", "Acme.Util.Deep.Thing"));
}

/// A call into another library is recorded as an external node carrying the
/// qualified name and nothing else.
#[test]
fn a_foreign_call_is_an_external_node() {
    let a = run(vec![(
        "E.cs",
        "namespace N;\nclass Engine { void Go() { Console.WriteLine(\"x\"); } }\n",
    )]);
    assert!(has_edge(
        &a,
        "N.Engine.Go",
        "CALLS",
        "System.Console.WriteLine"
    ));
    assert_eq!(node(&a, "System.Console.WriteLine").label, "Function");
    note_containing(&a, "call(s) into other libraries");
}

/// A receiver whose type only a compiler would know is counted, never
/// guessed.
#[test]
fn an_untyped_receiver_is_counted() {
    let a = run(vec![(
        "E.cs",
        "namespace N;\nclass Engine { void Go() { mystery.Spin(); } }\n",
    )]);
    note_containing(&a, "left unresolved");
}

// ---- using directives -----------------------------------------------------

/// C#'s `using` is file scoped, so the edge hangs off each of the file's
/// top-level types — the ones a reader navigates by.
#[test]
fn usings_hang_off_the_files_top_level_types() {
    let a = run(vec![(
        "E.cs",
        "using System;\nusing Acme.Util;\nnamespace N;\nclass Engine {}\nclass Other {}\n",
    )]);
    assert!(has_edge(&a, "N.Engine", "IMPORTS", "Acme.Util"));
    assert!(has_edge(&a, "N.Other", "IMPORTS", "System"));
}

/// A plain `using` imports a namespace, which is how a simple name resolves
/// to a type in it.
#[test]
fn a_using_namespace_resolves_a_simple_name() {
    let a = run(vec![
        (
            "H.cs",
            "namespace Acme.Util;\npublic class Helper { public void Spin() {} }\n",
        ),
        (
            "E.cs",
            "using Acme.Util;\nnamespace N;\nclass Engine { void Go() { Helper.Spin(); } }\n",
        ),
    ]);
    assert!(has_edge(
        &a,
        "N.Engine.Go",
        "CALLS",
        "Acme.Util.Helper.Spin"
    ));
}

/// An alias renames what the file calls a type; the target is unchanged.
#[test]
fn an_alias_binds_the_alias() {
    let a = run(vec![
        (
            "H.cs",
            "namespace Acme.Util;\npublic class Helper { public void Spin() {} }\n",
        ),
        (
            "E.cs",
            "using Aide = Acme.Util.Helper;\nnamespace N;\nclass Engine { void Go() { Aide.Spin(); } }\n",
        ),
    ]);
    assert!(has_edge(
        &a,
        "N.Engine.Go",
        "CALLS",
        "Acme.Util.Helper.Spin"
    ));
}

/// `using static` puts a type's members in scope under their own names.
#[test]
fn a_using_static_binds_the_members() {
    let a = run(vec![
        (
            "H.cs",
            "namespace Acme.Util;\npublic class Helper { public static void Spin() {} }\n",
        ),
        (
            "E.cs",
            "using static Acme.Util.Helper;\nnamespace N;\nclass Engine { void Go() { Spin(); } }\n",
        ),
    ]);
    assert!(has_edge(
        &a,
        "N.Engine.Go",
        "CALLS",
        "Acme.Util.Helper.Spin"
    ));
}

/// A `global using` is every file's, which is what the keyword means.
#[test]
fn a_global_using_reaches_every_file() {
    let a = run(vec![
        ("G.cs", "global using Acme.Util;\n"),
        (
            "H.cs",
            "namespace Acme.Util;\npublic class Helper { public void Spin() {} }\n",
        ),
        (
            "E.cs",
            "namespace N;\nclass Engine { void Go() { Helper.Spin(); } }\n",
        ),
    ]);
    assert!(has_edge(
        &a,
        "N.Engine.Go",
        "CALLS",
        "Acme.Util.Helper.Spin"
    ));
}

/// A reference resolves against the enclosing namespace and then outward
/// through its ancestors, the way the compiler reads one.
#[test]
fn an_enclosing_namespace_resolves_outward() {
    let a = run(vec![
        (
            "H.cs",
            "namespace Acme;\npublic class Helper { public void Spin() {} }\n",
        ),
        (
            "E.cs",
            "namespace Acme.Core.Deep;\nclass Engine { void Go() { Helper.Spin(); } }\n",
        ),
    ]);
    assert!(has_edge(
        &a,
        "Acme.Core.Deep.Engine.Go",
        "CALLS",
        "Acme.Helper.Spin"
    ));
}

// ---- types a declaration is typed by --------------------------------------

/// A declared type is a dependency: a field, a property, a parameter and a
/// return each get a `USES_TYPE` edge with the position on it.
#[test]
fn a_declared_type_is_a_dependency() {
    let a = run(vec![(
        "E.cs",
        r#"namespace N;
class Job {}
class Helper {}
class Engine
{
    private readonly Helper _helper;
    public Job Current { get; set; }
    public Helper Run(Job job) => _helper;
}
"#,
    )]);
    let roles: Vec<String> = a
        .edges
        .iter()
        .filter(|e| e.ty == "USES_TYPE" && e.dst == "N.Job")
        .map(|e| text(&e.props["role"]["$value"]).to_string())
        .collect();
    assert!(roles.iter().any(|r| r.contains("property")), "{roles:?}");
    assert!(roles.iter().any(|r| r.contains("param")), "{roles:?}");
    assert_eq!(
        text(&edge(&a, "USES_TYPE", "N.Helper").props["name"]),
        "_helper"
    );
    note_containing(&a, "type reference(s) recorded as `USES_TYPE`");
}

/// Generic arguments are walked: a field of type `List<Job>` depends on
/// `Job`, and a predefined type is the language's own, never this tree's.
#[test]
fn generic_arguments_are_walked() {
    let a = run(vec![(
        "E.cs",
        "namespace N;\nclass Job {}\nclass Engine { private Dictionary<string, List<Job>> _jobs; }\n",
    )]);
    assert!(has_edge(&a, "N.Engine", "USES_TYPE", "N.Job"));
    assert!(!a.nodes.iter().any(|n| n.key == "string"));
}

/// A nullable or array type names the type it wraps.
#[test]
fn a_wrapped_type_names_its_type() {
    let a = run(vec![(
        "E.cs",
        "namespace N;\nclass Helper {}\nclass Engine { private Helper? _one; private Helper[] _many; }\n",
    )]);
    assert!(has_edge(&a, "N.Engine", "USES_TYPE", "N.Helper"));
}

/// A delegate's signature is a dependency on every type it writes.
#[test]
fn a_delegate_depends_on_its_signature() {
    let a = run(vec![(
        "E.cs",
        "namespace N;\nclass Job {}\npublic delegate Job Handler(Job input);\n",
    )]);
    assert!(has_edge(&a, "N.Handler", "USES_TYPE", "N.Job"));
    assert_eq!(
        text(&node(&a, "N.Handler").props["signature"]),
        "Job Handler(Job input)"
    );
}

// ---- attributes -----------------------------------------------------------

/// An attribute is a type reference with its arguments carried as written:
/// what they mean is the framework's business.
#[test]
fn an_attribute_carries_its_arguments_as_written() {
    let a = run(vec![(
        "E.cs",
        r#"namespace N;
using Microsoft.AspNetCore.Mvc;

[ApiController]
public class Orders
{
    [HttpGet("items")]
    public void One() {}
}
"#,
    )]);
    assert!(has_edge(&a, "N.Orders", "ANNOTATED_BY", "ApiController"));
    assert_eq!(
        text(&edge(&a, "ANNOTATED_BY", "HttpGet").props["args"]["$value"]),
        "\"items\""
    );
    assert_eq!(node(&a, "ApiController").label, "Annotation");
}

/// C# lets the source omit the `Attribute` suffix the type carries, so both
/// spellings reach one declaration.
#[test]
fn an_attribute_resolves_without_its_suffix() {
    let a = run(vec![(
        "E.cs",
        r#"namespace N;
public class MarkerAttribute : Attribute {}

[Marker]
public class Engine {}
"#,
    )]);
    assert!(has_edge(
        &a,
        "N.Engine",
        "ANNOTATED_BY",
        "N.MarkerAttribute"
    ));
}

/// Compiler directives and debugger markers say nothing about structure and
/// stay out of the graph.
#[test]
fn noise_attributes_stay_out() {
    let a = run(vec![(
        "E.cs",
        "namespace N;\nclass Engine {\n    [DebuggerStepThrough]\n    [CompilerGenerated]\n    void Go() {}\n}\n",
    )]);
    assert!(!a.edges.iter().any(|e| e.ty == "ANNOTATED_BY"));
}

// ---- test code ------------------------------------------------------------

/// A test project directory is strong evidence; a test attribute is
/// definitive, and reaches the type holding the method as well as the
/// method.
#[test]
fn test_code_says_what_the_evidence_was() {
    let a = run(vec![
        (
            "Acme.Tests/EngineTests.cs",
            "namespace N;\nclass EngineTests { void Helper() {} }\n",
        ),
        (
            "src/Live.cs",
            "namespace N;\nclass Live {\n    [Fact]\n    public void Checks() {}\n}\n",
        ),
    ]);
    let helper = node(&a, "N.EngineTests.Helper");
    assert_eq!(text(&value(helper, "test_flag")), "project-layout");
    assert_eq!(text(&helper.props["_test_flag_confidence"]), "strong");
    let checks = node(&a, "N.Live.Checks");
    assert_eq!(text(&value(checks, "test_flag")), "attribute");
    assert_eq!(text(&checks.props["_test_flag_confidence"]), "definitive");
    // A class holding an attributed test is a test class.
    assert_eq!(text(&value(node(&a, "N.Live"), "test_flag")), "attribute");
}

/// The suffix C# lets the source omit does not hide a test either.
#[test]
fn a_suffixed_test_attribute_still_counts() {
    let a = run(vec![(
        "src/E.cs",
        "namespace N;\nclass Live {\n    [TestMethodAttribute]\n    public void Checks() {}\n}\n",
    )]);
    assert_eq!(
        text(&value(node(&a, "N.Live.Checks"), "test_flag")),
        "attribute"
    );
}

/// Nothing is flagged when nothing is written down.
#[test]
fn production_code_carries_no_flag() {
    let a = run(vec![("src/E.cs", "namespace N;\nclass Engine {}\n")]);
    assert!(!node(&a, "N.Engine").props.contains_key("test_flag"));
}

// ---- routes ---------------------------------------------------------------

/// A type-level `[Route]` is the prefix every handler under it hangs off,
/// `[controller]` is filled in the way ASP.NET fills it, and the verb is
/// read from the attribute's own name.
#[test]
fn a_handler_declares_the_route_its_attributes_state() {
    let a = run(vec![(
        "C.cs",
        r#"namespace N;
using Microsoft.AspNetCore.Mvc;

[ApiController]
[Route("api/[controller]")]
public class OrdersController
{
    [HttpGet("{id:int}")]
    public string One(int id) => "x";

    [HttpPost]
    public void Create() {}
}
"#,
    )]);
    assert_eq!(
        value(node(&a, "N.OrdersController.One"), "serves"),
        serde_json::json!(["GET /api/Orders/{}"])
    );
    assert_eq!(
        value(node(&a, "N.OrdersController.Create"), "serves"),
        serde_json::json!(["POST /api/Orders"])
    );
}

/// A path written without a leading slash is still a path, which is how
/// ASP.NET writes every one of them.
#[test]
fn a_slashless_path_is_a_path() {
    let a = run(vec![(
        "C.cs",
        "namespace N;\nclass Api {\n    [HttpGet(\"health\")]\n    public void Health() {}\n}\n",
    )]);
    assert_eq!(
        value(node(&a, "N.Api.Health"), "serves"),
        serde_json::json!(["GET /health"])
    );
}

/// An argument that names neither a verb nor a route is not a path, however
/// much it looks like one.
#[test]
fn a_media_type_is_not_a_route() {
    let a = run(vec![(
        "C.cs",
        "namespace N;\nclass Api {\n    [Produces(\"application/json\")]\n    public void Health() {}\n}\n",
    )]);
    assert!(!node(&a, "N.Api.Health").props.contains_key("serves"));
}

/// A call with a path-shaped argument is asking for a route, whatever the
/// callee is called.
#[test]
fn a_path_shaped_argument_is_a_request() {
    let a = run(vec![(
        "E.cs",
        "namespace N;\nclass Engine { void Go() { CallBridge(\"/bridge/check\"); } }\n",
    )]);
    assert_eq!(
        value(node(&a, "N.Engine.Go"), "requests"),
        serde_json::json!(["* /bridge/check"])
    );
}

/// The verb comes from the callee's name where the client puts it there.
#[test]
fn a_clients_name_states_the_verb() {
    let a = run(vec![(
        "E.cs",
        r#"namespace N;
class Engine
{
    private readonly HttpClient _client;
    void Go() { _client.GetAsync("api/orders"); }
}
"#,
    )]);
    assert_eq!(
        value(node(&a, "N.Engine.Go"), "requests"),
        serde_json::json!(["GET /api/orders"])
    );
}

/// A declarative client declares nothing: the interface *asks for* the
/// path, it does not serve it, and claiming a handler there would invent
/// one.
#[test]
fn a_declarative_client_serves_nothing() {
    let a = run(vec![(
        "C.cs",
        r#"namespace N;
public interface IOrdersApi
{
    [Get("/orders")]
    Task<string> All();
}
"#,
    )]);
    assert!(!node(&a, "N.IOrdersApi.All").props.contains_key("serves"));
}

// ---- failure and chunking -------------------------------------------------

/// A file that is not C# at all is skipped and counted, never guessed at.
#[test]
fn an_unparsable_file_is_skipped() {
    let a = run(vec![("junk.cs", "%%% not c sharp %%%\n")]);
    assert_eq!(a.skipped, 1);
}

/// `include_source` attaches the declaration's own text, for retrieval
/// rather than for indexing.
#[test]
fn include_source_attaches_the_text() {
    let m = MapFiles {
        files: vec![("E.cs", "namespace N;\nclass Engine { void Go() {} }\n")],
    };
    let paths = m.list(".cs").unwrap();
    let a = assemble(parse_chunk(&m, &paths, true));
    assert!(text(&value(node(&a, "N.Engine.Go"), "_code")).contains("void Go()"));
}

/// Chunk boundaries do not change the answer: the facts are resolved once,
/// over every chunk, in the order the host gives them.
#[test]
fn chunking_does_not_change_the_answer() {
    let files = vec![
        (
            "H.cs",
            "namespace N;\nclass Helper { public void Spin() {} }\n",
        ),
        (
            "E.cs",
            "namespace N;\nclass Engine { void Go() { new Helper().Spin(); } }\n",
        ),
    ];
    let m = MapFiles {
        files: files.clone(),
    };
    let whole = assemble(parse_chunk(&m, &m.list(".cs").unwrap(), false));
    let split = {
        let mut facts = parse_chunk(&m, &["E.cs".to_string()], false);
        facts.extend(parse_chunk(&m, &["H.cs".to_string()], false));
        assemble(facts)
    };
    let keys = |a: &Assembled| {
        let mut k: Vec<String> = a.nodes.iter().map(|n| n.key.clone()).collect();
        k.sort();
        k
    };
    assert_eq!(keys(&whole), keys(&split));
}

// ---- the container's own wiring -------------------------------------------

/// A registration is a call with both types written down in it, and a
/// registered type's constructor is one the container fills.
#[test]
fn a_registration_makes_a_constructor_an_injection_point() {
    let a = run(vec![(
        "E.cs",
        r#"namespace N;
public interface IRepo {}
public class JpaRepo : IRepo {}

public class Engine
{
    private readonly IRepo _repo;
    public Engine(IRepo repo) { _repo = repo; }
}

public static class Wiring
{
    public static void Register(IServiceCollection services)
    {
        services.AddScoped<IRepo, JpaRepo>();
        services.AddScoped<Engine>();
    }
}
"#,
    )]);
    let e = edge(&a, "USES_TYPE", "N.IRepo");
    assert_eq!(text(&e.props["injected"]["$value"]), "constructor");
    assert_eq!(text(&e.props["_injected_confidence"]), "strong");
    note_containing(&a, "service registration(s) read from the calls");
    note_containing(&a, "injection point(s)");
}

/// A constructor of a type nothing registers is not an injection point: the
/// container never calls it, and whoever does chose the argument.
#[test]
fn an_unregistered_constructor_is_not_injected() {
    let a = run(vec![(
        "E.cs",
        r#"namespace N;
public interface IRepo {}
public class Engine
{
    public Engine(IRepo repo) {}
}
"#,
    )]);
    assert!(
        !edge(&a, "USES_TYPE", "N.IRepo")
            .props
            .contains_key("injected")
    );
}

/// `[FromServices]` says so outright, and outranks a constructor the
/// container happens to call.
#[test]
fn an_attributed_injection_point_outranks_a_constructor() {
    let a = run(vec![(
        "E.cs",
        r#"namespace N;
public interface IRepo {}
public class Engine
{
    private readonly IRepo _repo;
    public Engine([FromServices] IRepo repo) { _repo = repo; }
}
"#,
    )]);
    let e = edge(&a, "USES_TYPE", "N.IRepo");
    assert_eq!(text(&e.props["injected"]["$value"]), "attribute");
    assert_eq!(text(&e.props["_injected_confidence"]), "definitive");
}

/// A call through an injected service also reaches the registered
/// implementation, stamped with the rule that picked it — and the call to
/// the service stays beside it, as the source wrote it.
#[test]
fn a_call_through_an_injected_service_reaches_its_implementation() {
    let a = run(vec![(
        "E.cs",
        r#"namespace N;
public interface IRepo { void Save(); }

public class JpaRepo : IRepo
{
    public void Save() {}
}

public class Engine
{
    private readonly IRepo _repo;
    public Engine(IRepo repo) { _repo = repo; }
    public void Go() { _repo.Save(); }
}

public static class Wiring
{
    public static void Register(IServiceCollection services)
    {
        services.AddScoped<IRepo, JpaRepo>();
        services.AddScoped<Engine>();
    }
}
"#,
    )]);
    assert!(has_edge(&a, "N.Engine.Go", "CALLS", "N.IRepo.Save"));
    let w = wired(&a, "N.Engine.Go", "N.JpaRepo.Save");
    assert_eq!(text(&w.props["_resolved_by"]), "di-registration");
    assert_eq!(text(&w.props["_via"]["$value"]), "N.IRepo.Save");
    note_containing(&a, "reach the registered implementation");
}

/// A keyed registration answers the key it was registered under, and a
/// `[FromKeyedServices]` names it.
#[test]
fn a_key_names_the_implementation() {
    let a = run(vec![(
        "E.cs",
        r#"namespace N;
public interface IRepo { void Save(); }

public class FastRepo : IRepo { public void Save() {} }
public class SlowRepo : IRepo { public void Save() {} }

public class Engine
{
    private readonly IRepo _repo;
    public Engine([FromKeyedServices("fast")] IRepo repo) { _repo = repo; }
    public void Go() { _repo.Save(); }
}

public static class Wiring
{
    public static void Register(IServiceCollection services)
    {
        services.AddKeyedScoped<IRepo, FastRepo>("fast");
        services.AddKeyedScoped<IRepo, SlowRepo>("slow");
        services.AddScoped<Engine>();
    }
}
"#,
    )]);
    let w = wired(&a, "N.Engine.Go", "N.FastRepo.Save");
    assert_eq!(text(&w.props["_resolved_by"]), "di-keyed");
    assert!(!has_edge(&a, "N.Engine.Go", "CALLS", "N.SlowRepo.Save"));
}

/// Several registrations and nothing in the source saying which runs: the
/// call stops at its service, and the report says how many did. Which one
/// the container resolves depends on the order the registrations run in,
/// and that order is not written in any one file.
#[test]
fn an_ambiguous_service_stops_at_the_service() {
    let a = run(vec![(
        "E.cs",
        r#"namespace N;
public interface IRepo { void Save(); }
public class JpaRepo : IRepo { public void Save() {} }
public class MemRepo : IRepo { public void Save() {} }

public class Engine
{
    private readonly IRepo _repo;
    public Engine(IRepo repo) { _repo = repo; }
    public void Go() { _repo.Save(); }
}

public static class Wiring
{
    public static void Register(IServiceCollection services)
    {
        services.AddScoped<IRepo, JpaRepo>();
        services.AddScoped<IRepo, MemRepo>();
        services.AddScoped<Engine>();
    }
}
"#,
    )]);
    assert!(has_edge(&a, "N.Engine.Go", "CALLS", "N.IRepo.Save"));
    assert!(!has_edge(&a, "N.Engine.Go", "CALLS", "N.JpaRepo.Save"));
    note_containing(&a, "stop at their service");
}

/// A type that asks for a container-full of a service has declared it takes
/// every registration of it, so a call through one reaches them all.
#[test]
fn a_container_full_reaches_every_registration() {
    let a = run(vec![(
        "E.cs",
        r#"namespace N;
public interface IHandler { void Handle(); }
public class First : IHandler { public void Handle() {} }
public class Second : IHandler { public void Handle() {} }

public class Engine
{
    private readonly IEnumerable<IHandler> _handlers;
    public Engine(IEnumerable<IHandler> handlers) { _handlers = handlers; }
    public void Go(IHandler one) { one.Handle(); }
}

public static class Wiring
{
    public static void Register(IServiceCollection services)
    {
        services.AddScoped<IHandler, First>();
        services.AddScoped<IHandler, Second>();
        services.AddScoped<Engine>();
    }
}
"#,
    )]);
    for implementation in ["N.First.Handle", "N.Second.Handle"] {
        let w = wired(&a, "N.Engine.Go", implementation);
        assert_eq!(text(&w.props["_resolved_by"]), "di-registration-collection");
        assert_eq!(text(&w.props["_confidence"]), "medium");
    }
}

/// A service value that is not an injection point is left alone: whoever
/// built it chose the implementation.
#[test]
fn a_service_that_is_not_injected_is_left_alone() {
    let a = run(vec![(
        "E.cs",
        r#"namespace N;
public interface IRepo { void Save(); }
public class JpaRepo : IRepo { public void Save() {} }

public class Engine
{
    public void Go(IRepo repo) { repo.Save(); }
}

public static class Wiring
{
    public static void Register(IServiceCollection services)
    {
        services.AddScoped<IRepo, JpaRepo>();
    }
}
"#,
    )]);
    assert!(has_edge(&a, "N.Engine.Go", "CALLS", "N.IRepo.Save"));
    assert!(!has_edge(&a, "N.Engine.Go", "CALLS", "N.JpaRepo.Save"));
}

/// The edge targets the type that declares the member, which may be a base
/// class: that is where the code a reader wants is.
#[test]
fn a_wired_edge_points_at_the_code() {
    let a = run(vec![(
        "E.cs",
        r#"namespace N;
public interface IRepo { void Save(); }
public abstract class RepoBase { public void Save() {} }
public class JpaRepo : RepoBase, IRepo {}

public class Engine
{
    private readonly IRepo _repo;
    public Engine(IRepo repo) { _repo = repo; }
    public void Go() { _repo.Save(); }
}

public static class Wiring
{
    public static void Register(IServiceCollection services)
    {
        services.AddSingleton<IRepo, JpaRepo>();
        services.AddSingleton<Engine>();
    }
}
"#,
    )]);
    let w = wired(&a, "N.Engine.Go", "N.RepoBase.Save");
    assert_eq!(text(&w.props["_resolved_by"]), "di-registration");
}

/// `TryAddScoped` registers too — the container's own API spells the same
/// rule four ways.
#[test]
fn try_add_registers_as_well() {
    let a = run(vec![(
        "E.cs",
        r#"namespace N;
public interface IRepo { void Save(); }
public class JpaRepo : IRepo { public void Save() {} }

public class Engine
{
    private readonly IRepo _repo;
    public Engine(IRepo repo) { _repo = repo; }
    public void Go() { _repo.Save(); }
}

public static class Wiring
{
    public static void Register(IServiceCollection services)
    {
        services.TryAddTransient<IRepo, JpaRepo>();
        services.TryAddTransient<Engine>();
    }
}
"#,
    )]);
    assert_eq!(
        text(&wired(&a, "N.Engine.Go", "N.JpaRepo.Save").props["_resolved_by"]),
        "di-registration"
    );
}

/// Only an interface is dispatched through: a concrete type's method has a
/// body, so the recorded edge already points at code that runs.
#[test]
fn a_concrete_service_is_not_dispatched_through() {
    let a = run(vec![(
        "E.cs",
        r#"namespace N;
public class Cache { public void Get() {} }

public class Engine
{
    private readonly Cache _cache;
    public Engine(Cache cache) { _cache = cache; }
    public void Go() { _cache.Get(); }
}

public static class Wiring
{
    public static void Register(IServiceCollection services)
    {
        services.AddSingleton<Cache>();
        services.AddSingleton<Engine>();
    }
}
"#,
    )]);
    assert_eq!(
        a.edges
            .iter()
            .filter(|e| e.src == "N.Engine.Go" && e.ty == "CALLS")
            .count(),
        1
    );
    assert!(has_edge(&a, "N.Engine.Go", "CALLS", "N.Cache.Get"));
}

/// A C# 12 primary constructor states a type's dependencies with no field
/// behind them, and the container fills it the same way.
#[test]
fn a_primary_constructor_is_an_injection_point() {
    let a = run(vec![(
        "E.cs",
        r#"namespace N;
public interface IRepo { void Save(); }
public class JpaRepo : IRepo { public void Save() {} }

public class Engine(IRepo repo)
{
    public void Go() { repo.Save(); }
}

public static class Wiring
{
    public static void Register(IServiceCollection services)
    {
        services.AddScoped<IRepo, JpaRepo>();
        services.AddScoped<Engine>();
    }
}
"#,
    )]);
    let e = edge(&a, "USES_TYPE", "N.IRepo");
    assert_eq!(text(&e.props["injected"]["$value"]), "constructor");
    assert!(has_edge(&a, "N.Engine.Go", "CALLS", "N.IRepo.Save"));
    assert_eq!(
        text(&wired(&a, "N.Engine.Go", "N.JpaRepo.Save").props["_resolved_by"]),
        "di-registration"
    );
}
