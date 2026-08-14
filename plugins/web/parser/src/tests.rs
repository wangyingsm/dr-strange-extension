//! The parser's own tests, running natively.

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
    let mut paths = Vec::new();
    for ext in EXTENSIONS {
        paths.extend(m.list(&format!(".{ext}")).unwrap());
    }
    paths.sort();
    paths.dedup();
    assemble(parse_chunk(&m, &paths, Options::default()))
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
        .unwrap_or_else(|| panic!("no {ty} edge to {dst}"))
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

const PAGE: &str = r#"<!doctype html>
<html>
<head>
  <title>Home</title>
  <link rel="stylesheet" href="css/site.css">
  <link rel="icon" href="favicon.ico">
</head>
<body>
  <div id="map" class="panel wide"></div>
  <a href="about.html">About</a>
  <a href="about.html#team">Team</a>
  <a href="https://example.com">Out</a>
  <script src="app.js"></script>
</body>
</html>
"#;

const CSS: &str = r#":root {
  --brand: #6c5ce7;
  --pad: 4px;
}

.panel {
  color: var(--brand);
}

.wide { width: 100%; }
"#;

// ---- shapes ---------------------------------------------------------------

/// A page is a node with its title; an id'd element keys as the URL
/// fragment — the web's own address for it.
#[test]
fn pages_and_elements_take_web_addresses() {
    let a = run(vec![
        ("index.html", PAGE),
        (
            "about.html",
            "<html><body><h2 id=\"team\">Team</h2></body></html>",
        ),
        ("css/site.css", CSS),
    ]);
    let p = node(&a, "index.html");
    assert_eq!(p.label, "Page");
    assert_eq!(text(&p.props["title"]), "Home");

    let el = node(&a, "index.html#map");
    assert_eq!(el.label, "Element");
    assert_eq!(text(&el.props["tag"]), "div");
    assert!(has_edge(&a, "index.html", "CONTAINS", "index.html#map"));
}

/// A stylesheet declares its vocabulary: classes as `{file}::.name`,
/// custom properties as Consts with values as written.
#[test]
fn stylesheets_declare_classes_and_custom_properties() {
    let a = run(vec![("css/site.css", CSS)]);
    let s = node(&a, "css/site.css");
    assert_eq!(s.label, "Stylesheet");
    assert_eq!(s.props["rules"], Value::from(3u64));

    assert_eq!(node(&a, "css/site.css::.panel").label, "Class");
    let brand = node(&a, "css/site.css::--brand");
    assert_eq!(brand.label, "Const");
    assert_eq!(text(&brand.props["value"]), "#6c5ce7");
    // var(--brand) inside .panel is a use of the property.
    assert!(has_edge(
        &a,
        "css/site.css",
        "USES",
        "css/site.css::--brand"
    ));
}

/// The cross-file heart: `class="panel"` in markup binds to the stylesheet
/// that defines `.panel` — resolved at assemble, where both sides meet.
#[test]
fn markup_classes_bind_to_their_stylesheet() {
    let a = run(vec![("index.html", PAGE), ("css/site.css", CSS)]);
    // The id'd element carries its own classes.
    assert!(has_edge(
        &a,
        "index.html#map",
        "STYLED_BY",
        "css/site.css::.panel"
    ));
    assert!(has_edge(
        &a,
        "index.html#map",
        "STYLED_BY",
        "css/site.css::.wide"
    ));
    // The stylesheet link is an import; the icon link is bookkeeping.
    assert!(has_edge(&a, "index.html", "IMPORTS", "css/site.css"));
    assert!(!a.edges.iter().any(|e| e.dst.contains("favicon")));
}

/// Links resolve to pages, and to the id'd element itself when the fragment
/// names one this run parsed. External links are counted, not guessed.
#[test]
fn links_resolve_pages_and_fragments() {
    let a = run(vec![
        ("index.html", PAGE),
        (
            "about.html",
            "<html><body><h2 id=\"team\">Team</h2></body></html>",
        ),
        ("css/site.css", CSS),
    ]);
    assert!(has_edge(&a, "index.html", "LINKS", "about.html"));
    assert!(has_edge(&a, "index.html", "LINKS", "about.html#team"));
    note_containing(&a, "outside this tree");
}

/// A class defined in two stylesheets is ambiguous — which one loads is the
/// page's business, counted, never guessed. A page's own `<style>` is
/// nearest and wins.
#[test]
fn class_binding_is_nearest_then_unique() {
    let a = run(vec![
        (
            "index.html",
            "<html><head><style>.local { color: red; }</style></head>\
             <body><div class=\"local\"></div><div class=\"dup\"></div></body></html>",
        ),
        ("a.css", ".dup { color: blue; }"),
        ("b.css", ".dup { color: green; }"),
    ]);
    // The page's own <style> defines .local in the page's namespace.
    assert_eq!(node(&a, "index.html::.local").label, "Class");
    assert!(has_edge(
        &a,
        "index.html",
        "STYLED_BY",
        "index.html::.local"
    ));
    // .dup is defined twice across stylesheets — counted, no edge.
    assert!(
        !a.edges
            .iter()
            .any(|e| e.ty == "STYLED_BY" && e.dst.contains("dup"))
    );
    note_containing(&a, "more than one stylesheet");
}

/// Inline scripts are shallow-parsed: functions keyed `page::name` — the
/// family's file-scope form — and the calls between them bound within the
/// page, because an inline script's world is its page.
#[test]
fn inline_scripts_are_shallow_parsed() {
    let a = run(vec![(
        "index.html",
        r#"<html><body>
<script>
function boot() {
  render();
  fetch("/api");
}
const render = () => {};
</script>
</body></html>"#,
    )]);
    let f = node(&a, "index.html::boot");
    assert_eq!(f.label, "Function");
    assert_eq!(text(&f.props["signature"]), "function boot()");
    node(&a, "index.html::render");
    assert!(has_edge(
        &a,
        "index.html::boot",
        "CALLS",
        "index.html::render"
    ));
    // fetch() is the platform's, not the page's — counted.
    note_containing(&a, "left unresolved");
    assert!(has_edge(&a, "index.html", "CONTAINS", "index.html::boot"));
}

/// A CDN import keeps the URL as its identity — an external File.
#[test]
fn cdn_imports_are_external() {
    let a = run(vec![(
        "index.html",
        "<html><head><script src=\"https://unpkg.com/htmx.org@2\"></script></head></html>",
    )]);
    let ext = node(&a, "https://unpkg.com/htmx.org@2");
    assert_eq!(ext.label, "File");
    assert!(ext.extra_labels.contains(&"External".to_string()));
    assert!(has_edge(
        &a,
        "index.html",
        "IMPORTS",
        "https://unpkg.com/htmx.org@2"
    ));
}

/// CSS `@import` chains stylesheets; relative paths resolve from the
/// importing file.
#[test]
fn css_imports_chain() {
    let a = run(vec![
        ("css/site.css", "@import \"base.css\";\n.x { color: red; }"),
        ("css/base.css", ":root { --pad: 2px; }"),
    ]);
    assert!(has_edge(&a, "css/site.css", "IMPORTS", "css/base.css"));
}

// ---- provenance and robustness --------------------------------------------

/// Every definition knows its file and line, and every written relation the
/// line it is written on — inline bodies offset by their position in the
/// page.
#[test]
fn lines_are_recorded_with_inline_offsets() {
    let a = run(vec![(
        "p.html",
        "<html><head>\n<style>\n.late { color: red; }\n</style>\n</head>\n<body class=\"late\">\n<script>\nfunction go() {}\n</script>\n</body></html>",
    )]);
    // .late is defined on page line 3 (1 inside the style body + 2 offset).
    let c = node(&a, "p.html::.late");
    assert_eq!(c.props["line"], Value::from(3u64));
    // go() is defined on page line 8.
    let f = node(&a, "p.html::go");
    assert_eq!(f.props["line"], Value::from(8u64));
    assert_eq!(
        edge(&a, "STYLED_BY", "p.html::.late").props["line"],
        Value::from(6u64)
    );
}

/// The result must not depend on where the chunk boundaries fell, and the
/// same tree twice gives the same facts.
#[test]
fn deterministic_and_chunk_independent() {
    let files = vec![("index.html", PAGE), ("css/site.css", CSS)];
    let m = MapFiles { files };
    let mut paths = vec!["css/site.css".to_string(), "index.html".to_string()];
    paths.sort();

    let one = assemble(parse_chunk(&m, &paths, Options::default()));
    let mut split = Vec::new();
    for p in &paths {
        split.extend(parse_chunk(&m, std::slice::from_ref(p), Options::default()));
    }
    let other = assemble(split);
    let again = assemble(parse_chunk(&m, &paths, Options::default()));

    let dump = |a: &Assembled| serde_json::to_string(&(&a.nodes, &a.edges, &a.notes)).unwrap();
    assert_eq!(dump(&one), dump(&other));
    assert_eq!(dump(&one), dump(&again));
}

/// A pushed document parses alone.
#[test]
fn a_document_parses_alone() {
    let facts = parse_document(
        "snippet.html",
        b"<html><body><div id=\"solo\"></div></body></html>",
        Options::default(),
    );
    let a = assemble(facts);
    node(&a, "snippet.html#solo");
}

/// include_source attaches inline-script functions as written.
#[test]
fn include_source_attaches_inline_functions() {
    let m = MapFiles {
        files: vec![(
            "p.html",
            "<html><body><script>function shown() { return 42; }</script></body></html>",
        )],
    };
    let paths = m.list(".html").unwrap();
    let a = assemble(parse_chunk(
        &m,
        &paths,
        Options {
            include_source: true,
            ..Default::default()
        },
    ));
    let f = node(&a, "p.html::shown");
    assert!(
        text(&f.props["_code"]["$value"]).contains("return 42"),
        "{:?}",
        f.props
    );
}

/// A minified stylesheet is a build artifact of its readable sibling, not a
/// second opinion: when both define `.btn`, the source definition wins the
/// binding; the minified one only speaks when nothing else defines the name.
#[test]
fn minified_definitions_yield_to_source() {
    let a = run(vec![
        (
            "index.html",
            "<html><body><div class=\"btn\"></div><div class=\"only-min\"></div></body></html>",
        ),
        ("css/site.css", ".btn { color: red; }"),
        ("css/site.min.css", ".btn{color:red}.only-min{width:1px}"),
    ]);
    assert!(has_edge(
        &a,
        "index.html",
        "STYLED_BY",
        "css/site.css::.btn"
    ));
    assert!(has_edge(
        &a,
        "index.html",
        "STYLED_BY",
        "css/site.min.css::.only-min"
    ));
}

/// Elements nest the way the DOM nests: an id'd element inside another
/// hangs from its nearest node ancestor, not flatly from the page — and a
/// class on an anonymous child binds from that nearest scope.
#[test]
fn elements_nest_like_the_dom() {
    let a = run(vec![
        (
            "index.html",
            "<html><body>\
             <div id=\"outer\"><section><div id=\"inner\" class=\"boxed\"></div></section></div>\
             </body></html>",
        ),
        ("s.css", ".boxed { border: 1px; }"),
    ]);
    assert!(has_edge(&a, "index.html", "CONTAINS", "index.html#outer"));
    assert!(has_edge(
        &a,
        "index.html#outer",
        "CONTAINS",
        "index.html#inner"
    ));
    assert!(!has_edge(&a, "index.html", "CONTAINS", "index.html#inner"));
    assert!(has_edge(
        &a,
        "index.html#inner",
        "STYLED_BY",
        "s.css::.boxed"
    ));
}

/// A class node carries the rules it appears in, as written — the shape the
/// dashboard shows as "N items". Minified stylesheets skip it: their
/// one-line rules are noise, not reading material.
#[test]
fn class_nodes_carry_their_rules() {
    let a = run(vec![(
        "s.css",
        ".btn { color: red; }\n.btn:hover { color: blue; }\n.btn.big { width: 2em; }\n",
    )]);
    let c = node(&a, "s.css::.btn");
    let rules: Vec<&str> = c.props["rules"]["$value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        rules,
        vec![
            ".btn { color: red; }",
            ".btn:hover { color: blue; }",
            ".btn.big { width: 2em; }",
        ]
    );
}

/// `dom = "full"` turns the whole tree into nodes, keyed positionally —
/// deterministic within a snapshot, and explicitly opt-in because
/// positional keys churn when markup shifts.
#[test]
fn full_dom_is_opt_in() {
    let m = MapFiles {
        files: vec![(
            "p.html",
            "<html><body><div><span class=\"x\"></span></div><div id=\"named\"></div></body></html>",
        )],
    };
    let paths = m.list(".html").unwrap();

    // Default: only the id'd element is a node.
    let restrained = assemble(parse_chunk(&m, &paths, Options::default()));
    assert!(!restrained.nodes.iter().any(|n| n.key.contains("span")));
    node(&restrained, "p.html#named");

    // Full: every element, nested, position-keyed; ids keep their fragment
    // form.
    let full = assemble(parse_chunk(
        &m,
        &paths,
        Options {
            dom: true,
            ..Default::default()
        },
    ));
    let span = node(&full, "p.html::html[1]>body[1]>div[1]>span[1]");
    assert_eq!(span.label, "Element");
    assert!(has_edge(
        &full,
        "p.html::html[1]>body[1]>div[1]",
        "CONTAINS",
        "p.html::html[1]>body[1]>div[1]>span[1]"
    ));
    node(&full, "p.html#named");
}
