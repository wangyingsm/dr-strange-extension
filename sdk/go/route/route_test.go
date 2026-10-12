package route

import (
	"reflect"
	"strings"
	"testing"
)

// Each case is a path a real framework writes, and they are one route.
func TestEveryFrameworksVariableNormalizesTheSame(t *testing.T) {
	for _, written := range []string{
		"/orders/{id}",        // Go 1.22, gorilla/mux, Spring
		"/orders/{id:[0-9]+}", // a regex constraint
		"/orders/:id",         // gin, echo, Express
		"/orders/${id}",       // a TS template hole
		"/orders/<int:id>",    // Flask, Django
		"/orders/*",           // an Ant-style wildcard
	} {
		if got := Normalize(written); got != "/orders/{}" {
			t.Errorf("Normalize(%q) = %q", written, got)
		}
	}
}

// A segment that holds a hole without being one is unknowable too.
func TestASegmentAroundAHoleIsAVariable(t *testing.T) {
	for written, want := range map[string]string{
		"/files/report-${id}.pdf": "/files/{}",
		"/files/{name}.json":      "/files/{}",
	} {
		if got := Normalize(written); got != want {
			t.Errorf("Normalize(%q) = %q, want %q", written, got, want)
		}
	}
}

// Separators, a trailing slash, an origin and a query say nothing about which
// handler serves the path.
func TestTheShapeAroundThePathIsDropped(t *testing.T) {
	for written, want := range map[string]string{
		"/api//orders/":                          "/api/orders",
		"api/orders":                             "/api/orders",
		"/api/orders?page=2":                     "/api/orders",
		"/api/orders#top":                        "/api/orders",
		"https://api.example.com/v1/orders/{id}": "/v1/orders/{}",
		"https://api.example.com":                "/",
		"/":                                      "/",
	} {
		if got := Normalize(written); got != want {
			t.Errorf("Normalize(%q) = %q, want %q", written, got, want)
		}
	}
}

// A client's base URL and a call's path make one path, each side's slash
// forgiven.
func TestABaseAndAPathJoinOnce(t *testing.T) {
	for _, c := range []struct{ base, path, want string }{
		{"/api", "/orders", "/api/orders"},
		{"/api/", "orders", "/api/orders"},
		{"", "/orders", "/orders"},
	} {
		if got := Join(c.base, c.path); got != c.want {
			t.Errorf("Join(%q, %q) = %q, want %q", c.base, c.path, got, c.want)
		}
	}
	if got := Normalize(Join("https://host/api/", "/orders/:id")); got != "/api/orders/{}" {
		t.Errorf("joined then normalized = %q", got)
	}
}

// The verb a framework's own name carries, wherever it puts the word.
func TestAVerbIsReadFromTheNameThatCarriesIt(t *testing.T) {
	for name, want := range map[string]Verb{
		"Get": Get, "get": Get, "GET": Get, "getJSON": Get, "HandleGet": Get,
		"Post": Post, "post": Post, "Delete": Delete, "patch": Patch,
		"Put": Put, "head": Head, "options": Options,
	} {
		got, ok := FromName(name)
		if !ok || got != want {
			t.Errorf("FromName(%q) = %q, %v; want %q", name, got, ok, want)
		}
	}
	// A name that names no verb says so, which is a fact about the call site.
	for _, name := range []string{"HandleFunc", "route", "request", "Do", "ServeHTTP"} {
		if got, ok := FromName(name); ok {
			t.Errorf("FromName(%q) = %q, true; want no verb", name, got)
		}
	}
}

// `*` is what a declaration means when it names no verb, and what it matches.
func TestNoVerbIsWrittenAsTheOneThatMatchesAll(t *testing.T) {
	if got := Route("", "/orders"); got != "* /orders" {
		t.Errorf("Route(no verb) = %q", got)
	}
	if got := Route(Get, "/orders/:id"); got != "GET /orders/{}" {
		t.Errorf("Route(Get) = %q", got)
	}
}

// A route reference is recognised by its shape, broadly on purpose: a
// project-local wrapper is only visible this way.
func TestAPathIsRecognisedByItsShape(t *testing.T) {
	for _, yes := range []string{"/orders", "/bridge/check/{id}", "https://api.example.com/v1", "/"} {
		if !LooksLikePath(yes) {
			t.Errorf("LooksLikePath(%q) = false", yes)
		}
	}
	for _, no := range []string{"orders", "SELECT 1", "", "  ", "a/b", "GET /orders"} {
		if LooksLikePath(no) {
			t.Errorf("LooksLikePath(%q) = true", no)
		}
	}
}

// The fold and the two properties, done once here for every Go plugin: one
// symbol's routes written, a symbol that states none left alone, and an
// account counted from what was written.
func TestOneFoldWritesBothPropertiesAndTheAccount(t *testing.T) {
	refs := []Ref{
		{Symbol: "pkg.one", Verb: Get, Written: "/api/orders/{id}", Line: 3},
		// The same route twice is one route.
		{Symbol: "pkg.one", Verb: Get, Written: "/api/orders/{id}", Line: 9},
		{Symbol: "pkg.one", Written: "/health", Line: 11},
	}
	asked := Of(Requests, refs)

	props := map[string]any{}
	if n := asked.Write("pkg.one", props); n != 2 {
		t.Fatalf("Write recorded %d routes, want 2", n)
	}
	want := []string{"* /health", "GET /api/orders/{}"}
	got := props["requests"].(map[string]any)["$value"].([]string)
	if !reflect.DeepEqual(got, want) {
		t.Errorf("requests = %v, want %v", got, want)
	}
	wantWritten := []string{"/api/orders/{id}", "/health"}
	gotWritten := props["requests_as_written"].(map[string]any)["$value"].([]string)
	if !reflect.DeepEqual(gotWritten, wantWritten) {
		t.Errorf("requests_as_written = %v, want %v", gotWritten, wantWritten)
	}

	none := map[string]any{}
	if n := asked.Write("pkg.two", none); n != 0 || len(none) != 0 {
		t.Errorf("a symbol stating none got %d routes and %d props", n, len(none))
	}

	if !strings.Contains(asked.Note(2), "2 route(s) asked for") {
		t.Errorf("Note(2) = %q", asked.Note(2))
	}
	if asked.Note(0) != "" {
		t.Errorf("Note(0) = %q, want nothing to account for", asked.Note(0))
	}
}

// The declaration side writes the other pair of properties — the same fold,
// told apart only by which half it holds.
func TestTheDeclarationSideWritesServes(t *testing.T) {
	served := Of(Serves, []Ref{
		{Symbol: "pkg.Check", Verb: Get, Written: "/bridge/check/{id}", Line: 42},
	})
	props := map[string]any{}
	if n := served.Write("pkg.Check", props); n != 1 {
		t.Fatalf("Write recorded %d routes, want 1", n)
	}
	got := props["serves"].(map[string]any)["$value"].([]string)
	if !reflect.DeepEqual(got, []string{"GET /bridge/check/{}"}) {
		t.Errorf("serves = %v", got)
	}
	if _, ok := props["serves_as_written"]; !ok {
		t.Error("the source's own spelling is kept beside the comparable form")
	}
	if !strings.Contains(served.Note(1), "declared by a handler") {
		t.Errorf("Note = %q", served.Note(1))
	}
}

// A mount gives a router a prefix; it serves nothing and asks for nothing.
func TestAMountIsNotARoute(t *testing.T) {
	for _, name := range []string{
		"use", "nest", "nest_service", "merge", "scope", "mount", "Group", "group",
		"include", "include_router", "register_blueprint",
	} {
		if !Mounts(name) {
			t.Errorf("Mounts(%q) = false", name)
		}
	}
	// `Route` says nothing on its own: axum's is a route, chi's is a mount.
	for _, name := range []string{"route", "Route", "get", "HandleFunc", "service"} {
		if Mounts(name) {
			t.Errorf("Mounts(%q) = true", name)
		}
	}
}

// A wildcard path is a pattern a framework guards with, not one a client asks
// for — and left in, it would match the very handler it guards.
func TestAPatternIsNotARequest(t *testing.T) {
	refs := []Ref{
		{Symbol: "cfg.addCors", Written: "/api/**", Line: 3},
		{Symbol: "web.load", Verb: Get, Written: "/api/orders", Line: 9},
	}
	asked := Of(Requests, refs)
	props := map[string]any{}
	if n := asked.Write("cfg.addCors", props); n != 0 || len(props) != 0 {
		t.Errorf("a pattern wrote %d request(s) and %d prop(s)", n, len(props))
	}
	if n := asked.Write("web.load", props); n != 1 {
		t.Errorf("the concrete path wrote %d", n)
	}
	if note := asked.Note(1); !strings.Contains(note, "1 wildcard pattern(s) left out") {
		t.Errorf("the account says what it dropped: %q", note)
	}

	// A declaration may legitimately be written that way, and is kept.
	served := Of(Serves, refs)
	props = map[string]any{}
	if n := served.Write("cfg.addCors", props); n != 1 {
		t.Errorf("an Ant-style declaration wrote %d", n)
	}
}
