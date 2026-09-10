// Package parser turns Go source into facts — nodes and edges a parser is
// certain of, leaving nothing for a model to guess at.
//
// Same discipline as the Rust parser it sits beside: parse each file alone
// (so chunks run concurrently in instances that share nothing), resolve
// across files once in [Assemble], and count whatever could not be resolved
// in the notes rather than dropping it silently. A thin graph should be
// explained by its report.
//
// Keys are Go's own qualified names: the package's import path (read from
// the nearest `go.mod`, the way the Rust parser reads `Cargo.toml`), then
// `path.Ident` for declarations and `path.Type.Method` for methods —
// exactly how Go code refers to these things, which is what makes keys
// stable across subtrees and ingests.
package parser

import (
	"fmt"
	"go/ast"
	"go/parser"
	"go/printer"
	"go/token"
	"path"
	"regexp"
	"strings"
	"unicode/utf8"
)

// Files is what the parser reads through — the plugin contract's host, as
// one small interface so tests can hand in a plain map.
type Files interface {
	// List returns readable paths ending with suffix ("" for all), sorted.
	List(suffix string) ([]string, error)
	Read(path string) ([]byte, error)
	// Label is what to call the tree when its contents do not say.
	Label() (string, bool)
}

// Props are one node's properties; they serialize to the JSON object the
// contract carries. encoding/json sorts map keys, so the encoding is
// deterministic.
type Props map[string]any

// Node is a fact about a thing.
type Node struct {
	Key         string   `json:"key"`
	Label       string   `json:"label"`
	ExtraLabels []string `json:"extra_labels,omitempty"`
	Props       Props    `json:"props,omitempty"`
}

// Edge is a fact about a relation, between node keys. Line is where the
// relation is *written* — the call site, the import statement, the declared
// member — and 0 where nothing is written anywhere (a structural
// satisfaction has no line).
type Edge struct {
	Src  string `json:"src"`
	Dst  string `json:"dst"`
	Type string `json:"type"`
	Line int    `json:"line,omitempty"`
	// Resolution stamps and reasons (P1 family conventions) — empty for
	// edges that carry none.
	Props Props `json:"props,omitempty"`
}

// Call is a call site held until every file is known. Alias is the package
// qualifier as written ("" for an unqualified call); resolution happens in
// [Assemble], against the file's own import table.
type Call struct {
	Caller string `json:"caller"`
	Alias  string `json:"alias,omitempty"`
	Name   string `json:"name"`
	Line   int    `json:"line,omitempty"`
	// Concurrent marks `go f()`. Control does reach the callee, so this is
	// still a CALLS edge — but it reaches it on another goroutine, and a
	// reader who cannot tell the two apart is reading the wrong program.
	Concurrent bool `json:"concurrent,omitempty"`
	// Args are the arguments as bare names, by position ("" for anything
	// that is not a plain identifier). Handing a channel to a function is
	// how one goroutine's channel becomes another's, and the position is
	// what joins the argument to the parameter it lands on.
	Args []string `json:"args,omitempty"`
}

// ChanParam is one channel-typed parameter of a declared function: which
// function, which position, and the name the body knows it by.
type ChanParam struct {
	Fn    string `json:"fn"`
	Index int    `json:"index"`
	Name  string `json:"name"`
}

// Chan is one channel a body makes: `ch := make(chan T, n)`. Owner is the
// function key it was made in, or "" for a package-level var, whose scope is
// the whole package.
//
// A channel is the one value in Go whose whole purpose is to join two pieces
// of code that never call each other. Nothing else in the graph can carry
// that: `ch <- v` names no function, and the goroutine on the other end is
// reached through the variable alone.
type Chan struct {
	Owner string `json:"owner,omitempty"`
	Name  string `json:"name"`
	Elem  string `json:"elem,omitempty"`
	Buf   string `json:"buf,omitempty"`
	Line  int    `json:"line,omitempty"`
}

// ChanOp is one send or receive written on a name, held until [Assemble]
// knows which names are channels. `ch <- v` and `<-ch` say so outright;
// `range x` does not, so it is recorded as a candidate and kept only when
// the name turns out to be a channel.
type ChanOp struct {
	Caller string `json:"caller"`
	Name   string `json:"name"`
	// "SENDS" or "RECEIVES".
	Op   string `json:"op"`
	Line int    `json:"line,omitempty"`
}

// Hint records how a body (or the package scope) binds a name to a type the
// source declares — the deterministic inputs receiver typing reads. Caller is
// "" for package-level vars, whose scope is the whole package. Exactly one of
// TypeName (a declared/annotated/composite-literal type) or CallName (an
// initializer call whose declared return names the type) is set; the Alias
// fields carry the package qualifier as written.
type Hint struct {
	Caller    string `json:"caller,omitempty"`
	Name      string `json:"name"`
	TypeAlias string `json:"type_alias,omitempty"`
	TypeName  string `json:"type_name,omitempty"`
	CallAlias string `json:"call_alias,omitempty"`
	CallName  string `json:"call_name,omitempty"`
	// ElemOf names another binding whose element type this one takes:
	// `for _, r := range rules` makes r ElemOf "rules". Set on its own —
	// the type is not known here, only where to look it up.
	ElemOf string `json:"elem_of,omitempty"`
}

// Ret records a callable's first result type when it is a plain named type
// (possibly pointered/qualified) — what types `x := f()`.
type Ret struct {
	Recv      string `json:"recv,omitempty"`
	Name      string `json:"name"`
	TypeAlias string `json:"type_alias,omitempty"`
	TypeName  string `json:"type_name"`
}

// TypeRef is one type position a declaration writes down — a struct field, a
// parameter, a result — resolved in [Assemble] into a `USES_TYPE` edge when
// it names a type this tree declares.
//
// Alias is the package qualifier as written, so it resolves through the
// file's own import table like every other reference.
type TypeRef struct {
	Owner string `json:"owner"`
	Role  string `json:"role"`
	Name  string `json:"name,omitempty"`
	Alias string `json:"alias,omitempty"`
	Type  string `json:"type"`
	Line  int    `json:"line,omitempty"`
}

// Embed records a struct's embedded field, for method-set promotion: a call
// on the outer type may resolve to a method the embedded type declares.
type Embed struct {
	TypeName   string `json:"type_name"`
	FieldAlias string `json:"field_alias,omitempty"`
	FieldName  string `json:"field_name"`
}

// FnRef is a bare name in call-argument position — the one shape where a
// function is verifiably handed around as a value.
type FnRef struct {
	Caller string `json:"caller"`
	Name   string `json:"name"`
	Line   int    `json:"line,omitempty"`
}

// Import is one import declaration. Alias is as written ("" when the default
// name is used) — a list, not a map, because two blank imports may coexist.
type Import struct {
	Alias string `json:"alias,omitempty"`
	Path  string `json:"path"`
	Line  int    `json:"line,omitempty"`
}

// Method records one method's receiver-independent signature, for the
// interface-satisfaction check in [Assemble].
type Method struct {
	Recv     string `json:"recv"`
	Name     string `json:"name"`
	Sig      string `json:"sig"`
	Portable bool   `json:"portable,omitempty"`
}

// IfaceMethod is one method an interface demands.
type IfaceMethod struct {
	Name     string `json:"name"`
	Sig      string `json:"sig"`
	Portable bool   `json:"portable,omitempty"`
}

// Iface is one interface declaration, held whole until every file is known:
// satisfaction is structural in Go, so it can only be decided once every
// type's method set has been seen.
type Iface struct {
	Key     string        `json:"key"`
	Pkg     string        `json:"pkg"`
	Methods []IfaceMethod `json:"methods,omitempty"`
	Embeds  []string      `json:"embeds,omitempty"`
}

// FileFacts is everything one file contributes — the opaque partial the
// component serializes between the two phases.
type FileFacts struct {
	File    string   `json:"file"`
	PkgPath string   `json:"pkg_path"`
	PkgName string   `json:"pkg_name"`
	Failed  bool     `json:"failed,omitempty"`
	Nodes   []Node   `json:"nodes,omitempty"`
	Edges   []Edge   `json:"edges,omitempty"`
	Calls   []Call   `json:"calls,omitempty"`
	Imports []Import `json:"imports,omitempty"`
	Ifaces  []Iface  `json:"ifaces,omitempty"`
	Methods []Method `json:"methods,omitempty"`
	Hints   []Hint   `json:"hints,omitempty"`
	// ElemHints have the same shape as Hints, but TypeName holds the
	// *element* type of a container — the lookup an ElemOf binding needs.
	// A separate list because a name has both: `rules` is a []Rule here
	// and has no methods of its own there.
	ElemHints []Hint  `json:"elem_hints,omitempty"`
	Returns   []Ret   `json:"returns,omitempty"`
	Embeds    []Embed `json:"embeds,omitempty"`
	// FnRefs are functions passed as values: `register(handler)` —
	// (caller, bare name, line), argument position only.
	FnRefs []FnRef `json:"fn_refs,omitempty"`
	// Chans are the channels this file's bodies make; ChanOps the sends and
	// receives written on a name, resolved against them in [Assemble].
	Chans   []Chan   `json:"chans,omitempty"`
	ChanOps []ChanOp `json:"chan_ops,omitempty"`
	// ChanParams are the channel-typed parameters each function declares —
	// where a caller's channel arrives under a new name.
	ChanParams []ChanParam `json:"chan_params,omitempty"`
	// TypeRefs are the type positions this file's declarations write down.
	TypeRefs []TypeRef `json:"type_refs,omitempty"`
	// Call sites too dynamic to name at all — `f()()`, chained selectors —
	// counted so the notes can account for them.
	Opaque int `json:"opaque,omitempty"`
}

// ParseChunk parses one chunk of paths into per-file facts.
//
// Pure per file: nothing here looks across files, which is what makes chunks
// safe to parse concurrently. Resolution — the cross-file half — happens in
// [Assemble], once, over every chunk's facts together.
func ParseChunk(files Files, paths []string, includeSource bool) []FileFacts {
	modules := newModuleTable(files)
	out := make([]FileFacts, 0, len(paths))
	for _, p := range paths {
		src, err := files.Read(p)
		if err != nil {
			out = append(out, FileFacts{File: p, Failed: true})
			continue
		}
		out = append(out, parseFile(p, modules.pathFor(path.Dir(p)), src, includeSource))
	}
	return out
}

// ParseDocument parses one pushed document. With no tree around it there is
// no `go.mod` to read, so the package's own name serves as its path.
func ParseDocument(name string, src []byte, includeSource bool) []FileFacts {
	f, fset, err := parseSource(name, src)
	if err != nil {
		return []FileFacts{{File: name, Failed: true}}
	}
	return []FileFacts{walkFile(name, f.Name.Name, f, fset, src, includeSource)}
}

// moduleTable resolves a directory to its package's import path by finding
// the nearest `go.mod` above it — the declared `module` line is what decides,
// the way the Rust parser prefers the manifest's `[package] name` over the
// directory. A tree with no `go.mod` falls back to the host's label.
type moduleTable struct {
	files Files
	// dir → import path, memoized; "" keys the root.
	cache map[string]string
}

var moduleLine = regexp.MustCompile(`(?m)^module\s+(\S+)`)

func newModuleTable(files Files) *moduleTable {
	return &moduleTable{files: files, cache: map[string]string{}}
}

func (m *moduleTable) pathFor(dir string) string {
	if dir == "." {
		dir = ""
	}
	if got, ok := m.cache[dir]; ok {
		return got
	}
	resolved := m.resolve(dir)
	m.cache[dir] = resolved
	return resolved
}

func (m *moduleTable) resolve(dir string) string {
	at := dir
	for {
		manifest := "go.mod"
		if at != "" {
			manifest = at + "/go.mod"
		}
		if src, err := m.files.Read(manifest); err == nil {
			if match := moduleLine.FindSubmatch(src); match != nil {
				module := string(match[1])
				if rel := strings.TrimPrefix(dir, at); rel != "" {
					return module + "/" + strings.TrimPrefix(rel, "/")
				}
				return module
			}
		}
		if at == "" {
			break
		}
		parent := path.Dir(at)
		if parent == "." {
			parent = ""
		}
		at = parent
	}
	base := "module"
	if label, ok := m.files.Label(); ok && label != "" {
		base = label
	}
	if dir != "" {
		return base + "/" + dir
	}
	return base
}

func parseSource(name string, src []byte) (*ast.File, *token.FileSet, error) {
	fset := token.NewFileSet()
	f, err := parser.ParseFile(fset, name, src, parser.ParseComments|parser.SkipObjectResolution)
	return f, fset, err
}

func parseFile(file, pkgPath string, src []byte, includeSource bool) FileFacts {
	f, fset, err := parseSource(file, src)
	if err != nil {
		return FileFacts{File: file, Failed: true}
	}
	return walkFile(file, pkgPath, f, fset, src, includeSource)
}

// walkFile is the per-file walk: every declaration becomes a node under the
// package, every call site is held for resolution, and every interface and
// method signature is kept for the satisfaction check.
func walkFile(file, pkgPath string, f *ast.File, fset *token.FileSet, src []byte, includeSource bool) FileFacts {
	facts := FileFacts{File: file, PkgPath: pkgPath, PkgName: f.Name.Name}

	pkgProps := Props{"name": f.Name.Name}
	if doc := strings.TrimSpace(f.Doc.Text()); doc != "" {
		pkgProps["doc_comment"] = doc
	}
	facts.Nodes = append(facts.Nodes, Node{Key: pkgPath, Label: "Package", Props: pkgProps})

	for _, spec := range f.Imports {
		imp := Import{
			Path: strings.Trim(spec.Path.Value, `"`),
			Line: fset.Position(spec.Pos()).Line,
		}
		if spec.Name != nil {
			imp.Alias = spec.Name.Name
		}
		facts.Imports = append(facts.Imports, imp)
	}

	w := &walker{
		facts:         &facts,
		fset:          fset,
		src:           src,
		pkg:           pkgPath,
		includeSource: includeSource,
	}
	for _, decl := range f.Decls {
		switch d := decl.(type) {
		case *ast.FuncDecl:
			w.funcDecl(d)
		case *ast.GenDecl:
			w.genDecl(d)
		}
	}

	// Every declaration in this walk came from this file, so the file lands
	// on each node in one place. The package is the exception: it *spans*
	// files, and a single file+line on it would be an arbitrary pick.
	//
	// Test-ness rides along for the same reason and with the same exception:
	// `_test.go` is the toolchain's own rule, not a convention, so every
	// declaration in this file is test code — but a package holding one test
	// file is not a test package, and flagging it would say it was.
	testFile := isTestFile(file)
	for i := range facts.Nodes {
		if facts.Nodes[i].Label == "Package" {
			continue
		}
		facts.Nodes[i].Props["file"] = file
		if testFile {
			setTestFlag(facts.Nodes[i].Props, "build-rule", testFileDesc, "definitive")
		}
	}
	return facts
}

// isTestFile reports Go's own build rule: a file whose name ends `_test.go`
// is compiled only by `go test`, never into the package's production build.
// That is stronger than a naming convention and stronger than "looks like a
// test" — nothing this file declares can be reached by a production binary.
func isTestFile(path string) bool {
	base := path
	if i := strings.LastIndex(base, "/"); i >= 0 {
		base = base[i+1:]
	}
	return strings.HasSuffix(base, "_test.go")
}

const testFileDesc = "test code: `_test.go`, which the go tool compiles only under `go test` and never into the production build"

// setTestFlag marks a node as test code: what the evidence was, and — in the
// companion `_` property the renderers keep out of sight and the embedder out
// of its vectors — how much that evidence is worth. Two properties rather
// than one because the second is for a reader weighing the first, not for
// display.
func setTestFlag(props Props, kind, desc, confidence string) {
	props["test_flag"] = map[string]any{
		"$desc":  desc,
		"$value": kind,
	}
	props["_test_flag_confidence"] = confidence
}

type walker struct {
	facts         *FileFacts
	fset          *token.FileSet
	src           []byte
	pkg           string
	includeSource bool
}

func (w *walker) funcDecl(d *ast.FuncDecl) {
	name := d.Name.Name
	if name == "_" {
		return
	}
	// Every `init` in a package shares one name, so as a node it could only
	// be a key collision; it declares nothing callable either. Its calls are
	// wiring, not API, and go uncounted with it.
	if name == "init" && d.Recv == nil {
		return
	}

	label, key, parent := "Function", w.pkg+"."+name, w.pkg
	sig := "func " + name + strings.TrimPrefix(w.print(d.Type), "func")
	if d.Recv != nil && len(d.Recv.List) > 0 {
		recv := receiverBase(d.Recv.List[0].Type)
		if recv == "" {
			return
		}
		label = "Method"
		parent = w.pkg + "." + recv
		key = parent + "." + name
		sig = "func (" + w.print(d.Recv.List[0].Type) + ") " + name +
			strings.TrimPrefix(w.print(d.Type), "func")
		w.facts.Methods = append(w.facts.Methods, Method{
			Recv:     recv,
			Name:     name,
			Sig:      strings.TrimPrefix(w.print(d.Type), "func"),
			Portable: portable(d.Type),
		})
	}

	props := w.props(sig, d.Doc, d.Name.Name)
	if w.includeSource {
		w.addSource(props, d)
	}
	props["end_line"] = w.endLine(d)
	w.node(parent, key, label, props, w.line(d))
	w.calls(key, d.Body)
	w.chans(key, d.Body)

	// Receiver-typing inputs: the declared first result (what `x := f()`
	// makes x), the typed parameters, and the body's own stated bindings.
	if d.Type.Results != nil {
		for _, r := range d.Type.Results.List {
			w.typeRefs(key, "return", "", r.Type, w.line(d))
		}
	}
	if d.Type.Params != nil {
		for _, field := range d.Type.Params.List {
			pname := ""
			if len(field.Names) > 0 {
				pname = field.Names[0].Name
			}
			w.typeRefs(key, "param", pname, field.Type, w.line(d))
		}
	}
	if d.Type.Results != nil && len(d.Type.Results.List) > 0 {
		if alias, tname, ok := typeRef(d.Type.Results.List[0].Type); ok {
			recv := ""
			if label == "Method" {
				recv = receiverBase(d.Recv.List[0].Type)
			}
			w.facts.Returns = append(w.facts.Returns, Ret{
				Recv: recv, Name: name, TypeAlias: alias, TypeName: tname,
			})
		}
	}
	if d.Type.Params != nil {
		pos := 0
		for _, field := range d.Type.Params.List {
			// A channel-typed parameter is where a caller's channel arrives
			// under a new name — `chan T`, `<-chan T` and `chan<- T` alike.
			if _, isChan := field.Type.(*ast.ChanType); isChan {
				for _, id := range field.Names {
					if id.Name != "_" {
						w.facts.ChanParams = append(w.facts.ChanParams, ChanParam{
							Fn: key, Index: pos, Name: id.Name,
						})
					}
					pos++
				}
				continue
			}
			alias, tname, ok := typeRef(field.Type)
			ealias, ename, eok := elemRef(field.Type)
			for _, id := range field.Names {
				if id.Name != "_" {
					if ok {
						w.hint(Hint{Caller: key, Name: id.Name, TypeAlias: alias, TypeName: tname})
					}
					if eok {
						w.elemHint(Hint{Caller: key, Name: id.Name, TypeAlias: ealias, TypeName: ename})
					}
				}
				pos++
			}
			if len(field.Names) == 0 {
				pos++ // an unnamed parameter still occupies its position
			}
		}
	}
	// The receiver's type is stated in the signature — the one binding that
	// needs no inference at all, and until now the one that was never taken:
	// every `recv.Method()` in the tree fell to the unresolved ledger.
	if d.Recv != nil && len(d.Recv.List) > 0 && len(d.Recv.List[0].Names) > 0 {
		if rn := d.Recv.List[0].Names[0].Name; rn != "" && rn != "_" {
			// A receiver is always declared in the method's own package,
			// so the alias is empty by construction.
			if base := receiverBase(d.Recv.List[0].Type); base != "" {
				w.hint(Hint{Caller: key, Name: rn, TypeName: base})
			}
		}
	}
	w.hints(key, d.Body)
}

func (w *walker) genDecl(d *ast.GenDecl) {
	// In a const block, a spec with no expressions repeats the previous
	// one — that is the language's own rule for iota ladders, so carrying
	// the expression (and its type) forward is recording, not guessing.
	var carryVals []ast.Expr
	var carryType ast.Expr
	for _, spec := range d.Specs {
		switch s := spec.(type) {
		case *ast.TypeSpec:
			w.typeSpec(d, s)
		case *ast.ValueSpec:
			values, typ := s.Values, s.Type
			if d.Tok == token.CONST {
				if len(values) == 0 && carryVals != nil {
					values, typ = carryVals, carryType
				} else {
					carryVals, carryType = values, typ
				}
			}
			w.valueSpec(d, s, values, typ)
		}
	}
}

func (w *walker) typeSpec(d *ast.GenDecl, s *ast.TypeSpec) {
	name := s.Name.Name
	if name == "_" {
		return
	}
	key := w.pkg + "." + name
	doc := s.Doc
	if doc == nil {
		doc = d.Doc
	}

	switch t := s.Type.(type) {
	case *ast.StructType:
		for _, field := range t.Fields.List {
			if len(field.Names) != 0 {
				continue // named fields are described, not embedded
			}
			if alias, fname, ok := typeRef(field.Type); ok {
				w.facts.Embeds = append(w.facts.Embeds, Embed{
					TypeName: name, FieldAlias: alias, FieldName: fname,
				})
			}
		}
		props := w.props("", doc, name)
		if t.Fields != nil {
			for _, field := range t.Fields.List {
				fname := embeddedName(field.Type)
				if len(field.Names) > 0 {
					fname = field.Names[0].Name
				}
				w.typeRefs(key, "field", fname, field.Type, w.line(field))
			}
		}
		if fields := w.fieldList(t.Fields); len(fields) > 0 {
			props["fields"] = map[string]any{
				"$desc":  "the fields it declares, each with its type as written",
				"$value": fields,
			}
		}
		if w.includeSource {
			w.addSource(props, s)
		}
		props["end_line"] = w.endLine(s)
		w.node(w.pkg, key, "Struct", props, w.line(s))
	case *ast.InterfaceType:
		iface := Iface{Key: key, Pkg: w.pkg}
		props := w.props("", doc, name)
		if w.includeSource {
			w.addSource(props, s)
		}
		props["end_line"] = w.endLine(s)
		w.node(w.pkg, key, "Interface", props, w.line(s))
		for _, m := range t.Methods.List {
			if len(m.Names) == 0 {
				iface.Embeds = append(iface.Embeds, w.print(m.Type))
				continue
			}
			fn, ok := m.Type.(*ast.FuncType)
			if !ok {
				continue
			}
			for _, id := range m.Names {
				sig := strings.TrimPrefix(w.print(fn), "func")
				iface.Methods = append(iface.Methods, IfaceMethod{
					Name:     id.Name,
					Sig:      sig,
					Portable: portable(fn),
				})
				// Each demanded method is its own node, the way the Rust
				// parser treats a trait's items. No visibility: an
				// interface's methods are as public as the interface.
				mkey := key + "." + id.Name
				mprops := Props{
					"signature": "func " + id.Name + sig,
					"line":      w.line(m),
					"end_line":  w.endLine(m),
				}
				if text := strings.TrimSpace(m.Doc.Text()); text != "" {
					mprops["doc_comment"] = text
				}
				w.facts.Nodes = append(w.facts.Nodes, Node{Key: mkey, Label: "Method", Props: mprops})
				w.facts.Edges = append(w.facts.Edges, Edge{Src: key, Dst: mkey, Type: "HAS_METHOD", Line: w.line(m)})
			}
		}
		w.facts.Ifaces = append(w.facts.Ifaces, iface)
	default:
		label := "Type"
		if s.Assign.IsValid() {
			label = "TypeAlias"
		}
		props := w.props(w.print(t), doc, name)
		if w.includeSource {
			w.addSource(props, s)
		}
		props["end_line"] = w.endLine(s)
		w.node(w.pkg, key, label, props, w.line(s))
	}
}

// valueSpec records consts and vars the way the Rust parser records `const`
// and `static`: the type as written under `signature`, and the initializer
// as written under `value` — never evaluated, because `256 * 1024` folded
// wrongly is worse than the expression that produced it. "As written" is
// meant literally: the value is the file's own bytes, cut at
// [maxValueBytes]. See [walker.setValue] for why it is not printed.
func (w *walker) valueSpec(d *ast.GenDecl, s *ast.ValueSpec, values []ast.Expr, typ ast.Expr) {
	// A package-level var's stated type or initializer types it for every
	// function in the package (Caller "" = package scope). Consts are basic
	// values and carry no method sets worth hinting.
	if d.Tok == token.VAR {
		for i, id := range s.Names {
			if id.Name == "_" {
				continue
			}
			// A package-level `var ch = make(chan T)` is in scope for every
			// function in the package, and is the shape a worker pool most
			// often takes. Owner "" says exactly that.
			if i < len(values) {
				if elem, buf, ok := w.chanMake(values[i]); ok {
					w.facts.Chans = append(w.facts.Chans, Chan{
						Name: id.Name, Elem: elem, Buf: buf, Line: w.line(s),
					})
				}
			}
			if typ != nil {
				if alias, tname, ok := typeRef(typ); ok {
					w.hint(Hint{Name: id.Name, TypeAlias: alias, TypeName: tname})
				}
			} else if i < len(values) {
				if h, ok := initHint("", id.Name, values[i]); ok {
					w.hint(h)
				}
			}
		}
	}
	label := "Var"
	if d.Tok == token.CONST {
		label = "Const"
	}
	doc := s.Doc
	if doc == nil {
		doc = d.Doc
	}
	signature := ""
	if typ != nil {
		signature = w.print(typ)
	}
	for i, id := range s.Names {
		if id.Name == "_" {
			continue
		}
		props := w.props(signature, doc, id.Name)
		switch {
		case len(values) == len(s.Names):
			w.setValue(props, values[i])
		case len(values) > 0:
			// One expression, several names — `var a, b = f()`. The
			// initializer as written is the fact for each of them.
			w.setValue(props, values[0])
		}
		props["end_line"] = w.endLine(id)
		w.node(w.pkg, w.pkg+"."+id.Name, label, props, w.line(id))
	}
}

// calls walks a body collecting every call site. A plain identifier or a
// single `qualifier.Name` selector is held for resolution; anything deeper —
// `a.b.C()`, `f()()` — cannot be named without types and is counted opaque.
// Function literals inside the body attribute their calls to the declaration
// that contains them, which is where a reader would look.
func (w *walker) calls(caller string, body *ast.BlockStmt) {
	if body == nil {
		return
	}
	// Which calls a `go` starts. The visit gives no parents, so the go
	// statements are collected first and the call sites checked against
	// them — `go f()` is one CallExpr, the same node the walk below reaches
	// on its own.
	spawned := map[*ast.CallExpr]bool{}
	ast.Inspect(body, func(n ast.Node) bool {
		if g, ok := n.(*ast.GoStmt); ok && g.Call != nil {
			spawned[g.Call] = true
		}
		return true
	})
	ast.Inspect(body, func(n ast.Node) bool {
		call, ok := n.(*ast.CallExpr)
		if !ok {
			return true
		}
		// The arguments as bare names, by position — the only shape in
		// which a channel is verifiably handed to another function.
		args := make([]string, len(call.Args))
		bare := false
		for i, arg := range call.Args {
			if id, ok := arg.(*ast.Ident); ok {
				args[i] = id.Name
				bare = true
				w.facts.FnRefs = append(w.facts.FnRefs, FnRef{Caller: caller, Name: id.Name, Line: w.line(call)})
			}
		}
		if !bare {
			args = nil
		}
		site := Call{Caller: caller, Line: w.line(call), Concurrent: spawned[call], Args: args}
		switch fn := call.Fun.(type) {
		case *ast.Ident:
			site.Name = fn.Name
			w.facts.Calls = append(w.facts.Calls, site)
		case *ast.SelectorExpr:
			if x, ok := fn.X.(*ast.Ident); ok {
				site.Alias, site.Name = x.Name, fn.Sel.Name
				w.facts.Calls = append(w.facts.Calls, site)
			} else {
				w.facts.Opaque++
			}
		default:
			w.facts.Opaque++
		}
		return true
	})
}

// chanMake reads `make(chan T, n)` off an initializer, returning the element
// type and the buffer size as written. Anything else — including a channel
// obtained from a call — is not a make and states no element type here.
func (w *walker) chanMake(e ast.Expr) (elem, buf string, ok bool) {
	call, ok := e.(*ast.CallExpr)
	if !ok {
		return "", "", false
	}
	if id, ok := call.Fun.(*ast.Ident); !ok || id.Name != "make" || len(call.Args) == 0 {
		return "", "", false
	}
	ct, ok := call.Args[0].(*ast.ChanType)
	if !ok {
		return "", "", false
	}
	elem = w.print(ct.Value)
	if len(call.Args) > 1 {
		buf = w.print(call.Args[1])
	}
	return elem, buf, true
}

// chans records the channels a body makes and every send or receive it
// writes on a name.
//
// Only a bare name is followed. `s.ch <- v` names a field, and which channel
// that is depends on which `s` — a question this parser cannot answer, and
// answering it wrong would join two goroutines that never meet.
func (w *walker) chans(caller string, body *ast.BlockStmt) {
	if body == nil {
		return
	}
	op := func(name string, kind string, n ast.Node) {
		w.facts.ChanOps = append(w.facts.ChanOps, ChanOp{
			Caller: caller, Name: name, Op: kind, Line: w.line(n),
		})
	}
	bare := func(e ast.Expr) (string, bool) {
		id, ok := e.(*ast.Ident)
		if !ok || id.Name == "_" {
			return "", false
		}
		return id.Name, true
	}
	ast.Inspect(body, func(n ast.Node) bool {
		switch s := n.(type) {
		case *ast.AssignStmt:
			// `ch := make(chan T)`, and the same in a `var` inside a body.
			for i, lhs := range s.Lhs {
				if i >= len(s.Rhs) {
					break
				}
				name, ok := bare(lhs)
				if !ok {
					continue
				}
				if elem, buf, ok := w.chanMake(s.Rhs[i]); ok {
					w.facts.Chans = append(w.facts.Chans, Chan{
						Owner: caller, Name: name, Elem: elem, Buf: buf, Line: w.line(s),
					})
				}
			}
		case *ast.ValueSpec:
			for i, id := range s.Names {
				if i >= len(s.Values) || id.Name == "_" {
					continue
				}
				if elem, buf, ok := w.chanMake(s.Values[i]); ok {
					w.facts.Chans = append(w.facts.Chans, Chan{
						Owner: caller, Name: id.Name, Elem: elem, Buf: buf, Line: w.line(s),
					})
				}
			}
		case *ast.SendStmt:
			// `ch <- v`. Only a channel can be sent on, so no confirmation
			// against the channel table is needed for the *kind* of fact —
			// only for which channel it names.
			if name, ok := bare(s.Chan); ok {
				op(name, "SENDS", s)
			}
		case *ast.UnaryExpr:
			// `<-ch`, wherever it appears: an expression, a statement, the
			// right of a `case v := <-ch` in a select.
			if s.Op == token.ARROW {
				if name, ok := bare(s.X); ok {
					op(name, "RECEIVES", s)
				}
			}
		case *ast.RangeStmt:
			// `for v := range ch` drains a channel — but `range` also walks
			// slices, maps and strings, and the syntax does not say which.
			// Recorded as a candidate; [Assemble] keeps it only if the name
			// is a channel.
			if name, ok := bare(s.X); ok {
				op(name, "RECEIVES", s)
			}
		}
		return true
	})
}

// typeRef reads a type expression down to a plain named type — through
// pointers and generic instantiations, never through slices, maps, channels
// or funcs, whose methods belong to other types entirely.
func typeRef(e ast.Expr) (alias, name string, ok bool) {
	switch t := e.(type) {
	case *ast.StarExpr:
		return typeRef(t.X)
	case *ast.IndexExpr:
		return typeRef(t.X)
	case *ast.IndexListExpr:
		return typeRef(t.X)
	case *ast.Ident:
		return "", t.Name, true
	case *ast.SelectorExpr:
		if x, ok := t.X.(*ast.Ident); ok {
			return x.Name, t.Sel.Name, true
		}
	}
	return "", "", false
}

// elemRef reads a container type's element type. It is deliberately separate
// from [typeRef], which stops at the container — a slice's methods are not
// its element's, and widening typeRef would resolve `rules.Foo()` onto
// `Rule.Foo`, an edge that is not merely missing but wrong.
func elemRef(e ast.Expr) (alias, name string, ok bool) {
	switch t := e.(type) {
	case *ast.ParenExpr:
		return elemRef(t.X)
	case *ast.StarExpr:
		return elemRef(t.X)
	case *ast.ArrayType:
		return typeRef(t.Elt)
	case *ast.Ellipsis:
		return typeRef(t.Elt)
	case *ast.MapType:
		return typeRef(t.Value)
	case *ast.ChanType:
		return typeRef(t.Value)
	}
	return "", "", false
}

// elemInitHint reads `xs := []T{…}` — the one initializer shape that names a
// container's element type outright. A call initializer would mean following
// the callee's declared return into its element, which is not attempted.
func elemInitHint(caller, name string, e ast.Expr) (Hint, bool) {
	if v, ok := e.(*ast.CompositeLit); ok && v.Type != nil {
		if alias, tname, ok := elemRef(v.Type); ok {
			return Hint{Caller: caller, Name: name, TypeAlias: alias, TypeName: tname}, true
		}
	}
	return Hint{}, false
}

// typeNames reads every named type a type expression mentions, outermost
// first: `map[string]*Job` yields `string` and `Job`, `chan Event` yields
// `Event`, `Cache[Key, Job]` yields all three.
//
// Where [typeRef] stops — a slice's, map's or channel's methods belong to
// other types entirely, so receiver typing must not walk in — a *dependency*
// does not: holding a `[]Job` depends on `Job` as surely as holding one does,
// and that is the fact `USES_TYPE` records.
func typeNames(e ast.Expr) []TypeRef {
	var out []TypeRef
	var walk func(ast.Expr)
	walk = func(e ast.Expr) {
		switch t := e.(type) {
		case *ast.StarExpr:
			walk(t.X)
		case *ast.ParenExpr:
			walk(t.X)
		case *ast.Ellipsis:
			walk(t.Elt)
		case *ast.ArrayType:
			walk(t.Elt)
		case *ast.MapType:
			walk(t.Key)
			walk(t.Value)
		case *ast.ChanType:
			walk(t.Value)
		case *ast.IndexExpr:
			walk(t.X)
			walk(t.Index)
		case *ast.IndexListExpr:
			walk(t.X)
			for _, i := range t.Indices {
				walk(i)
			}
		case *ast.FuncType:
			for _, l := range []*ast.FieldList{t.Params, t.Results} {
				if l == nil {
					continue
				}
				for _, f := range l.List {
					walk(f.Type)
				}
			}
		case *ast.StructType:
			if t.Fields != nil {
				for _, f := range t.Fields.List {
					walk(f.Type)
				}
			}
		case *ast.Ident:
			out = append(out, TypeRef{Type: t.Name})
		case *ast.SelectorExpr:
			if x, ok := t.X.(*ast.Ident); ok {
				out = append(out, TypeRef{Alias: x.Name, Type: t.Sel.Name})
			}
		}
	}
	walk(e)
	return out
}

// typeRefs records every named type `e` mentions as one position of `owner`.
func (w *walker) typeRefs(owner, role, name string, e ast.Expr, line int) {
	for _, r := range typeNames(e) {
		r.Owner, r.Role, r.Name, r.Line = owner, role, name, line
		w.facts.TypeRefs = append(w.facts.TypeRefs, r)
	}
}

// initHint reads an initializer expression: a composite literal names its
// type outright, a call names a function whose declared return will. Anything
// else is a checker's job and yields nothing.
func initHint(caller, name string, e ast.Expr) (Hint, bool) {
	switch v := e.(type) {
	case *ast.UnaryExpr:
		if v.Op == token.AND {
			return initHint(caller, name, v.X)
		}
	case *ast.CompositeLit:
		if v.Type != nil {
			if alias, tname, ok := typeRef(v.Type); ok {
				return Hint{Caller: caller, Name: name, TypeAlias: alias, TypeName: tname}, true
			}
		}
	case *ast.CallExpr:
		switch fn := v.Fun.(type) {
		case *ast.Ident:
			return Hint{Caller: caller, Name: name, CallName: fn.Name}, true
		case *ast.SelectorExpr:
			if x, ok := fn.X.(*ast.Ident); ok {
				return Hint{Caller: caller, Name: name, CallAlias: x.Name, CallName: fn.Sel.Name}, true
			}
		}
	}
	return Hint{}, false
}

// hint records a binding, first writing wins — the calls it might type were
// themselves kept at their first site.
func (w *walker) hint(h Hint) {
	for _, have := range w.facts.Hints {
		if have.Caller == h.Caller && have.Name == h.Name {
			return
		}
	}
	w.facts.Hints = append(w.facts.Hints, h)
}

// elemHint records a container's element type, first writing wins — the same
// discipline as [walker.hint], on the separate list ElemOf bindings read.
func (w *walker) elemHint(h Hint) {
	for _, have := range w.facts.ElemHints {
		if have.Caller == h.Caller && have.Name == h.Name {
			return
		}
	}
	w.facts.ElemHints = append(w.facts.ElemHints, h)
}

// hints walks a body for the locals whose type the source states: typed
// `var` declarations, `:=` with a composite-literal or call initializer —
// the first value of a multi-assign takes the call's first result.
func (w *walker) hints(caller string, body *ast.BlockStmt) {
	if body == nil {
		return
	}
	ast.Inspect(body, func(n ast.Node) bool {
		switch st := n.(type) {
		case *ast.FuncLit:
			// A closure's parameters are explicitly typed in Go, and its
			// calls attribute to the enclosing function — so its typed
			// params are that function's hints too.
			if st.Type.Params != nil {
				for _, field := range st.Type.Params.List {
					alias, tname, ok := typeRef(field.Type)
					if !ok {
						continue
					}
					for _, id := range field.Names {
						if id.Name != "_" {
							w.hint(Hint{Caller: caller, Name: id.Name, TypeAlias: alias, TypeName: tname})
						}
					}
				}
			}
		case *ast.AssignStmt:
			if st.Tok != token.DEFINE || len(st.Lhs) == 0 {
				return true
			}
			first, ok := st.Lhs[0].(*ast.Ident)
			if !ok || first.Name == "_" {
				return true
			}
			if len(st.Rhs) >= 1 {
				if h, ok := initHint(caller, first.Name, st.Rhs[0]); ok {
					w.hint(h)
				}
				if h, ok := elemInitHint(caller, first.Name, st.Rhs[0]); ok {
					w.elemHint(h)
				}
			}
		case *ast.DeclStmt:
			gd, ok := st.Decl.(*ast.GenDecl)
			if !ok || gd.Tok != token.VAR {
				return true
			}
			for _, spec := range gd.Specs {
				vs, ok := spec.(*ast.ValueSpec)
				if !ok {
					continue
				}
				for i, id := range vs.Names {
					if id.Name == "_" {
						continue
					}
					if vs.Type != nil {
						if alias, tname, ok := typeRef(vs.Type); ok {
							w.hint(Hint{Caller: caller, Name: id.Name, TypeAlias: alias, TypeName: tname})
						}
						if alias, tname, ok := elemRef(vs.Type); ok {
							w.elemHint(Hint{Caller: caller, Name: id.Name, TypeAlias: alias, TypeName: tname})
						}
					} else if i < len(vs.Values) {
						if h, ok := initHint(caller, id.Name, vs.Values[i]); ok {
							w.hint(h)
						}
					}
				}
			}
		case *ast.RangeStmt:
			if st.Tok != token.DEFINE {
				return true
			}
			// The range expression has to be a bare name: that is the only
			// shape whose element type is already written down somewhere.
			src, ok := st.X.(*ast.Ident)
			if !ok {
				return true
			}
			// Key is skipped on purpose — a slice's is `int`, and a map's
			// would need a table of its own for a shape that does not occur.
			if v, vok := st.Value.(*ast.Ident); vok && v.Name != "_" {
				w.hint(Hint{Caller: caller, Name: v.Name, ElemOf: src.Name})
			}
		}
		return true
	})
}

func (w *walker) node(parent, key, label string, props Props, line int) {
	props["line"] = line
	w.facts.Nodes = append(w.facts.Nodes, Node{Key: key, Label: label, Props: props})
	w.facts.Edges = append(w.facts.Edges, Edge{Src: parent, Dst: key, Type: "CONTAINS", Line: line})
}

// line is 1-based, like every editor's gutter.
func (w *walker) line(n ast.Node) int {
	return w.fset.Position(n.Pos()).Line
}

// endLine is where a declaration stops. Together with line it lets a reader
// see how big a thing is without opening it, and lets `snippet` read exactly
// the declaration instead of guessing a fixed number of lines after its
// first.
func (w *walker) endLine(n ast.Node) int {
	return w.fset.Position(n.End()).Line
}

// props builds the common property set, dropping entries that came back
// empty — an absent property is cheaper and truer than one holding "".
func (w *walker) props(signature string, doc *ast.CommentGroup, name string) Props {
	out := Props{}
	if signature != "" {
		out["signature"] = signature
	}
	if text := strings.TrimSpace(doc.Text()); text != "" {
		out["doc_comment"] = text
	}
	if ast.IsExported(name) {
		out["visibility"] = "exported"
	}
	return out
}

// addSource attaches the declaration's own source under `_code` — retrieval
// only. The underscore keeps it out of the embedding text and the schema
// summary the model reads.
func (w *walker) addSource(props Props, n ast.Node) {
	from, to := w.fset.Position(n.Pos()).Offset, w.fset.Position(n.End()).Offset
	if from < 0 || to > len(w.src) || from >= to {
		return
	}
	props["_code"] = map[string]any{
		"$desc":  "source as written, for retrieval — not indexed or embedded",
		"$value": string(w.src[from:to]),
	}
}

// fieldList renders fields the way the Rust parser does — a list of
// `name: type` strings, in declaration order, which a map would have lost.
// Go has no visibility keyword to prepend: the capitalization *is* the
// visibility, and it is already in the name.
func (w *walker) fieldList(fields *ast.FieldList) []string {
	if fields == nil {
		return nil
	}
	var out []string
	for _, f := range fields.List {
		ty := w.print(f.Type)
		if len(f.Names) == 0 {
			// An embedded field: its name is its type's.
			out = append(out, embeddedName(f.Type)+": "+ty)
			continue
		}
		for _, id := range f.Names {
			if id.Name != "_" {
				out = append(out, id.Name+": "+ty)
			}
		}
	}
	return out
}

func (w *walker) print(n ast.Node) string {
	var b strings.Builder
	if err := printer.Fprint(&b, w.fset, n); err != nil {
		return ""
	}
	return b.String()
}

// Past this many bytes an initializer has stopped describing a declaration
// and started being the data. Generated code is where that happens:
// protoc-gen-go writes a descriptor blob as one string concatenation
// hundreds of terms long, and a `value` carrying a hundred kilobytes of
// escaped bytes costs every reader of the graph and tells none of them
// anything a prefix would not.
const maxValueBytes = 1024

// setValue records an initializer as written — by offset, not by printing.
//
// [walker.print] re-renders an expression from the AST, and go/printer does
// it recursively: one frame per level, so `a + b + c + …` costs a frame per
// term. That is affordable on a host and fatal in a plugin, whose stack is
// fixed when it is linked (64 KiB here), sits at the bottom of linear memory
// and has no guard page under it — it wraps past zero and surfaces as an
// out-of-bounds access with no mention of a stack. One generated file used
// to refuse the whole repository that way.
//
// The bytes are already in hand and need no walk at all. For gofmt'd input —
// which generated code always is — the slice is byte for byte what the
// printer would have produced, because the printer reproduces the original
// line breaks from these same positions.
func (w *walker) setValue(props Props, e ast.Expr) {
	from := w.fset.Position(exprStart(e)).Offset
	to := w.fset.Position(exprEnd(e)).Offset
	if from < 0 || to > len(w.src) || from >= to {
		return
	}
	text := string(w.src[from:to])
	if len(text) > maxValueBytes {
		cut := maxValueBytes
		for cut > 0 && !utf8.RuneStart(text[cut]) {
			cut--
		}
		text = fmt.Sprintf("%s… (%d more bytes, elided)", text[:cut], len(text)-cut)
	}
	props["value"] = text
}

// exprStart is `e.Pos()` with the recursion taken out.
//
// Several ast nodes answer Pos() by asking their leftmost child: BinaryExpr
// returns X.Pos(), and in `a + b + c` that X is another BinaryExpr. So the
// depth of the expression is the depth of the call stack — the same
// pathology as printing it, in a much cheaper frame, but on the same fixed
// stack. Walking the spine in a loop costs one frame at any depth.
func exprStart(e ast.Expr) token.Pos {
	for {
		switch x := e.(type) {
		case nil:
			return token.NoPos
		case *ast.BinaryExpr:
			e = x.X
		case *ast.CallExpr:
			e = x.Fun
		case *ast.SelectorExpr:
			e = x.X
		case *ast.IndexExpr:
			e = x.X
		case *ast.IndexListExpr:
			e = x.X
		case *ast.SliceExpr:
			e = x.X
		case *ast.TypeAssertExpr:
			e = x.X
		case *ast.KeyValueExpr:
			e = x.Key
		case *ast.CompositeLit:
			if x.Type == nil {
				return x.Lbrace
			}
			e = x.Type
		default:
			return e.Pos()
		}
	}
}

// exprEnd is `e.End()`, the same spine walked from the other side. A `+`
// chain leans left, so its End() is shallow where its Pos() is deep — but
// unary operators and key-value pairs lean the other way, and symmetry here
// costs nothing.
func exprEnd(e ast.Expr) token.Pos {
	for {
		switch x := e.(type) {
		case nil:
			return token.NoPos
		case *ast.BinaryExpr:
			e = x.Y
		case *ast.KeyValueExpr:
			e = x.Value
		case *ast.UnaryExpr:
			e = x.X
		case *ast.StarExpr:
			e = x.X
		default:
			return e.End()
		}
	}
}

// receiverBase unwraps a receiver type down to the identifier it names:
// `*List[T]` → `List`. An unnamed receiver has no place to hang a method.
func receiverBase(t ast.Expr) string {
	for {
		switch e := t.(type) {
		case *ast.StarExpr:
			t = e.X
		case *ast.IndexExpr:
			t = e.X
		case *ast.IndexListExpr:
			t = e.X
		case *ast.Ident:
			return e.Name
		case *ast.ParenExpr:
			t = e.X
		default:
			return ""
		}
	}
}

func embeddedName(t ast.Expr) string {
	switch e := t.(type) {
	case *ast.StarExpr:
		return embeddedName(e.X)
	case *ast.SelectorExpr:
		return e.Sel.Name
	case *ast.IndexExpr:
		return embeddedName(e.X)
	case *ast.IndexListExpr:
		return embeddedName(e.X)
	case *ast.Ident:
		return e.Name
	default:
		return "_"
	}
}

// predeclared is Go's built-in type universe. A signature spelled entirely
// in these means the same thing in every package, which is what makes it
// comparable across packages by its text.
var predeclared = map[string]bool{
	"bool": true, "string": true, "error": true, "any": true,
	"int": true, "int8": true, "int16": true, "int32": true, "int64": true,
	"uint": true, "uint8": true, "uint16": true, "uint32": true, "uint64": true,
	"uintptr": true, "byte": true, "rune": true,
	"float32": true, "float64": true, "complex64": true, "complex128": true,
}

// portable reports whether a signature can be compared across packages by
// its text alone: every named type in it must be predeclared. A local type
// spells the same in two packages and means two different things, and a
// qualified one spells differently under two import aliases — either way,
// text stops being identity, so the comparison is refused rather than
// guessed.
func portable(fn *ast.FuncType) bool {
	ok := true
	check := func(list *ast.FieldList) {
		if list == nil {
			return
		}
		for _, f := range list.List {
			ast.Inspect(f.Type, func(n ast.Node) bool {
				switch e := n.(type) {
				case *ast.Ident:
					if !predeclared[e.Name] {
						ok = false
					}
				case *ast.SelectorExpr:
					ok = false
					return false
				}
				return true
			})
		}
	}
	check(fn.Params)
	check(fn.Results)
	return ok
}
