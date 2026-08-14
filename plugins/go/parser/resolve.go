package parser

import (
	"fmt"
	"path"
	"sort"
	"strings"
)

// Assembled is the result: facts, and an account of what could not be done.
type Assembled struct {
	Nodes   []Node
	Edges   []Edge
	Skipped int
	Notes   []string
}

// builtins are callable without being declared anywhere; a call to one is
// not an edge worth recording.
var builtins = map[string]bool{
	"append": true, "cap": true, "clear": true, "close": true, "complex": true,
	"copy": true, "delete": true, "imag": true, "len": true, "make": true,
	"max": true, "min": true, "new": true, "panic": true, "print": true,
	"println": true, "real": true, "recover": true,
}

// Assemble resolves across every file's facts, in chunk order.
//
// This is the half that must see everything at once: an unqualified call
// binds to a function another file of the package declares, a qualified one
// binds through the file's import table, and interface satisfaction — which
// is structural in Go — can only be decided once every method set is known.
// The result must not depend on where the chunk boundaries fell, and does
// not: everything here keys on file-order-stable indexes, never on how the
// facts were batched.
func Assemble(all []FileFacts) Assembled {
	out := Assembled{}

	// The indexes resolution reads: what each package declares, by kind.
	type declared struct {
		funcs map[string]string // name → key
		types map[string]bool   // name declares a type (conversion, not call)
	}
	pkgs := map[string]*declared{}
	pkgNames := map[string]string{}
	forPkg := func(p string) *declared {
		d, ok := pkgs[p]
		if !ok {
			d = &declared{funcs: map[string]string{}, types: map[string]bool{}}
			pkgs[p] = d
		}
		return d
	}
	for _, f := range all {
		if f.Failed {
			out.Skipped++
			continue
		}
		pkgNames[f.PkgPath] = f.PkgName
		d := forPkg(f.PkgPath)
		for _, n := range f.Nodes {
			simple := strings.TrimPrefix(n.Key, f.PkgPath+".")
			switch n.Label {
			case "Function":
				d.funcs[simple] = n.Key
			case "Struct", "Interface", "Type", "TypeAlias":
				d.types[simple] = true
			}
		}
	}

	// Nodes, first seen wins. Package nodes repeat by design — one per file —
	// and merge; any other repeated key is a build-tag variant, counted.
	seen := map[string]int{}
	dupes := 0
	for _, f := range all {
		for _, n := range f.Nodes {
			at, ok := seen[n.Key]
			if !ok {
				seen[n.Key] = len(out.Nodes)
				out.Nodes = append(out.Nodes, n)
				continue
			}
			if n.Label == "Package" {
				// Keep the first file's node, adopt the doc a later file
				// (usually doc.go) carries if the first had none.
				if _, has := out.Nodes[at].Props["doc_comment"]; !has {
					if doc, ok := n.Props["doc_comment"]; ok {
						out.Nodes[at].Props["doc_comment"] = doc
					}
				}
				continue
			}
			dupes++
		}
	}

	// The package node carries what its files import, joined — the same
	// `imports` property the Rust parser writes on a Module. A package is
	// every one of its files, so the union is the fact.
	importsByPkg := map[string]map[string]bool{}
	for _, f := range all {
		for _, imp := range f.Imports {
			if importsByPkg[f.PkgPath] == nil {
				importsByPkg[f.PkgPath] = map[string]bool{}
			}
			importsByPkg[f.PkgPath][imp.Path] = true
		}
	}
	for pkg, set := range importsByPkg {
		at, ok := seen[pkg]
		if !ok {
			continue
		}
		paths := make([]string, 0, len(set))
		for p := range set {
			paths = append(paths, p)
		}
		sort.Strings(paths)
		if out.Nodes[at].Props == nil {
			out.Nodes[at].Props = Props{}
		}
		out.Nodes[at].Props["imports"] = strings.Join(paths, ", ")
	}

	edges := map[string]bool{}
	addEdge := func(e Edge) {
		k := e.Src + "\x00" + e.Type + "\x00" + e.Dst
		if !edges[k] {
			edges[k] = true
			out.Edges = append(out.Edges, e)
		}
	}
	for _, f := range all {
		for _, e := range f.Edges {
			addEdge(e)
		}
	}

	// External stand-ins, strengthened as more is learned: an import alone
	// says only "a package"; a call through it names a function too.
	external := map[string]string{}
	note := func(key, label string) {
		if have, ok := external[key]; !ok || (have == "Package" && label != "Package") {
			if _, ours := seen[key]; !ours {
				external[key] = label
			}
		}
	}

	// Imports become edges; in-tree ones bind package to package, foreign
	// ones get a stand-in carrying the import path and nothing else.
	for _, f := range all {
		for _, imp := range f.Imports {
			if _, ok := pkgNames[imp.Path]; ok {
				addEdge(Edge{Src: f.PkgPath, Dst: imp.Path, Type: "IMPORTS", Line: imp.Line})
			} else {
				note(imp.Path, "Package")
				addEdge(Edge{Src: f.PkgPath, Dst: imp.Path, Type: "IMPORTS", Line: imp.Line})
			}
		}
	}

	// Calls, against each file's own import table.
	unresolved, externalCalls := 0, 0
	for _, f := range all {
		aliases := map[string]string{}
		for _, imp := range f.Imports {
			switch imp.Alias {
			case "_", ".":
				continue
			case "":
				name := path.Base(imp.Path)
				if in, ok := pkgNames[imp.Path]; ok {
					name = in // the tree knows the real package name
				}
				aliases[name] = imp.Path
			default:
				aliases[imp.Alias] = imp.Path
			}
		}
		unresolved += f.Opaque
		for _, c := range f.Calls {
			if c.Alias == "" {
				if builtins[c.Name] {
					continue
				}
				d := forPkg(f.PkgPath)
				if key, ok := d.funcs[c.Name]; ok {
					addEdge(Edge{Src: c.Caller, Dst: key, Type: "CALLS", Line: c.Line})
				} else if !d.types[c.Name] { // a conversion is not a call
					unresolved++
				}
				continue
			}
			target, ok := aliases[c.Alias]
			if !ok {
				// Not a package qualifier, so a method on a value — whose
				// type is what a parser cannot know.
				unresolved++
				continue
			}
			if d, ok := pkgs[target]; ok {
				if key, ok := d.funcs[c.Name]; ok {
					addEdge(Edge{Src: c.Caller, Dst: key, Type: "CALLS", Line: c.Line})
				} else if !d.types[c.Name] {
					unresolved++
				}
				continue
			}
			key := target + "." + c.Name
			note(key, "Function")
			addEdge(Edge{Src: c.Caller, Dst: key, Type: "CALLS", Line: c.Line})
			externalCalls++
		}
	}

	// Interface satisfaction: structural, so decidable here and only here.
	implements, unmatchedIfaces := satisfy(all, pkgNames)
	for _, e := range implements {
		addEdge(e)
	}

	// A method whose receiver type sits in a file this run never saw — a
	// build-tag variant, or a subtree ingest that split a package — still
	// proves a type with that name exists: say so with a bare node rather
	// than emit an edge into nothing.
	implied := map[string]bool{}
	for _, e := range out.Edges {
		for _, key := range [2]string{e.Src, e.Dst} {
			if _, ok := seen[key]; !ok {
				if _, ext := external[key]; !ext {
					implied[key] = true
				}
			}
		}
	}
	impliedKeys := make([]string, 0, len(implied))
	for k := range implied {
		impliedKeys = append(impliedKeys, k)
	}
	sort.Strings(impliedKeys)
	for _, k := range impliedKeys {
		seen[k] = len(out.Nodes)
		out.Nodes = append(out.Nodes, Node{Key: k, Label: "Type"})
	}

	// The stand-ins, in sorted order so the output never depends on map
	// iteration.
	extKeys := make([]string, 0, len(external))
	for k := range external {
		extKeys = append(extKeys, k)
	}
	sort.Strings(extKeys)
	for _, k := range extKeys {
		out.Nodes = append(out.Nodes, Node{
			Key:         k,
			Label:       external[k],
			ExtraLabels: []string{"External"},
		})
	}

	if unresolved > 0 {
		out.Notes = append(out.Notes, fmt.Sprintf(
			"%d call(s) left unresolved: a receiver's type, or a name local to a body, is what a parser cannot know", unresolved))
	}
	if externalCalls > 0 {
		out.Notes = append(out.Notes, fmt.Sprintf(
			"%d call(s) into other modules and the standard library, recorded as external nodes carrying the import path and nothing else", externalCalls))
	}
	if dupes > 0 {
		out.Notes = append(out.Notes, fmt.Sprintf(
			"%d declaration(s) defined in more than one file of a package — build-tag variants; the first seen is kept", dupes))
	}
	if unmatchedIfaces > 0 {
		out.Notes = append(out.Notes, fmt.Sprintf(
			"%d interface(s) left unmatched: an embedded interface is not declared in this tree", unmatchedIfaces))
	}
	return out
}

// satisfy decides which declared types implement which declared interfaces.
//
// Go's rule is structural — a type satisfies an interface iff its method set
// covers the interface's — and this check applies it textually, under the
// certainty rules the parser lives by: within one package, signatures are
// compared as written; across packages only when both sides are spelled
// entirely in predeclared types (see [portable]); and an interface embedding
// anything this tree does not declare is left unmatched and counted, rather
// than half-checked. Receiver pointer-ness is deliberately ignored: the edge
// claims the pointer method set, which is what a caller holding either can
// reach through.
func satisfy(all []FileFacts, pkgNames map[string]string) ([]Edge, int) {
	type methodSet map[string]Method
	types := map[string]methodSet{} // type key → its methods
	for _, f := range all {
		for _, m := range f.Methods {
			key := f.PkgPath + "." + m.Recv
			if types[key] == nil {
				types[key] = methodSet{}
			}
			if _, ok := types[key][m.Name]; !ok {
				types[key][m.Name] = m
			}
		}
	}

	ifaces := map[string]Iface{} // key → declaration, first seen
	order := []string{}
	for _, f := range all {
		for _, i := range f.Ifaces {
			if _, ok := ifaces[i.Key]; !ok {
				ifaces[i.Key] = i
				order = append(order, i.Key)
			}
		}
	}

	// Flatten embedded interfaces, within what the tree declares. `error` is
	// predeclared and known; anything else must resolve here or the whole
	// interface is left unmatched — a half-checked satisfaction would be a
	// guess wearing an edge's clothes.
	aliasesByPkg := map[string]map[string]string{}
	for _, f := range all {
		if aliasesByPkg[f.PkgPath] == nil {
			aliasesByPkg[f.PkgPath] = map[string]string{}
		}
		for _, imp := range f.Imports {
			alias := imp.Alias
			if alias == "_" || alias == "." {
				continue
			}
			if alias == "" {
				alias = path.Base(imp.Path)
				if in, ok := pkgNames[imp.Path]; ok {
					alias = in
				}
			}
			aliasesByPkg[f.PkgPath][alias] = imp.Path
		}
	}

	var flatten func(key string, trail map[string]bool) (map[string]IfaceMethod, bool)
	flatten = func(key string, trail map[string]bool) (map[string]IfaceMethod, bool) {
		if trail[key] {
			return nil, false // an embedding cycle is not a thing to chase
		}
		trail[key] = true
		defer delete(trail, key)
		i, ok := ifaces[key]
		if !ok {
			return nil, false
		}
		out := map[string]IfaceMethod{}
		for _, m := range i.Methods {
			out[m.Name] = m
		}
		for _, emb := range i.Embeds {
			if emb == "error" {
				out["Error"] = IfaceMethod{Name: "Error", Sig: "() string", Portable: true}
				continue
			}
			var embKey string
			if alias, name, qualified := strings.Cut(emb, "."); qualified {
				target, ok := aliasesByPkg[i.Pkg][alias]
				if !ok {
					return nil, false
				}
				embKey = target + "." + name
			} else {
				embKey = i.Pkg + "." + emb
			}
			inner, ok := flatten(embKey, trail)
			if !ok {
				return nil, false
			}
			for name, m := range inner {
				if _, have := out[name]; !have {
					out[name] = m
				}
			}
		}
		return out, true
	}

	typeKeys := make([]string, 0, len(types))
	for k := range types {
		typeKeys = append(typeKeys, k)
	}
	sort.Strings(typeKeys)

	var edges []Edge
	unmatched := 0
	for _, ifaceKey := range order {
		want, ok := flatten(ifaceKey, map[string]bool{})
		if !ok {
			unmatched++
			continue
		}
		if len(want) == 0 {
			continue // the empty interface claims nothing worth an edge
		}
		ifacePkg := ifaces[ifaceKey].Pkg
		for _, typeKey := range typeKeys {
			samePkg := strings.HasPrefix(typeKey, ifacePkg+".") &&
				!strings.Contains(strings.TrimPrefix(typeKey, ifacePkg+"."), ".")
			have := types[typeKey]
			satisfies := true
			for name, m := range want {
				got, ok := have[name]
				if !ok || got.Sig != m.Sig || (!samePkg && !(got.Portable && m.Portable)) {
					satisfies = false
					break
				}
			}
			if satisfies {
				edges = append(edges, Edge{Src: typeKey, Dst: ifaceKey, Type: "IMPLEMENTS"})
			}
		}
	}
	return edges, unmatched
}
