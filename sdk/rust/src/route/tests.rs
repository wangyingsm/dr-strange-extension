//! The convention's own tests. Each case is a path a real framework writes.

use super::*;

/// The four spellings of one variable segment are one route.
#[test]
fn every_frameworks_variable_normalizes_the_same() {
    for written in [
        "/orders/{id}",        // Spring, Go 1.22, gorilla/mux
        "/orders/{id:[0-9]+}", // a regex constraint
        "/orders/:id",         // Express, gin, Nest
        "/orders/${id}",       // a TS template hole
        "/orders/<int:id>",    // Flask, Django
        "/orders/*",           // an Ant-style wildcard
    ] {
        assert_eq!(normalize(written), "/orders/{}", "{written}");
    }
}

/// A segment that holds a hole without being one is unknowable too.
#[test]
fn a_segment_around_a_hole_is_a_variable() {
    assert_eq!(normalize("/files/report-${id}.pdf"), "/files/{}");
    assert_eq!(normalize("/files/{name}.json"), "/files/{}");
}

/// Separators, a trailing slash, an origin and a query say nothing about which
/// handler serves the path.
#[test]
fn the_shape_around_the_path_is_dropped() {
    assert_eq!(normalize("/api//orders/"), "/api/orders");
    assert_eq!(normalize("api/orders"), "/api/orders");
    assert_eq!(normalize("/api/orders?page=2"), "/api/orders");
    assert_eq!(normalize("/api/orders#top"), "/api/orders");
    assert_eq!(
        normalize("https://api.example.com/v1/orders/{id}"),
        "/v1/orders/{}"
    );
    assert_eq!(normalize("https://api.example.com"), "/");
    assert_eq!(normalize("/"), "/");
}

/// A client's `baseURL` and a call's path make one path, each side's slash
/// forgiven.
#[test]
fn a_base_and_a_path_join_once() {
    assert_eq!(join("/api", "/orders"), "/api/orders");
    assert_eq!(join("/api/", "orders"), "/api/orders");
    assert_eq!(join("", "/orders"), "/orders");
    assert_eq!(
        normalize(&join("https://host/api/", "/orders/:id")),
        "/api/orders/{}"
    );
}

/// The verb a framework's own name carries, wherever it puts the word.
#[test]
fn a_verb_is_read_from_the_name_that_carries_it() {
    for (name, want) in [
        ("GetMapping", Verb::Get),
        ("get", Verb::Get),
        ("GET", Verb::Get),
        ("Get", Verb::Get),
        ("HttpGet", Verb::Get),
        ("getJSON", Verb::Get),
        ("PostMapping", Verb::Post),
        ("post", Verb::Post),
        ("DeleteMapping", Verb::Delete),
        ("patch", Verb::Patch),
        ("PutMapping", Verb::Put),
        ("head", Verb::Head),
        ("options", Verb::Options),
    ] {
        assert_eq!(Verb::from_name(name), Some(want), "{name}");
    }
    // A name that names no verb says so, which is a fact about the call site.
    for name in ["HandleFunc", "route", "request", "path", "RequestMapping"] {
        assert_eq!(Verb::from_name(name), None, "{name}");
    }
}

/// `*` is what a declaration means when it names no verb, and what it
/// matches.
#[test]
fn no_verb_is_written_as_the_one_that_matches_all() {
    assert_eq!(route(None, "/orders"), "* /orders");
    assert_eq!(Verb::Any.as_str(), "*");
    assert_eq!(route(Some(Verb::Get), "/orders/:id"), "GET /orders/{}");
}

/// A route reference is recognised by its shape, broadly on purpose: a
/// project-local wrapper is only visible this way.
#[test]
fn a_path_is_recognised_by_its_shape() {
    for yes in [
        "/orders",
        "/bridge/check/{id}",
        "https://api.example.com/v1",
        "/",
    ] {
        assert!(looks_like_path(yes), "{yes}");
    }
    for no in ["orders", "SELECT 1", "", "  ", "a/b", "GET /orders"] {
        assert!(!looks_like_path(no), "{no}");
    }
}

/// The property splits back into the two things it holds — the whole contract
/// between the plugins that write it and the verb that reads it.
#[test]
fn the_property_is_a_verb_a_space_and_a_path() {
    let written = route(Some(Verb::Post), "/api/orders/");
    let (verb, path) = written.split_once(' ').expect("verb and path");
    assert_eq!(verb, "POST");
    assert_eq!(path, "/api/orders");
}

/// One record, both halves, and the comparable form derived from it — the
/// type every Rust-written plugin passes between its two phases.
#[test]
fn a_route_ref_states_a_symbol_and_what_it_wrote() {
    let r = RouteRef {
        symbol: "web/src/api.one".into(),
        verb: Some(Verb::Get),
        written: "/api/orders/${}/items".into(),
        line: 12,
    };
    assert_eq!(r.route(), "GET /api/orders/{}/items");
    let round: RouteRef = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
    assert_eq!(round.route(), r.route());
    assert_eq!(
        round.written, r.written,
        "the source's own spelling survives"
    );
    // No verb survives as no verb, rather than becoming a guess.
    let bare = RouteRef { verb: None, ..r };
    assert_eq!(bare.route(), "* /api/orders/{}/items");
}

/// The fold and the two properties, done once here for every plugin: one
/// symbol's routes written, a symbol that states none left alone, and an
/// account counted from what was written.
#[test]
fn one_fold_writes_both_properties_and_the_account() {
    let refs = vec![
        RouteRef {
            symbol: "api.one".into(),
            verb: Some(Verb::Get),
            written: "/api/orders/${}".into(),
            line: 3,
        },
        // The same route twice is one route.
        RouteRef {
            symbol: "api.one".into(),
            verb: Some(Verb::Get),
            written: "/api/orders/${}".into(),
            line: 9,
        },
        RouteRef {
            symbol: "api.one".into(),
            verb: None,
            written: "/health".into(),
            line: 11,
        },
    ];
    let asked = Routes::of(Side::Requests, &refs);

    let mut props = Map::new();
    assert_eq!(asked.write("api.one", &mut props), 2);
    assert_eq!(
        props["requests"]["$value"],
        serde_json::json!(["* /health", "GET /api/orders/{}"])
    );
    assert_eq!(
        props["requests_as_written"]["$value"],
        serde_json::json!(["/api/orders/${}", "/health"]),
        "the source's own spelling, beside the comparable form"
    );

    let mut none = Map::new();
    assert_eq!(asked.write("api.two", &mut none), 0);
    assert!(none.is_empty(), "no property rather than an empty one");

    assert!(asked.note(2).unwrap().contains("2 route(s) asked for"));
    assert_eq!(asked.note(0), None, "nothing to account for");
}

/// The declaration side writes the other pair of properties — the same fold,
/// told apart only by which half it holds.
#[test]
fn the_declaration_side_writes_serves() {
    let refs = vec![RouteRef {
        symbol: "com.acme.CheckController.check".into(),
        verb: Some(Verb::Get),
        written: "/bridge/check/{id}".into(),
        line: 42,
    }];
    let served = Routes::of(Side::Serves, &refs);
    let mut props = Map::new();
    assert_eq!(
        served.write("com.acme.CheckController.check", &mut props),
        1
    );
    assert_eq!(
        props["serves"]["$value"],
        serde_json::json!(["GET /bridge/check/{}"])
    );
    assert_eq!(
        props["serves_as_written"]["$value"],
        serde_json::json!(["/bridge/check/{id}"])
    );
    assert!(served.note(1).unwrap().contains("declared by a handler"));
}

/// A mount gives a router a prefix; it serves nothing and asks for nothing,
/// and recording its path as a route is wrong in both directions.
#[test]
fn a_mount_is_not_a_route() {
    for name in [
        "use",
        "nest",
        "nest_service",
        "merge",
        "scope",
        "mount",
        "Group",
        "group",
        "include_router",
        "register_blueprint",
    ] {
        assert!(mounts(name), "{name}");
    }
    // `route` says nothing on its own: axum's is a route, chi's is a mount.
    for name in ["route", "Route", "get", "handle", "HandleFunc", "service"] {
        assert!(!mounts(name), "{name}");
    }
}

/// A wildcard path is a pattern a framework guards with, not one a client
/// asks for — and left in, it would match the very handler it guards.
#[test]
fn a_pattern_is_not_a_request() {
    let refs = vec![
        RouteRef {
            symbol: "cfg.addCors".into(),
            verb: None,
            written: "/api/**".into(),
            line: 3,
        },
        RouteRef {
            symbol: "web.load".into(),
            verb: Some(Verb::Get),
            written: "/api/orders".into(),
            line: 9,
        },
    ];
    let asked = Routes::of(Side::Requests, &refs);
    let mut props = Map::new();
    assert_eq!(asked.write("cfg.addCors", &mut props), 0);
    assert!(props.is_empty(), "a pattern writes no request");
    assert_eq!(asked.write("web.load", &mut props), 1);
    let note = asked.note(1).unwrap();
    assert!(
        note.contains("1 wildcard pattern(s) left out"),
        "the account says what it dropped: {note}"
    );

    // A declaration may legitimately be written that way, and is kept.
    let served = Routes::of(Side::Serves, &refs);
    let mut props = Map::new();
    assert_eq!(served.write("cfg.addCors", &mut props), 1);
    assert_eq!(
        props["serves"]["$value"],
        serde_json::json!(["* /api/{}"]),
        "an Ant-style declaration reduces its wildcard like any variable"
    );
}
