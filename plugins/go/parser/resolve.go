package parser

import (
	"fmt"
	"path"
	"slices"
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

// predeclaredTypes are the type names the language predeclares. Written as
// `T(x)` they are conversions, not calls — and named as a receiver's type
// they belong to no package, which is why the resolver checks this table in
// two places rather than folding it into builtins.
var predeclaredTypes = map[string]bool{
	"bool": true, "string": true, "error": true, "any": true,
	"int": true, "int8": true, "int16": true, "int32": true, "int64": true,
	"uint": true, "uint8": true, "uint16": true, "uint32": true, "uint64": true,
	"uintptr": true, "byte": true, "rune": true,
	"float32": true, "float64": true, "complex64": true, "complex128": true,
}

// stamped is a CALLS edge carrying the family's resolution stamps: which
// strategy bound it, how certain that strategy is, and what the source wrote.
func stamped(src, dst string, line int, strategy, band, written string) Edge {
	return Edge{Src: src, Dst: dst, Type: "CALLS", Line: line, Props: Props{
		"_resolved_by": strategy,
		"_confidence":  band,
		"_ref":         written,
	}}
}

// spawns marks a CALLS edge that a `go` statement wrote. It stays a CALLS
// edge — control does reach the callee, and dropping that would cost every
// reader the reachability — but a caller and a goroutine are not the same
// fact, and a graph that cannot tell them apart describes a different
// program than the one on disk.
func spawns(e Edge) Edge {
	e.Props["concurrent"] = map[string]any{
		"$desc":  "how control departs from waiting for this callee: `go` starts it on another goroutine",
		"$value": "go",
	}
	return e
}

// aliasTable is one file's import table: local name → import path.
func aliasTable(f *FileFacts, pkgNames map[string]string) map[string]string {
	out := map[string]string{}
	for _, imp := range f.Imports {
		switch imp.Alias {
		case "_", ".":
			continue
		case "":
			name := path.Base(imp.Path)
			if in, ok := pkgNames[imp.Path]; ok {
				name = in
			}
			out[name] = imp.Path
		default:
			out[imp.Alias] = imp.Path
		}
	}
	return out
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

	// Interface satisfaction: structural, decided before calls now — an
	// interface-typed receiver resolves through its implementers.
	implements, unmatchedIfaces := satisfy(all, pkgNames)
	for _, e := range implements {
		addEdge(e)
	}
	implementers := map[string][]string{}
	for _, e := range implements {
		implementers[e.Dst] = append(implementers[e.Dst], e.Src)
	}
	ifaceKeys := map[string]bool{}
	for _, f := range all {
		for _, i := range f.Ifaces {
			ifaceKeys[i.Key] = true
		}
	}

	// ---- receiver-typing indexes (P1–P3 parity with the rust/py parsers) --
	// A binding is how a name got its type: stated outright, through a free
	// callable's declared return, or through a method call on another local —
	// resolved lazily so chains type link by link.
	type binding struct {
		typeKey  string
		callKey  string
		callRecv string
		callName string
		elemOf   string
	}
	// One type position a declaration writes down, before it is known whether
	// the type it names is one this tree declares.
	type typeUse struct{ owner, target, role, name string }
	var typeUses []typeUse
	returns := map[string]string{} // callable key → first-result type key
	locals := map[string]binding{} // caller\x00name
	elems := map[string]binding{}  // caller\x00name → that container's element type
	pkgVars := map[string]binding{}
	embeds := map[string][]string{}
	for fi := range all {
		f := &all[fi]
		if f.Failed {
			continue
		}
		aliases := aliasTable(f, pkgNames)
		resolveType := func(alias, name string) string {
			if alias == "" {
				return f.PkgPath + "." + name
			}
			if target, ok := aliases[alias]; ok {
				return target + "." + name
			}
			return ""
		}
		for _, r := range f.TypeRefs {
			if tk := resolveType(r.Alias, r.Type); tk != "" {
				typeUses = append(typeUses, typeUse{owner: r.Owner, target: tk, role: r.Role, name: r.Name})
			}
		}
		for _, r := range f.Returns {
			key := f.PkgPath + "." + r.Name
			if r.Recv != "" {
				key = f.PkgPath + "." + r.Recv + "." + r.Name
			}
			if tk := resolveType(r.TypeAlias, r.TypeName); tk != "" {
				if _, have := returns[key]; !have {
					returns[key] = tk
				}
			}
		}
		for _, h := range f.Hints {
			var b binding
			switch {
			case h.ElemOf != "":
				b.elemOf = h.ElemOf
			case h.TypeName != "":
				// A predeclared type belongs to no package: qualifying it
				// would invent `<pkg>.error`. Unless this package actually
				// declares a type by that name — legal, and then it wins.
				if h.TypeAlias == "" && predeclaredTypes[h.TypeName] {
					if d, ok := pkgs[f.PkgPath]; !ok || !d.types[h.TypeName] {
						continue
					}
				}
				b.typeKey = resolveType(h.TypeAlias, h.TypeName)
				if b.typeKey == "" {
					continue
				}
			case h.CallAlias == "":
				b.callKey = f.PkgPath + "." + h.CallName
			default:
				if target, ok := aliases[h.CallAlias]; ok {
					b.callKey = target + "." + h.CallName
				} else {
					// Not an import: a method call on another local —
					// typed when that local is.
					b.callRecv, b.callName = h.CallAlias, h.CallName
				}
			}
			k := h.Caller + "\x00" + h.Name
			if h.Caller == "" {
				k = f.PkgPath + "\x00" + h.Name
				if _, have := pkgVars[k]; !have {
					pkgVars[k] = b
				}
				continue
			}
			if _, have := locals[k]; !have {
				locals[k] = b
			}
		}
		for _, h := range f.ElemHints {
			if h.TypeName == "" {
				continue
			}
			// Same guard as Hints: a predeclared element type belongs to
			// no package, so `[]string` must not become `<pkg>.string`.
			if h.TypeAlias == "" && predeclaredTypes[h.TypeName] {
				if d, ok := pkgs[f.PkgPath]; !ok || !d.types[h.TypeName] {
					continue
				}
			}
			tk := resolveType(h.TypeAlias, h.TypeName)
			if tk == "" {
				continue
			}
			k := h.Caller + "\x00" + h.Name
			if _, have := elems[k]; !have {
				elems[k] = binding{typeKey: tk}
			}
		}
		for _, e := range f.Embeds {
			outer := f.PkgPath + "." + e.TypeName
			if tk := resolveType(e.FieldAlias, e.FieldName); tk != "" {
				embeds[outer] = append(embeds[outer], tk)
			}
		}
	}

	// The unresolved ledger: what could not be resolved becomes a queryable
	// UnresolvedRef node per (caller file, written form), the reason on the
	// edge — the graph shows its blind spots instead of only counting them.
	unresolvedNodes := map[string]Node{}
	ledger := func(f *FileFacts, caller, written string, line int, reason string) {
		key := "?::" + f.File + "::" + written
		if _, ok := unresolvedNodes[key]; !ok {
			unresolvedNodes[key] = Node{Key: key, Label: "UnresolvedRef", Props: Props{
				"name": written, "file": f.File,
			}}
		}
		e := stamped(caller, key, line, "unresolved", "none", written)
		e.Props["_reason"] = reason
		addEdge(e)
	}

	// The declared method a type answers `name` with: its own first, then
	// promotion through embedded types, then — for an interface — the one
	// satisfying type that has it, else the interface's own method node.
	var methodFor func(typeKey, name string, depth int) (string, string, string, bool)
	methodFor = func(typeKey, name string, depth int) (string, string, string, bool) {
		if depth == 0 {
			return "", "", "", false
		}
		if ifaceKeys[typeKey] {
			var hits []string
			for _, t := range implementers[typeKey] {
				if _, ok := seen[t+"."+name]; ok {
					hits = append(hits, t)
				}
			}
			if len(hits) == 1 {
				return hits[0] + "." + name, "interface", "high", true
			}
			if _, ok := seen[typeKey+"."+name]; ok {
				return typeKey + "." + name, "interface", "medium", true
			}
			return "", "", "", false
		}
		if _, ok := seen[typeKey+"."+name]; ok {
			return typeKey + "." + name, "receiver", "high", true
		}
		for _, emb := range embeds[typeKey] {
			if t, _, _, ok := methodFor(emb, name, depth-1); ok {
				return t, "embedded", "high", true
			}
		}
		return "", "", "", false
	}

	// Calls, against each file's own import table.
	unresolved, externalCalls := 0, 0
	chans, goroutines := 0, 0
	chanSeen := map[string]bool{}
	// Where a bare name was handed to an in-tree callee: (caller, callee,
	// argument names by position). A channel argument becomes the callee's
	// channel, which is the only way the graph can join a producer to a
	// consumer that never call each other.
	type handover struct {
		caller, callee string
		args           []string
	}
	var handovers []handover
	for fi := range all {
		f := &all[fi]
		if f.Failed {
			continue
		}
		aliases := aliasTable(f, pkgNames)
		var typeOf func(caller, name string, depth int) (string, bool)
		typeOf = func(caller, name string, depth int) (string, bool) {
			if depth == 0 {
				return "", false
			}
			b, ok := locals[caller+"\x00"+name]
			if !ok {
				b, ok = pkgVars[f.PkgPath+"\x00"+name]
			}
			if !ok {
				return "", false
			}
			switch {
			case b.typeKey != "":
				return b.typeKey, true
			case b.elemOf != "":
				eb, ok := elems[caller+"\x00"+b.elemOf]
				if !ok || eb.typeKey == "" {
					return "", false
				}
				return eb.typeKey, true
			case b.callKey != "":
				tk, ok := returns[b.callKey]
				return tk, ok
			case b.callRecv != "":
				rt, ok := typeOf(caller, b.callRecv, depth-1)
				if !ok {
					return "", false
				}
				mk, _, _, ok := methodFor(rt, b.callName, 4)
				if !ok {
					return "", false
				}
				tk, ok := returns[mk]
				return tk, ok
			}
			return "", false
		}
		unresolved += f.Opaque
		for _, c := range f.Calls {
			mark := func(e Edge) Edge { return e }
			if c.Concurrent {
				mark = spawns
				goroutines++
			}
			if c.Alias == "" {
				if builtins[c.Name] {
					continue
				}
				d := forPkg(f.PkgPath)
				if key, ok := d.funcs[c.Name]; ok {
					addEdge(mark(stamped(c.Caller, key, c.Line, "package", "high", c.Name)))
					if len(c.Args) > 0 {
						handovers = append(handovers, handover{c.Caller, key, c.Args})
					}
				} else if !d.types[c.Name] && !predeclaredTypes[c.Name] { // a conversion is not a call
					unresolved++
					ledger(f, c.Caller, c.Name, c.Line, "name not declared in this package")
				}
				continue
			}
			target, ok := aliases[c.Alias]
			if !ok {
				// Not a package qualifier: a method on a value. When the
				// body or the signature states the receiver's type, the
				// call resolves; otherwise it is shown, never guessed.
				written := c.Alias + "." + c.Name
				if tk, ok := typeOf(c.Caller, c.Alias, 6); ok {
					if mk, strategy, band, ok := methodFor(tk, c.Name, 4); ok {
						addEdge(mark(stamped(c.Caller, mk, c.Line, strategy, band, written)))
						if len(c.Args) > 0 {
							handovers = append(handovers, handover{c.Caller, mk, c.Args})
						}
						continue
					}
					if _, ours := seen[tk]; !ours {
						// Typed, but not by this tree: the same position a
						// package-level external call is in, so the same
						// treatment — a stand-in and an edge, not a ledger
						// entry claiming the receiver was untypeable.
						key := tk + "." + c.Name
						note(key, "Function")
						addEdge(mark(stamped(c.Caller, key, c.Line, "external-method", "high", written)))
						externalCalls++
						continue
					}
					// A type this tree does declare, with no such method:
					// a real gap, and the reason has to say which type.
					unresolved++
					ledger(f, c.Caller, written, c.Line, "method not found on "+tk)
					continue
				}
				unresolved++
				ledger(f, c.Caller, written, c.Line, "method call: receiver type unknown")
				continue
			}
			if d, ok := pkgs[target]; ok {
				if key, ok := d.funcs[c.Name]; ok {
					addEdge(mark(stamped(c.Caller, key, c.Line, "import", "high", c.Alias+"."+c.Name)))
					if len(c.Args) > 0 {
						handovers = append(handovers, handover{c.Caller, key, c.Args})
					}
				} else if !d.types[c.Name] {
					unresolved++
					ledger(f, c.Caller, c.Alias+"."+c.Name, c.Line, "not declared in "+target)
				}
				continue
			}
			key := target + "." + c.Name
			note(key, "Function")
			addEdge(mark(stamped(c.Caller, key, c.Line, "external-path", "high", c.Alias+"."+c.Name)))
			externalCalls++
		}
	}

	// Channels. A channel is where Go puts the join between two pieces of
	// code that never call each other, so without it the graph simply does
	// not hold the relation: `ch <- v` names no function, and the goroutine
	// draining the other end is reached through the variable alone.
	//
	// The channel becomes a node of its own, keyed under whatever made it —
	// `pkg.Producer.ch` for a local, `pkg.ch` for a package-level var. That
	// is safe as a key because Go's own package scope forbids a func and a
	// type sharing a name, so `pkg.Producer.ch` cannot also be a method.
	// owner \x00 name → the channel node(s) that name stands for. A list,
	// not one key: a parameter is bound once per call site that passes a
	// channel to it, and fan-in — two producers, one worker — is ordinary
	// Go. Keeping only the first would report the worker as draining one
	// producer and not the other.
	chanKey := map[string][]string{}
	for fi := range all {
		f := &all[fi]
		if f.Failed {
			continue
		}
		for _, c := range f.Chans {
			owner := c.Owner
			if owner == "" {
				owner = f.PkgPath
			}
			key := owner + "." + c.Name
			if chanSeen[key] {
				continue
			}
			chanSeen[key] = true
			props := Props{"name": c.Name}
			if c.Elem != "" {
				props["element"] = map[string]any{
					"$desc":  "what the channel carries",
					"$value": c.Elem,
				}
			}
			if c.Buf != "" {
				props["buffer"] = map[string]any{
					"$desc":  "declared capacity; unbuffered when absent, so every send blocks for a receiver",
					"$value": c.Buf,
				}
			}
			// A package-level channel is already a declared `Var` node. Two
			// nodes for one variable would be a lie about the program, so
			// the declaration is adopted rather than duplicated: it keeps
			// its key, its file and its line, and gains what being a channel
			// adds. A local has no declaration of its own and gets one.
			if at, taken := seen[key]; taken {
				if out.Nodes[at].Label != "Var" {
					continue // some other declaration owns the name
				}
				out.Nodes[at].Label = "Channel"
				for k, v := range props {
					if _, has := out.Nodes[at].Props[k]; !has {
						out.Nodes[at].Props[k] = v
					}
				}
			} else {
				out.Nodes = append(out.Nodes, Node{Key: key, Label: "Channel", Props: props})
				seen[key] = len(out.Nodes) - 1
				addEdge(Edge{Src: owner, Dst: key, Type: "CONTAINS", Line: c.Line})
			}
			chanKey[c.Owner+"\x00"+c.Name] = []string{key}
			chans++
		}
	}
	// A channel handed to a function becomes that function's channel.
	//
	// This is the hop that makes the graph hold the relation at all. The
	// producer usually keeps the `make` and the consumer only ever sees a
	// parameter, so without following the argument across, `Producer` and
	// `worker` sit in the same graph with nothing between them — which is
	// the opposite of what the code says. Position joins them: argument `i`
	// of the call lands on parameter `i` of the callee, and the callee's own
	// name for it is what its body writes.
	//
	// Repeated to a fixpoint, because a channel is often passed on again;
	// bounded because a cycle of mutually-recursive passers must not spin.
	chanParams := map[string]ChanParam{} // fn \x00 index → param
	for fi := range all {
		for _, cp := range all[fi].ChanParams {
			chanParams[cp.Fn+"\x00"+fmt.Sprint(cp.Index)] = cp
		}
	}
	for round := 0; round < 8; round++ {
		grew := false
		for _, h := range handovers {
			for i, arg := range h.args {
				if arg == "" {
					continue
				}
				cp, ok := chanParams[h.callee+"\x00"+fmt.Sprint(i)]
				if !ok {
					continue
				}
				nodes, ok := chanKey[h.caller+"\x00"+arg]
				if !ok {
					if nodes, ok = chanKey["\x00"+arg]; !ok {
						continue
					}
				}
				slot := h.callee + "\x00" + cp.Name
				for _, node := range nodes {
					if slices.Contains(chanKey[slot], node) {
						continue
					}
					chanKey[slot] = append(chanKey[slot], node)
					grew = true
				}
			}
		}
		if !grew {
			break
		}
	}

	// Sends and receives, once the channel names are known. A name that is
	// no channel here is dropped in silence: `range` over a slice reaches
	// this loop too, and most names in a body are ordinary values.
	for fi := range all {
		f := &all[fi]
		if f.Failed {
			continue
		}
		for _, o := range f.ChanOps {
			keys, ok := chanKey[o.Caller+"\x00"+o.Name]
			if !ok {
				// Not made in this body, and no channel arrived under the
				// name: a package-level channel, in scope for every function
				// in the package that declares it.
				if keys, ok = chanKey["\x00"+o.Name]; !ok {
					continue
				}
				if !strings.HasPrefix(o.Caller, f.PkgPath+".") {
					continue
				}
			}
			for _, key := range keys {
				addEdge(Edge{Src: o.Caller, Dst: key, Type: o.Op, Line: o.Line, Props: Props{
					"_resolved_by": "chan-name",
					"_confidence":  "high",
					"_ref":         o.Name,
				}})
			}
		}
	}

	// Functions passed as values (C4, narrow): a bare same-package function
	// name in argument position becomes a REFERENCES edge — silent on a
	// miss (most idents are values), never a self-loop.
	refSeen := map[string]bool{}
	for fi := range all {
		f := &all[fi]
		if f.Failed {
			continue
		}
		for _, r := range f.FnRefs {
			if builtins[r.Name] {
				continue
			}
			d := forPkg(f.PkgPath)
			key, ok := d.funcs[r.Name]
			if !ok || key == r.Caller {
				continue
			}
			dedup := r.Caller + "\x00" + key
			if refSeen[dedup] {
				continue
			}
			refSeen[dedup] = true
			addEdge(Edge{Src: r.Caller, Dst: key, Type: "REFERENCES", Line: r.Line, Props: Props{
				"_resolved_by": "fn-ref",
				"_confidence":  "high",
				"_ref":         r.Name,
			}})
		}
	}

	// Ledger nodes join `seen` before the implied pass — they are edge
	// targets, and the implied pass would otherwise mint bare doubles.
	// A declared type is a dependency as surely as a call is. Until this
	// existed, asking what a type's change would break saw only who built one
	// — never who takes it as a parameter, returns it, or holds it in a field.
	//
	// Only types this tree declares get an edge, and the check has to happen
	// here, before the implied-node pass below: an edge to a name nothing
	// declares would mint a bare `Type` node for every `string` in the tree.
	declaredType := map[string]bool{}
	for _, n := range out.Nodes {
		switch n.Label {
		case "Struct", "Interface", "Type", "TypeAlias":
			declaredType[n.Key] = true
		}
	}
	type useKey struct{ src, dst string }
	useRoles := map[useKey]map[string]bool{}
	useNames := map[useKey]string{}
	var useOrder []useKey
	foreignTypeRefs := 0
	for _, u := range typeUses {
		if !declaredType[u.target] {
			foreignTypeRefs++
			continue
		}
		// A type that mentions itself is a real shape and a useless edge.
		if u.target == u.owner {
			continue
		}
		if _, ok := seen[u.owner]; !ok {
			continue
		}
		k := useKey{u.owner, u.target}
		if useRoles[k] == nil {
			useRoles[k] = map[string]bool{}
			useNames[k] = u.name
			useOrder = append(useOrder, k)
		}
		useRoles[k][u.role] = true
	}
	for _, k := range useOrder {
		roles := make([]string, 0, len(useRoles[k]))
		for r := range useRoles[k] {
			roles = append(roles, r)
		}
		sort.Strings(roles)
		props := Props{"role": map[string]any{
			"$desc":  "the type position this declaration names the type in",
			"$value": strings.Join(roles, ", "),
		}}
		if useNames[k] != "" {
			props["name"] = useNames[k]
		}
		addEdge(Edge{Src: k.src, Dst: k.dst, Type: "USES_TYPE", Props: props})
	}

	ledgerKeys := make([]string, 0, len(unresolvedNodes))
	for k := range unresolvedNodes {
		ledgerKeys = append(ledgerKeys, k)
	}
	sort.Strings(ledgerKeys)
	for _, k := range ledgerKeys {
		seen[k] = len(out.Nodes)
		out.Nodes = append(out.Nodes, unresolvedNodes[k])
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
	if chans > 0 {
		out.Notes = append(out.Notes, fmt.Sprintf(
			"%d channel(s) recorded as nodes, with the sends and receives written on their names — including in a function the channel was handed to, joined by argument position; a channel reached through a struct field is not one of them", chans))
	}
	if goroutines > 0 {
		out.Notes = append(out.Notes, fmt.Sprintf(
			"%d call(s) started with `go`, marked `concurrent` on the CALLS edge", goroutines))
	}
	if len(useOrder) > 0 || foreignTypeRefs > 0 {
		out.Notes = append(out.Notes, fmt.Sprintf(
			"%d type reference(s) recorded as `USES_TYPE` — what a declaration's fields, parameters and results are typed by; %d more name types this tree does not declare and are left as written text on the node",
			len(useOrder), foreignTypeRefs))
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
