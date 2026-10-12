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
