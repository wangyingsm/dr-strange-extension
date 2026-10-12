// Package route is the HTTP route convention: what a declaration and a
// request have to agree on before anything can match them.
//
// A route is a statement a language makes, and every mainstream framework
// makes it in one of three shapes — an annotation carrying a path, a
// registration call whose callee names the verb, or a client call with a path
// literal. What differs between frameworks is the spelling of the path's
// variable segments, and that is exactly what this package erases:
// `/orders/{id}`, `/orders/:id` and `/orders/<int:id>` are one route, written
// three ways.
//
// This is the Go mirror of the Rust SDK's `dr_strange_ext::route`, and the two
// must agree character for character: a plugin written in either language
// writes the same property, and the `wire` verb compares them without knowing
// which produced it. Nothing here mints a node — matching a request to a
// declaration is a question asked of the graph, not an edge drawn into it.
package route

import (
	"fmt"
	"sort"
	"strings"
)

// Verb is an HTTP verb, as the frameworks spell it.
//
// Any is not a verb a client sends: it is what a declaration means when it
// names no verb (`http.HandleFunc`, Spring's bare `@RequestMapping`), and what
// a request means when a project-local wrapper hides it. It matches every
// verb, and a reader is told that it did.
type Verb string

// The verbs, spelled as the property spells them. Any is `*`, which is what
// it matches.
const (
	Get     Verb = "GET"
	Post    Verb = "POST"
	Put     Verb = "PUT"
	Patch   Verb = "PATCH"
	Delete  Verb = "DELETE"
	Head    Verb = "HEAD"
	Options Verb = "OPTIONS"
	Any     Verb = "*"
)

// verbWords are matched against a framework's own name, longest first so
// `options` is never read as the `opt` of something else.
var verbWords = []struct {
	word string
	verb Verb
}{
	{"delete", Delete},
	{"options", Options},
	{"patch", Patch},
	{"post", Post},
	{"head", Head},
	{"put", Put},
	{"get", Get},
}

// FromName reads the verb a framework's own name carries: `Get`, `GET`,
// `HandleGet`, `getJSON`, `get_async`. The second result is false when the
// name names no verb, which is a fact about the call site rather than a
// failure.
func FromName(name string) (Verb, bool) {
	lower := strings.ToLower(name)
	for _, w := range verbWords {
		if lower == w.word ||
			strings.HasPrefix(lower, w.word) ||
			strings.HasSuffix(lower, w.word) ||
			strings.Contains(lower, "_"+w.word) {
			return w.verb, true
		}
	}
	return Any, false
}

// LooksLikePath reports whether a string could be a route: a leading `/`, or
// an absolute http(s) URL, and no whitespace.
//
// Deliberately broad: a path-shaped literal passed to any call is a route
// reference whatever the callee is named, which is the only way a
// project-local `request("/x")` wrapper is ever visible. A file path that
// slips through matches no declaration and is counted — a named miss, not a
// wrong edge.
func LooksLikePath(s string) bool {
	s = strings.TrimSpace(s)
	if s == "" || strings.ContainsAny(s, " \t\n\r\v\f") {
		return false
	}
	return strings.HasPrefix(s, "/") ||
		strings.HasPrefix(s, "http://") ||
		strings.HasPrefix(s, "https://")
}

// Normalize reduces one path to the form a declaration and a request can be
// compared in: origin and query dropped, duplicate separators collapsed, no
// trailing slash, and every variable segment reduced to `{}`, however its
// framework spells it.
func Normalize(p string) string {
	rest := strings.TrimSpace(p)
	// An absolute URL names the same route as the path inside it; the host is
	// a deployment's business, not the graph's.
	for _, scheme := range []string{"http://", "https://"} {
		if after, ok := strings.CutPrefix(rest, scheme); ok {
			if at := strings.Index(after, "/"); at >= 0 {
				rest = after[at:]
			} else {
				rest = "/"
			}
			break
		}
	}
	// A query or a fragment says nothing about which handler serves it.
	for _, cut := range []string{"?", "#"} {
		if at := strings.Index(rest, cut); at >= 0 {
			rest = rest[:at]
		}
	}

	var out strings.Builder
	out.WriteString("/")
	for _, segment := range strings.Split(rest, "/") {
		if segment == "" {
			continue
		}
		if out.Len() > 1 {
			out.WriteString("/")
		}
		out.WriteString(normalizeSegment(segment))
	}
	return out.String()
}

// normalizeSegment reduces one segment, with a variable in any framework's
// spelling written `{}`.
//
// `{id}` (Go 1.22, gorilla/mux, Spring), `{id:[0-9]+}` (a regex constraint),
// `:id` (gin, echo, Express), `${id}` (a TS template hole), `<int:id>`
// (Flask, Django), `*` and `**` (an Ant-style wildcard — not a variable, but
// it stands for whatever a request puts there, which is the same thing here).
func normalizeSegment(segment string) string {
	variable := strings.HasPrefix(segment, "{") ||
		strings.HasPrefix(segment, ":") ||
		strings.HasPrefix(segment, "${") ||
		strings.HasPrefix(segment, "<") ||
		segment == "*" ||
		segment == "**"
	if variable {
		return "{}"
	}
	// A segment may hold a hole without being one: `file-${id}.json`,
	// `{id}.pdf`. The whole segment is then unknowable, so it is one too.
	if strings.Contains(segment, "${") ||
		(strings.Contains(segment, "{") && strings.Contains(segment, "}")) {
		return "{}"
	}
	return segment
}

// Join joins a base and a path the way a client's base URL and a call's path
// are joined: one separator between them, and the base's own trailing slash
// forgiven.
func Join(base, p string) string {
	base = strings.TrimRight(base, "/")
	p = strings.TrimLeft(p, "/")
	if base == "" {
		return "/" + p
	}
	return base + "/" + p
}

// Route is the property's value: the verb, a space, the normalized path. `*`
// where no verb is written, which matches every verb.
//
// The format is the whole contract between the plugins that write it and the
// verb that reads it, so it is deliberately the simplest thing that can be
// split back apart.
func Route(verb Verb, p string) string {
	if verb == "" {
		verb = Any
	}
	return string(verb) + " " + Normalize(p)
}

// Ref is one route a symbol states, as the source wrote it.
//
// The same record serves both sides, because a parser reads both the same
// way: a handler that serves a path, and a function whose call asks for one.
// What differs is the property it is written to, not what was read.
type Ref struct {
	// Symbol is what the route belongs to: the declaring handler, or the
	// function making the call.
	Symbol string
	// Verb is what the source states — in the callee's name, an annotation,
	// or a method argument. Empty when it states none, which a project-local
	// wrapper never does.
	Verb Verb
	// Written is the path exactly as the source writes it, before
	// normalising.
	Written string
	// Line is where it is written, 1-based.
	Line uint64
}

// Route is the comparable form — [Route] over this record's own two halves.
func (r Ref) Route() string { return Route(r.Verb, r.Written) }

// Side is which half of a route a set of records states.
//
// A handler serves a path and a call asks for one. The records are read the
// same way and differ only in which pair of properties they are written to,
// which is all this names.
type Side int

// The two halves.
const (
	// Serves is a declaration, written on the handler.
	Serves Side = iota
	// Requests is a request, written on the function making the call.
	Requests
)

// comparable names the property holding the comparable form, and what it
// says.
func (s Side) comparable() (string, string) {
	if s == Serves {
		return "serves", "the routes this handler serves, as `VERB path` with every variable " +
			"segment reduced to `{}` — `*` where the declaration names no verb. " +
			"`wire` matches these against what asks for them"
	}
	return "requests", "the routes this function asks for, as `VERB path` with every variable " +
		"segment reduced to `{}` — `*` where the call names no verb. `wire` " +
		"matches these against what serves them"
}

// asWritten names the property holding the source's own spelling, and what it
// says.
func (s Side) asWritten() (string, string) {
	name := "requests_as_written"
	if s == Serves {
		name = "serves_as_written"
	}
	return name, "the same paths exactly as the source writes them"
}

// stated is one symbol's routes: the comparable forms, and the spellings they
// came from. Sets, because one function may ask for the same path twice.
type stated struct {
	comparable map[string]bool
	written    map[string]bool
}

// Routes are the routes each symbol states, folded ready to write.
//
// Every plugin's assemble phase does the same two things with the records its
// parse produced — fold them by symbol, then write two properties on that
// symbol's node — so it is done here once.
type Routes struct {
	side     Side
	bySymbol map[string]*stated
}

// Of folds a parse's records, one side of the convention.
func Of(side Side, refs []Ref) Routes {
	by := map[string]*stated{}
	for _, r := range refs {
		s := by[r.Symbol]
		if s == nil {
			s = &stated{comparable: map[string]bool{}, written: map[string]bool{}}
			by[r.Symbol] = s
		}
		s.comparable[r.Route()] = true
		s.written[r.Written] = true
	}
	return Routes{side: side, bySymbol: by}
}

// Write writes this symbol's two properties into props, and returns how many
// routes that was.
//
// Zero, and nothing written, when the symbol states none: a function that
// asks for nothing carries no property rather than an empty one.
func (rs Routes) Write(symbol string, props map[string]any) int {
	s := rs.bySymbol[symbol]
	if s == nil {
		return 0
	}
	name, desc := rs.side.comparable()
	props[name] = map[string]any{"$desc": desc, "$value": sorted(s.comparable)}
	name, desc = rs.side.asWritten()
	props[name] = map[string]any{"$desc": desc, "$value": sorted(s.written)}
	return len(s.comparable)
}

// Note is the account of what was written, empty when there is none to give.
//
// Counted from what [Routes.Write] actually attached rather than from what was
// folded, so a record naming a symbol no node holds is not claimed as
// recorded.
func (rs Routes) Note(recorded int) string {
	if recorded == 0 {
		return ""
	}
	property, what := "requests", "asked for by a call whose argument is a path"
	if rs.side == Serves {
		property, what = "serves", "declared by a handler"
	}
	return fmt.Sprintf(
		"%d route(s) %s — recorded as `%s`; the other half is matched on demand, never stored",
		recorded, what, property)
}

// sorted is a set as the sorted list a property carries: the same order the
// Rust SDK's BTreeSet gives, so two plugins write one value.
func sorted(set map[string]bool) []string {
	out := make([]string, 0, len(set))
	for k := range set {
		out = append(out, k)
	}
	sort.Strings(out)
	return out
}
