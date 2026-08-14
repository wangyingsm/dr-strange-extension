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
	"go/ast"
	"go/parser"
	"go/printer"
	"go/token"
	"path"
	"regexp"
	"strings"
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

// Edge is a fact about a relation, between node keys.
type Edge struct {
	Src  string `json:"src"`
	Dst  string `json:"dst"`
	Type string `json:"type"`
}

// Call is a call site held until every file is known. Alias is the package
// qualifier as written ("" for an unqualified call); resolution happens in
// [Assemble], against the file's own import table.
type Call struct {
	Caller string `json:"caller"`
	Alias  string `json:"alias,omitempty"`
	Name   string `json:"name"`
}

// Import is one import declaration. Alias is as written ("" when the default
// name is used) — a list, not a map, because two blank imports may coexist.
type Import struct {
	Alias string `json:"alias,omitempty"`
	Path  string `json:"path"`
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
		imp := Import{Path: strings.Trim(spec.Path.Value, `"`)}
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
	return facts
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
	w.node(parent, key, label, props)
	w.calls(key, d.Body)
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
		props := w.props("", doc, name)
		if fields := w.fieldMap(t.Fields); len(fields) > 0 {
			props["fields"] = map[string]any{
				"$desc":  "the fields it declares, each with its type as written",
				"$value": fields,
			}
		}
		if w.includeSource {
			w.addSource(props, s)
		}
		w.node(w.pkg, key, "Struct", props)
	case *ast.InterfaceType:
		iface := Iface{Key: key, Pkg: w.pkg}
		props := w.props("", doc, name)
		if w.includeSource {
			w.addSource(props, s)
		}
		w.node(w.pkg, key, "Interface", props)
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
				mprops := Props{"signature": "func " + id.Name + sig}
				if text := strings.TrimSpace(m.Doc.Text()); text != "" {
					mprops["doc_comment"] = text
				}
				w.facts.Nodes = append(w.facts.Nodes, Node{Key: mkey, Label: "Method", Props: mprops})
				w.facts.Edges = append(w.facts.Edges, Edge{Src: key, Dst: mkey, Type: "HAS_METHOD"})
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
		w.node(w.pkg, key, label, props)
	}
}

// valueSpec records consts and vars the way the Rust parser records `const`
// and `static`: the type as written under `signature`, and the initializer
// as written under `value` — never evaluated, because `256 * 1024` folded
// wrongly is worse than the expression that produced it.
func (w *walker) valueSpec(d *ast.GenDecl, s *ast.ValueSpec, values []ast.Expr, typ ast.Expr) {
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
			props["value"] = w.print(values[i])
		case len(values) > 0:
			// One expression, several names — `var a, b = f()`. The
			// initializer as written is the fact for each of them.
			props["value"] = w.print(values[0])
		}
		w.node(w.pkg, w.pkg+"."+id.Name, label, props)
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
	ast.Inspect(body, func(n ast.Node) bool {
		call, ok := n.(*ast.CallExpr)
		if !ok {
			return true
		}
		switch fn := call.Fun.(type) {
		case *ast.Ident:
			w.facts.Calls = append(w.facts.Calls, Call{Caller: caller, Name: fn.Name})
		case *ast.SelectorExpr:
			if x, ok := fn.X.(*ast.Ident); ok {
				w.facts.Calls = append(w.facts.Calls, Call{Caller: caller, Alias: x.Name, Name: fn.Sel.Name})
			} else {
				w.facts.Opaque++
			}
		default:
			w.facts.Opaque++
		}
		return true
	})
}

func (w *walker) node(parent, key, label string, props Props) {
	w.facts.Nodes = append(w.facts.Nodes, Node{Key: key, Label: label, Props: props})
	w.facts.Edges = append(w.facts.Edges, Edge{Src: parent, Dst: key, Type: "CONTAINS"})
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

func (w *walker) fieldMap(fields *ast.FieldList) map[string]any {
	if fields == nil {
		return nil
	}
	out := map[string]any{}
	for _, f := range fields.List {
		ty := w.print(f.Type)
		if len(f.Names) == 0 {
			// An embedded field: its name is its type's.
			out[embeddedName(f.Type)] = ty
			continue
		}
		for _, id := range f.Names {
			if id.Name != "_" {
				out[id.Name] = ty
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
