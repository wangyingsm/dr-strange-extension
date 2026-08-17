package parser

import (
	"encoding/json"
	"os"
	"sort"
	"strings"
	"testing"
)

// mapFiles is the host, as a plain map — same discipline as the Rust
// parser's TestFiles: the tests exercise the parser, not a filesystem.
type mapFiles struct {
	files map[string]string
	label string
}

func (m mapFiles) List(suffix string) ([]string, error) {
	var out []string
	for k := range m.files {
		if strings.HasSuffix(k, suffix) {
			out = append(out, k)
		}
	}
	sort.Strings(out)
	return out, nil
}

func (m mapFiles) Read(path string) ([]byte, error) {
	if src, ok := m.files[path]; ok {
		return []byte(src), nil
	}
	return nil, notFound(path)
}

func (m mapFiles) Label() (string, bool) { return m.label, m.label != "" }

type notFound string

func (n notFound) Error() string { return "not found: " + string(n) }

// run parses every .go file in one chunk and assembles — the two phases,
// end to end, the way the component drives them.
func run(t *testing.T, m mapFiles) Assembled {
	t.Helper()
	paths, _ := m.List(".go")
	return Assemble(ParseChunk(m, paths, false))
}

func node(t *testing.T, a Assembled, key string) Node {
	t.Helper()
	for _, n := range a.Nodes {
		if n.Key == key {
			return n
		}
	}
	t.Fatalf("no node %q in %v", key, keys(a))
	return Node{}
}

func keys(a Assembled) []string {
	out := make([]string, 0, len(a.Nodes))
	for _, n := range a.Nodes {
		out = append(out, n.Key)
	}
	return out
}

func hasEdge(a Assembled, src, ty, dst string) bool {
	for _, e := range a.Edges {
		if e.Src == src && e.Type == ty && e.Dst == dst {
			return true
		}
	}
	return false
}

func noteContaining(t *testing.T, a Assembled, want string) string {
	t.Helper()
	for _, n := range a.Notes {
		if strings.Contains(n, want) {
			return n
		}
	}
	t.Fatalf("no note containing %q in %v", want, a.Notes)
	return ""
}

// Keys are Go's own qualified names: the module path from go.mod, then the
// directory, then the identifier.
func TestKeysUseTheModulePath(t *testing.T) {
	a := run(t, mapFiles{files: map[string]string{
		"go.mod":        "module example.com/demo\n\ngo 1.22\n",
		"main.go":       "package main\n\nfunc main() { helper() }\n\nfunc helper() {}\n",
		"sub/util.go":   "package sub\n\nfunc Do() {}\n",
		"sub/nested.go": "package sub\n\nfunc More() {}\n",
	}})
	node(t, a, "example.com/demo.helper")
	node(t, a, "example.com/demo/sub.Do")
	if !hasEdge(a, "example.com/demo/sub", "CONTAINS", "example.com/demo/sub.More") {
		t.Fatalf("package must contain its functions: %v", a.Edges)
	}
}

// A nested go.mod starts its own module, the way a workspace member crate
// starts its own crate — the nearest manifest decides.
func TestNestedModulesResolveToTheNearestManifest(t *testing.T) {
	a := run(t, mapFiles{files: map[string]string{
		"go.mod":       "module example.com/outer\n",
		"a.go":         "package outer\n\nfunc A() {}\n",
		"inner/go.mod": "module example.com/inner\n",
		"inner/b.go":   "package inner\n\nfunc B() {}\n",
		"inner/c/d.go": "package c\n\nfunc D() {}\n",
	}})
	node(t, a, "example.com/outer.A")
	node(t, a, "example.com/inner.B")
	node(t, a, "example.com/inner/c.D")
}

// With no go.mod anywhere, the host's label is what the tree is called.
func TestLabelIsTheFallbackModule(t *testing.T) {
	a := run(t, mapFiles{
		files: map[string]string{"x.go": "package x\n\nfunc F() {}\n"},
		label: "myrepo",
	})
	node(t, a, "myrepo.F")
}

// A method hangs off its receiver type, not the package — and the type
// contains it.
func TestMethodsBelongToTheirReceiver(t *testing.T) {
	a := run(t, mapFiles{files: map[string]string{
		"go.mod": "module m\n",
		"t.go": "package m\n\ntype Counter struct{ n int }\n\n" +
			"func (c *Counter) Add(d int) int { return c.n + d }\n",
	}})
	n := node(t, a, "m.Counter.Add")
	if n.Label != "Method" {
		t.Fatalf("label = %q", n.Label)
	}
	if !hasEdge(a, "m.Counter", "CONTAINS", "m.Counter.Add") {
		t.Fatalf("receiver must contain its method: %v", a.Edges)
	}
	sig, _ := n.Props["signature"].(string)
	if !strings.Contains(sig, "*Counter") || !strings.Contains(sig, "Add(d int) int") {
		t.Fatalf("signature = %q", sig)
	}
}

// An unqualified call binds to a function declared in another file of the
// same package — the cross-file half is assemble's whole reason to exist.
func TestUnqualifiedCallsResolveAcrossFiles(t *testing.T) {
	a := run(t, mapFiles{files: map[string]string{
		"go.mod": "module m\n",
		"a.go":   "package m\n\nfunc Caller() { helper() }\n",
		"b.go":   "package m\n\nfunc helper() {}\n",
	}})
	if !hasEdge(a, "m.Caller", "CALLS", "m.helper") {
		t.Fatalf("call must resolve across files: %v", a.Edges)
	}
}

// A qualified call binds through the file's import table to a package this
// tree declares.
func TestQualifiedCallsResolveThroughImports(t *testing.T) {
	a := run(t, mapFiles{files: map[string]string{
		"go.mod":    "module example.com/m",
		"main.go":   "package main\n\nimport \"example.com/m/util\"\n\nfunc main() { util.Do() }\n",
		"util/u.go": "package util\n\nfunc Do() {}\n",
	}})
	if !hasEdge(a, "example.com/m.main", "CALLS", "example.com/m/util.Do") {
		t.Fatalf("qualified call must resolve in-tree: %v", a.Edges)
	}
	if !hasEdge(a, "example.com/m", "IMPORTS", "example.com/m/util") {
		t.Fatalf("import must become an edge: %v", a.Edges)
	}
}

// An aliased import resolves by its alias, and the real package name still
// works when a directory disagrees with its package clause.
func TestAliasedImportsResolve(t *testing.T) {
	a := run(t, mapFiles{files: map[string]string{
		"go.mod":     "module m",
		"main.go":    "package main\n\nimport u \"m/tools\"\n\nfunc main() { u.Go() }\n",
		"tools/t.go": "package helpers\n\nfunc Go() {}\n",
	}})
	if !hasEdge(a, "m.main", "CALLS", "m/tools.Go") {
		t.Fatalf("aliased call must resolve: %v", a.Edges)
	}
}

// A call into a module this tree does not hold becomes an external stand-in
// carrying the import path and nothing else — and the note says how many.
func TestExternalCallsBecomeStandIns(t *testing.T) {
	a := run(t, mapFiles{files: map[string]string{
		"go.mod":  "module m",
		"main.go": "package main\n\nimport \"fmt\"\n\nfunc main() { fmt.Println(\"hi\") }\n",
	}})
	n := node(t, a, "fmt.Println")
	if n.Label != "Function" || len(n.ExtraLabels) != 1 || n.ExtraLabels[0] != "External" {
		t.Fatalf("stand-in shape wrong: %+v", n)
	}
	if len(n.Props) != 0 {
		t.Fatalf("a stand-in carries nothing else: %+v", n.Props)
	}
	p := node(t, a, "fmt")
	if p.Label != "Package" {
		t.Fatalf("the imported package is a stand-in too: %+v", p)
	}
	noteContaining(t, a, "other modules and the standard library")
}

// A method call on a value names no package, and the receiver's type is what
// a parser cannot know — counted, not guessed.
func TestMethodCallsAreCountedUnresolved(t *testing.T) {
	a := run(t, mapFiles{files: map[string]string{
		"go.mod":  "module m",
		"main.go": "package main\n\nfunc main() { var b builder; b.run() }\n\ntype builder struct{}\n\nfunc (b builder) run() {}\n",
	}})
	noteContaining(t, a, "left unresolved")
	if hasEdge(a, "m.main", "CALLS", "m.builder.run") {
		t.Fatal("a receiver call must not be guessed into an edge")
	}
}

// `pkg.Type(x)` is a conversion wearing a call's syntax; the parser knows
// the difference for types this tree declares.
func TestConversionsAreNotCalls(t *testing.T) {
	a := run(t, mapFiles{files: map[string]string{
		"go.mod": "module m",
		"a.go":   "package m\n\ntype ID string\n\nfunc Use(s string) ID { return ID(s) }\n",
	}})
	for _, e := range a.Edges {
		if e.Type == "CALLS" {
			t.Fatalf("a conversion is not a call: %+v", e)
		}
	}
	for _, n := range a.Notes {
		if strings.Contains(n, "unresolved") {
			t.Fatalf("a conversion must not be counted unresolved: %v", a.Notes)
		}
	}
}

// Struct fields are recorded with their types as written, described so the
// property explains itself.
func TestStructFieldsAreDescribed(t *testing.T) {
	a := run(t, mapFiles{files: map[string]string{
		"go.mod": "module m",
		"s.go":   "package m\n\ntype Point struct {\n\tX, Y float64\n\tname string\n}\n",
	}})
	n := node(t, a, "m.Point")
	described, ok := n.Props["fields"].(map[string]any)
	if !ok {
		t.Fatalf("fields prop missing: %+v", n.Props)
	}
	// A list of `name: type` in declaration order — the Rust parser's shape.
	value, ok := described["$value"].([]string)
	if !ok {
		t.Fatalf("fields must be a list: %+v", described)
	}
	want := []string{"X: float64", "Y: float64", "name: string"}
	if len(value) != len(want) {
		t.Fatalf("fields = %v", value)
	}
	for i := range want {
		if value[i] != want[i] {
			t.Fatalf("field %d = %q, want %q", i, value[i], want[i])
		}
	}
}

// Same-package interface satisfaction: structural, and certain.
func TestInterfaceSatisfactionInOnePackage(t *testing.T) {
	a := run(t, mapFiles{files: map[string]string{
		"go.mod": "module m",
		"i.go":   "package m\n\ntype Sink interface {\n\tWrite(p []byte) (int, error)\n\tClose() error\n}\n",
		"t.go": "package m\n\ntype File struct{}\n\n" +
			"func (f *File) Write(p []byte) (int, error) { return 0, nil }\n" +
			"func (f *File) Close() error { return nil }\n",
		"u.go": "package m\n\ntype Half struct{}\n\nfunc (h Half) Close() error { return nil }\n",
	}})
	if !hasEdge(a, "m.File", "IMPLEMENTS", "m.Sink") {
		t.Fatalf("File implements Sink: %v", a.Edges)
	}
	if hasEdge(a, "m.Half", "IMPLEMENTS", "m.Sink") {
		t.Fatal("Half lacks Write and must not match")
	}
}

// Across packages, a satisfaction is only claimed when both signatures are
// spelled entirely in predeclared types — text is identity only there.
func TestCrossPackageSatisfactionNeedsPortableSignatures(t *testing.T) {
	a := run(t, mapFiles{files: map[string]string{
		"go.mod": "module m",
		"i/i.go": "package i\n\ntype Named interface{ Name() string }\n\ntype Custom interface{ Get() Thing }\n\ntype Thing struct{}\n",
		"t/t.go": "package t\n\ntype User struct{}\n\nfunc (u User) Name() string { return \"\" }\n\ntype Thing struct{}\n\ntype Holder struct{}\n\nfunc (h Holder) Get() Thing { return Thing{} }\n",
	}})
	if !hasEdge(a, "m/t.User", "IMPLEMENTS", "m/i.Named") {
		t.Fatalf("portable signatures must match across packages: %v", a.Edges)
	}
	// t.Holder's Get returns t.Thing, the interface wants i.Thing — the same
	// spelling, two meanings. Text stops being identity, so no claim.
	if hasEdge(a, "m/t.Holder", "IMPLEMENTS", "m/i.Custom") {
		t.Fatal("a local type name must not match across packages by text")
	}
}

// An interface embedding another is flattened before matching; embedding
// `error` is known without being declared.
func TestEmbeddedInterfacesFlatten(t *testing.T) {
	a := run(t, mapFiles{files: map[string]string{
		"go.mod": "module m",
		"i.go": "package m\n\ntype Closer interface{ Close() error }\n\n" +
			"type FailingCloser interface {\n\tCloser\n\terror\n}\n",
		"t.go": "package m\n\ntype Both struct{}\n\n" +
			"func (b Both) Close() error { return nil }\n" +
			"func (b Both) Error() string { return \"\" }\n",
	}})
	if !hasEdge(a, "m.Both", "IMPLEMENTS", "m.FailingCloser") {
		t.Fatalf("flattened embeds must match: %v", a.Edges)
	}
}

// An interface embedding something foreign is left unmatched and counted —
// a half-checked satisfaction would be a guess wearing an edge's clothes.
func TestForeignEmbedsLeaveTheInterfaceUnmatched(t *testing.T) {
	a := run(t, mapFiles{files: map[string]string{
		"go.mod": "module m",
		"i.go":   "package m\n\nimport \"io\"\n\ntype Bigger interface {\n\tio.Reader\n\tExtra() int\n}\n",
		"t.go":   "package m\n\ntype T struct{}\n\nfunc (t T) Read(p []byte) (int, error) { return 0, nil }\n\nfunc (t T) Extra() int { return 0 }\n",
	}})
	if hasEdge(a, "m.T", "IMPLEMENTS", "m.Bigger") {
		t.Fatal("an interface with a foreign embed must not be matched")
	}
	noteContaining(t, a, "embedded interface")
}

// The same name in two files of one package is a build-tag variant — the
// first seen is kept, and the count says so.
func TestBuildTagVariantsAreCountedNotFatal(t *testing.T) {
	a := run(t, mapFiles{files: map[string]string{
		"go.mod":       "module m",
		"f_linux.go":   "package m\n\nfunc open() int { return 1 }\n",
		"f_windows.go": "package m\n\nfunc open() int { return 2 }\n",
	}})
	count := 0
	for _, n := range a.Nodes {
		if n.Key == "m.open" {
			count++
		}
	}
	if count != 1 {
		t.Fatalf("one node per key, got %d", count)
	}
	noteContaining(t, a, "more than one file")
}

// Consts and vars follow the Rust parser's `const`/`static` conventions:
// the type as written under `signature`, the initializer as written under
// `value` — never evaluated, so `256 * 1024` stays an expression.
func TestConstsAndVarsCarryTypeAndValue(t *testing.T) {
	a := run(t, mapFiles{files: map[string]string{
		"go.mod": "module m",
		"c.go": "package m\n\n// MaxRetries bounds the loop.\nconst MaxRetries int = 5\n\n" +
			"const Budget = 256 * 1024\n\n" +
			"var registry map[string]int\n\nvar answer = compute()\n\nfunc compute() int { return 42 }\n",
	}})
	c := node(t, a, "m.MaxRetries")
	if c.Label != "Const" || c.Props["signature"] != "int" || c.Props["value"] != "5" {
		t.Fatalf("const shape: %+v", c)
	}
	if doc, _ := c.Props["doc_comment"].(string); !strings.Contains(doc, "bounds the loop") {
		t.Fatalf("doc = %+v", c.Props)
	}
	if b := node(t, a, "m.Budget"); b.Props["value"] != "256 * 1024" {
		t.Fatalf("a value is recorded as written, not evaluated: %+v", b)
	}
	v := node(t, a, "m.registry")
	if v.Label != "Var" || v.Props["signature"] != "map[string]int" {
		t.Fatalf("var shape: %+v", v)
	}
	if _, has := v.Props["value"]; has {
		t.Fatalf("no initializer means no value: %+v", v)
	}
	if w := node(t, a, "m.answer"); w.Props["value"] != "compute()" {
		t.Fatalf("initializer as written: %+v", w)
	}
}

// A const block with no expressions repeats the previous spec — the
// language's own iota rule, recorded rather than evaluated.
func TestIotaLaddersRepeatTheExpression(t *testing.T) {
	a := run(t, mapFiles{files: map[string]string{
		"go.mod": "module m",
		"c.go":   "package m\n\nconst (\n\tKB uint64 = 1 << (10 * (iota + 1))\n\tMB\n\tGB\n)\n",
	}})
	for _, name := range []string{"m.KB", "m.MB", "m.GB"} {
		n := node(t, a, name)
		if n.Props["value"] != "1 << (10 * (iota + 1))" || n.Props["signature"] != "uint64" {
			t.Fatalf("%s must inherit the ladder's expression and type: %+v", name, n)
		}
	}
}

// An interface's demanded methods are nodes of their own, reached by
// HAS_METHOD — the way the Rust parser treats a trait's items.
func TestInterfaceMethodsAreNodes(t *testing.T) {
	a := run(t, mapFiles{files: map[string]string{
		"go.mod": "module m",
		"i.go":   "package m\n\ntype Store interface {\n\t// Get fetches one value.\n\tGet(key string) ([]byte, error)\n}\n",
	}})
	m := node(t, a, "m.Store.Get")
	if m.Label != "Method" {
		t.Fatalf("label = %q", m.Label)
	}
	if sig, _ := m.Props["signature"].(string); !strings.Contains(sig, "Get(key string)") {
		t.Fatalf("signature = %+v", m.Props)
	}
	if doc, _ := m.Props["doc_comment"].(string); !strings.Contains(doc, "fetches one value") {
		t.Fatalf("doc = %+v", m.Props)
	}
	if _, has := m.Props["visibility"]; has {
		t.Fatal("an interface's methods are as public as the interface")
	}
	if !hasEdge(a, "m.Store", "HAS_METHOD", "m.Store.Get") {
		t.Fatalf("HAS_METHOD missing: %v", a.Edges)
	}
}

// The package node carries the union of what its files import — the same
// `imports` property the Rust parser writes on a Module.
func TestThePackageNodeListsItsImports(t *testing.T) {
	a := run(t, mapFiles{files: map[string]string{
		"go.mod": "module m",
		"a.go":   "package m\n\nimport \"fmt\"\n\nfunc A() { fmt.Println() }\n",
		"b.go":   "package m\n\nimport \"sort\"\n\nfunc B() { sort.Strings(nil) }\n",
	}})
	p := node(t, a, "m")
	if p.Props["imports"] != "fmt, sort" {
		t.Fatalf("imports = %+v", p.Props)
	}
}

// Package documentation lands on the package node, whichever file carries it.
func TestPackageDocIsMerged(t *testing.T) {
	a := run(t, mapFiles{files: map[string]string{
		"go.mod": "module m",
		"a.go":   "package m\n\nfunc A() {}\n",
		"doc.go": "// Package m does the thing.\npackage m\n",
	}})
	p := node(t, a, "m")
	if doc, _ := p.Props["doc_comment"].(string); !strings.Contains(doc, "does the thing") {
		t.Fatalf("package doc must merge from doc.go: %+v", p.Props)
	}
}

// `init` shares one name per package: as a node it could only collide, so it
// is deliberately absent.
func TestInitIsSkipped(t *testing.T) {
	a := run(t, mapFiles{files: map[string]string{
		"go.mod": "module m",
		"a.go":   "package m\n\nfunc init() {}\n",
		"b.go":   "package m\n\nfunc init() {}\n",
	}})
	for _, n := range a.Nodes {
		if strings.HasSuffix(n.Key, ".init") {
			t.Fatalf("init must not be a node: %+v", n)
		}
	}
	for _, n := range a.Notes {
		if strings.Contains(n, "more than one file") {
			t.Fatalf("two inits are not a build-tag variant: %v", a.Notes)
		}
	}
}

// A file that will not parse is counted, and takes nothing else down.
func TestAParseErrorIsCountedNotFatal(t *testing.T) {
	a := run(t, mapFiles{files: map[string]string{
		"go.mod": "module m",
		"ok.go":  "package m\n\nfunc Fine() {}\n",
		"bad.go": "package m\n\nfunc {{{\n",
	}})
	if a.Skipped != 1 {
		t.Fatalf("skipped = %d", a.Skipped)
	}
	node(t, a, "m.Fine")
}

// include_source attaches the declaration as written under `_code`.
func TestIncludeSourceAttachesTheDeclaration(t *testing.T) {
	m := mapFiles{files: map[string]string{
		"go.mod": "module m",
		"a.go":   "package m\n\nfunc Shown() int { return 42 }\n",
	}}
	paths, _ := m.List(".go")
	a := Assemble(ParseChunk(m, paths, true))
	n := node(t, a, "m.Shown")
	code, ok := n.Props["_code"].(map[string]any)
	if !ok {
		t.Fatalf("_code missing: %+v", n.Props)
	}
	if !strings.Contains(code["$value"].(string), "return 42") {
		t.Fatalf("_code = %+v", code)
	}
}

// The result must not depend on where the chunk boundaries fell.
func TestChunkBoundariesDoNotChangeTheResult(t *testing.T) {
	m := mapFiles{files: map[string]string{
		"go.mod": "module m",
		"a.go":   "package m\n\nfunc A() { B() }\n",
		"b.go":   "package m\n\nfunc B() {}\n",
		"c/c.go": "package c\n\nfunc C() {}\n",
	}}
	paths, _ := m.List(".go")

	one := Assemble(ParseChunk(m, paths, false))
	var split []FileFacts
	for _, p := range paths {
		split = append(split, ParseChunk(m, []string{p}, false)...)
	}
	other := Assemble(split)

	left, _ := json.Marshal(one)
	right, _ := json.Marshal(other)
	if string(left) != string(right) {
		t.Fatalf("chunking changed the result:\n%s\n%s", left, right)
	}
}

// And the whole run is deterministic: twice in, byte-identical out.
func TestTheSameTreeTwiceGivesTheSameFacts(t *testing.T) {
	m := mapFiles{files: map[string]string{
		"go.mod": "module m",
		"a.go": "package m\n\nimport \"fmt\"\n\ntype I interface{ M() int }\n\ntype T struct{}\n\n" +
			"func (t T) M() int { return 0 }\n\nfunc Run() { fmt.Println(T{}.M()) }\n",
	}}
	paths, _ := m.List(".go")
	left, _ := json.Marshal(Assemble(ParseChunk(m, paths, false)))
	right, _ := json.Marshal(Assemble(ParseChunk(m, paths, false)))
	if string(left) != string(right) {
		t.Fatal("two runs disagreed")
	}
}

// A pushed document parses alone, its package name standing in for a path.
func TestADocumentParsesAlone(t *testing.T) {
	facts := ParseDocument("snippet.go", []byte("package demo\n\nfunc Solo() {}\n"), false)
	a := Assemble(facts)
	node(t, a, "demo.Solo")
}

// A subtree ingest can split a package: a method arrives whose receiver type
// was declared in a file this run never saw. The method proves the type
// exists, so a bare node says so — an edge into nothing would be refused by
// the database, and rightly.
func TestAMissingReceiverTypeIsImplied(t *testing.T) {
	m := mapFiles{files: map[string]string{
		"go.mod":  "module m",
		"part.go": "package m\n\nfunc (w Widget) Draw() {}\n",
	}}
	paths := []string{"part.go"} // whatever declares Widget was not routed
	a := Assemble(ParseChunk(m, paths, false))
	n := node(t, a, "m.Widget")
	if n.Label != "Type" || len(n.Props) != 0 {
		t.Fatalf("an implied type carries only what the method proved: %+v", n)
	}
	if !hasEdge(a, "m.Widget", "CONTAINS", "m.Widget.Draw") {
		t.Fatalf("the edge that implied it must survive: %v", a.Edges)
	}
}

// Every definition knows its file and line, and every written relation
// knows the line it is written on: caller --CALLS(line 4)--> callee(line 8).
func TestLinesAndFilesAreRecorded(t *testing.T) {
	a := run(t, mapFiles{files: map[string]string{
		"go.mod": "module m",
		"a.go": "package m\n" + // 1
			"\n" +
			"import \"fmt\"\n" + // 3
			"\n" +
			"func Caller() {\n" + // 5
			"\tfmt.Println(\"x\")\n" + // 6
			"\thelper()\n" + // 7
			"}\n" +
			"\n" +
			"func helper() {}\n", // 10
	}})
	c := node(t, a, "m.Caller")
	if c.Props["line"] != 5 || c.Props["file"] != "a.go" {
		t.Fatalf("definition site: %+v", c.Props)
	}
	if h := node(t, a, "m.helper"); h.Props["line"] != 10 {
		t.Fatalf("helper line: %+v", h.Props)
	}
	find := func(ty, dst string) Edge {
		for _, e := range a.Edges {
			if e.Type == ty && e.Dst == dst {
				return e
			}
		}
		t.Fatalf("no %s edge to %s: %v", ty, dst, a.Edges)
		return Edge{}
	}
	if e := find("CALLS", "m.helper"); e.Line != 7 {
		t.Fatalf("call site line = %d", e.Line)
	}
	if e := find("CALLS", "fmt.Println"); e.Line != 6 {
		t.Fatalf("external call site line = %d", e.Line)
	}
	if e := find("IMPORTS", "fmt"); e.Line != 3 {
		t.Fatalf("import line = %d", e.Line)
	}
	if e := find("CONTAINS", "m.helper"); e.Line != 10 {
		t.Fatalf("contains carries the declaration line: %d", e.Line)
	}
	// The package spans files; a single file+line on it would be a pick.
	p := node(t, a, "m")
	if _, has := p.Props["file"]; has {
		t.Fatalf("package must not claim one file: %+v", p.Props)
	}
}

// ---- baseline eval board: mined from codegraph + codebase-memory-mcp ----
// Red until receiver typing / stamps / ledger land in the go resolver; run
// with DRSG_EVAL=1 (the `just eval` recipe), skipped in normal CI.

func evalOnly(t *testing.T) {
	if os.Getenv("DRSG_EVAL") == "" {
		t.Skip("baseline eval board — run with DRSG_EVAL=1")
	}
}

// cbm golsp_param_type_simple + the decoy discipline: a parameter's declared
// type names the receiver, and two same-named methods never cross.
func TestParamTypedReceiversResolveWithoutCrossing(t *testing.T) {
	evalOnly(t)
	a := run(t, mapFiles{files: map[string]string{
		"go.mod": "module m\n",
		"t.go": "package m\n\ntype Logger struct{}\n\nfunc (l Logger) Log() int { return 1 }\n\n" +
			"type Other struct{}\n\nfunc (o Other) Log() int { return 2 }\n\n" +
			"func UseIt(lg Logger) int { return lg.Log() }\n\nfunc UseOther(o Other) int { return o.Log() }\n",
	}})
	if !hasEdge(a, "m.UseIt", "CALLS", "m.Logger.Log") {
		t.Fatalf("param type names the receiver: %v", a.Edges)
	}
	if !hasEdge(a, "m.UseOther", "CALLS", "m.Other.Log") {
		t.Fatalf("param type names the receiver: %v", a.Edges)
	}
	if hasEdge(a, "m.UseIt", "CALLS", "m.Other.Log") || hasEdge(a, "m.UseOther", "CALLS", "m.Logger.Log") {
		t.Fatalf("same-named methods must never cross-attribute: %v", a.Edges)
	}
}

// cbm golsp_composite_literal + golsp_return_type: locals typed by a
// composite literal and by a declared return type both dispatch.
func TestLocalInitializersTypeTheReceiver(t *testing.T) {
	evalOnly(t)
	a := run(t, mapFiles{files: map[string]string{
		"go.mod": "module m\n",
		"t.go": "package m\n\ntype Config struct{}\n\nfunc (c *Config) Validate() bool { return true }\n\n" +
			"type File struct{}\n\nfunc (f *File) Read() int { return 0 }\n\n" +
			"func Open(path string) *File { return &File{} }\n\n" +
			"func makeConfig() bool { c := &Config{}\n\treturn c.Validate() }\n\n" +
			"func doRead() int { f := Open(\"/tmp\")\n\treturn f.Read() }\n",
	}})
	if !hasEdge(a, "m.makeConfig", "CALLS", "m.Config.Validate") {
		t.Fatalf("composite-literal init types the local: %v", a.Edges)
	}
	if !hasEdge(a, "m.doRead", "CALLS", "m.File.Read") {
		t.Fatalf("declared return type types the local: %v", a.Edges)
	}
}

// cbm golsp_multi_return: the first value of a (T, error) return is the type.
func TestMultiReturnFirstValueTypesTheLocal(t *testing.T) {
	evalOnly(t)
	a := run(t, mapFiles{files: map[string]string{
		"go.mod": "module m\n",
		"t.go": "package m\n\ntype Conn struct{}\n\nfunc (c *Conn) Close() {}\n\n" +
			"func Dial(addr string) (*Conn, error) { return &Conn{}, nil }\n\n" +
			"func doConnect() { c, _ := Dial(\"localhost\")\n\tc.Close() }\n",
	}})
	if !hasEdge(a, "m.doConnect", "CALLS", "m.Conn.Close") {
		t.Fatalf("first result of a multi-return types the local: %v", a.Edges)
	}
}

// cbm golsp_struct_embedding: a method promoted from an embedded struct
// resolves to the embedded type's method.
func TestEmbeddedMethodsPromoteToTheEmbeddedType(t *testing.T) {
	evalOnly(t)
	a := run(t, mapFiles{files: map[string]string{
		"go.mod": "module m\n",
		"t.go": "package m\n\ntype Base struct{}\n\nfunc (b *Base) Save() {}\n\n" +
			"type Decoy struct{}\n\nfunc (d *Decoy) Save() {}\n\n" +
			"type Extended struct{ Base }\n\nfunc persist(e *Extended) { e.Save() }\n",
	}})
	if !hasEdge(a, "m.persist", "CALLS", "m.Base.Save") {
		t.Fatalf("promotion reaches the embedded type's method: %v", a.Edges)
	}
	if hasEdge(a, "m.persist", "CALLS", "m.Decoy.Save") {
		t.Fatalf("a same-named method on an unrelated type never wins: %v", a.Edges)
	}
}

// cbm golsp_interface_satisfaction: a single-implementer interface call
// resolves to the concrete method (the satisfy pass already knows).
func TestSingleImplInterfaceCallsResolveToTheConcreteMethod(t *testing.T) {
	evalOnly(t)
	a := run(t, mapFiles{files: map[string]string{
		"go.mod": "module m\n",
		"t.go": "package m\n\ntype Store interface {\n\tGet(k string) string\n}\n\n" +
			"type RedisStore struct{}\n\nfunc (r *RedisStore) Get(k string) string { return k }\n\n" +
			"func process(s Store) string { return s.Get(\"k\") }\n",
	}})
	if !hasEdge(a, "m.process", "CALLS", "m.RedisStore.Get") &&
		!hasEdge(a, "m.process", "CALLS", "m.Store.Get") {
		t.Fatalf("an interface-typed receiver dispatches (concrete when unique): %v", a.Edges)
	}
}

// P1 parity: resolved call edges carry stamps, and what stays unresolved
// becomes a queryable UnresolvedRef with a reason — not just a count.
func TestResolvedCallsAreStampedAndMissesAreLedgered(t *testing.T) {
	evalOnly(t)
	a := run(t, mapFiles{files: map[string]string{
		"go.mod": "module m\n",
		"t.go":   "package m\n\nfunc helper() {}\n\nfunc run(x interface{ Weird() }) {\n\thelper()\n\tx.Weird()\n}\n",
	}})
	var stamped bool
	for _, e := range a.Edges {
		if e.Type == "CALLS" && e.Src == "m.run" && e.Dst == "m.helper" {
			if e.Props["_resolved_by"] != nil && e.Props["_confidence"] != nil {
				stamped = true
			}
		}
	}
	if !stamped {
		t.Fatalf("resolved calls carry {_resolved_by,_confidence}: %v", a.Edges)
	}
	var ledgered bool
	for _, n := range a.Nodes {
		if n.Label == "UnresolvedRef" {
			ledgered = true
		}
	}
	if !ledgered {
		t.Fatalf("an unresolvable call is a queryable UnresolvedRef, not a bare count: %v", a.Notes)
	}
}

// cbm golsp_package_level_var: a package-level var's initializer types it.
func TestPackageLevelVarInitializerTypesIt(t *testing.T) {
	evalOnly(t)
	a := run(t, mapFiles{files: map[string]string{
		"go.mod": "module m\n",
		"t.go": "package m\n\ntype Database struct{}\n\nfunc (d *Database) Query() int { return 1 }\n\n" +
			"func NewDatabase() *Database { return &Database{} }\n\nvar db = NewDatabase()\n\n" +
			"func handler() int { return db.Query() }\n",
	}})
	if !hasEdge(a, "m.handler", "CALLS", "m.Database.Query") {
		t.Fatalf("package-level var initializer return types the receiver: %v", a.Edges)
	}
}
