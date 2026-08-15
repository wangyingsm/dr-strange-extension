// Package ext is the Go SDK for drsg preprocessor plugins.
//
// A plugin author takes this one module, implements [Plugin] on a type, and
// wires it up with [Register] — the component-model plumbing (variant
// accessors, cm lists, JSON property strings) stays in here. The wasm
// bindings underneath are generated from the canonical contract at the
// repository root, `wit/preprocess.wit`.
//
// Build with TinyGo against the build world in your plugin's `wit/` directory
// (see `plugins/go/component` for the shape):
//
//	tinygo build -target=wasip2 --wit-package ./wit \
//	    --wit-world drsg:preprocess-build/plugin-go -o plugin.wasm .
package ext

import (
	"bytes"
	"encoding/json"
	"fmt"
	"strings"

	"go.bytecodealliance.org/cm"

	pp "github.com/wangyingsm/dr-strange-extension/sdk/go/bindings/drsg/preprocess/preprocessor"
)

// Manifest names the plugin and claims its file extensions.
type Manifest struct {
	Name    string
	Version string
	// Extensions claimed, without the dot: ["go"].
	Extensions []string
	// Logo is an optional inline SVG shown beside the plugin's name in UIs —
	// the whole <svg>…</svg> element as a string. Empty means the UI shows
	// its default mark. Rendered without script execution.
	Logo string
}

// Doc is a single pushed document: a name and its bytes.
type Doc struct {
	Name  string
	Bytes []byte
}

// Subject is what a `parse` call is given: exactly one of a chunk of tree
// paths (pull the bytes through [Read]) or one pushed document.
type Subject struct {
	Files []string
	Doc   *Doc
}

// Props become the node's JSON properties object. Values may be anything
// `encoding/json` can marshal; use [Described] for a value that should carry
// its own explanation.
type Props map[string]any

// Described wraps a value with the description the database's self-explaining
// property shape expects.
func Described(desc string, value any) map[string]any {
	return map[string]any{"$desc": desc, "$value": value}
}

// Node is a fact about a thing. Key is the stable identity merges happen on.
type Node struct {
	Key         string
	Label       string
	ExtraLabels []string
	Props       Props
}

// Edge is a fact about a relation, between node keys.
type Edge struct {
	Src, Dst, Type string
	Props          Props
}

// Output is what `assemble` returns: facts, prose for a model if any is
// left, and the account of what could not be done.
type Output struct {
	Nodes   []Node
	Edges   []Edge
	Prose   string
	Skipped uint32
	Notes   []string
}

// Plugin is the two-phase contract, in Go clothing.
//
// Parse sees one chunk and returns an opaque partial — serialize whatever
// your Assemble wants to read back; the host never looks inside. It may run
// concurrently in instances that share nothing, so it must depend only on
// its input and the host. Assemble runs once, with every partial in chunk
// order; cross-file resolution belongs there.
type Plugin interface {
	Describe() Manifest
	Parse(subject Subject, options map[string]string) ([]byte, error)
	Assemble(partials [][]byte, options map[string]string) (Output, error)
}

// Register wires a [Plugin] to the component's exports. Call it from an
// `init` (or assign to a package-level `var _`) in the component's main
// package.
func Register(p Plugin) {
	pp.Exports.Describe = func() pp.Manifest {
		m := p.Describe()
		logo := cm.None[string]()
		if m.Logo != "" {
			logo = cm.Some(m.Logo)
		}
		return pp.Manifest{
			Name:       m.Name,
			Version:    m.Version,
			Extensions: cm.ToList(m.Extensions),
			Logo:       logo,
		}
	}

	// Everything lifted from the ABI is **copied before use**: a cm slice is
	// a view into the canonical-ABI buffer, and holding a view across the
	// allocations a real parse makes invites the collector to move the
	// ground out from under it. Measured, not hypothetical — decoding a
	// partial through a view trapped once the input grew past ~20 chunks.
	pp.Exports.Parse = func(subject pp.Input, options cm.List[[2]string]) cm.Result[cm.List[uint8], cm.List[uint8], string] {
		var s Subject
		if files := subject.Files(); files != nil {
			s.Files = make([]string, 0, files.Len())
			for _, p := range files.Slice() {
				s.Files = append(s.Files, strings.Clone(p))
			}
		} else if doc := subject.Document(); doc != nil {
			s.Doc = &Doc{Name: strings.Clone(doc.Name), Bytes: bytes.Clone(doc.Bytes.Slice())}
		}
		partial, err := p.Parse(s, optionMap(options))
		if err != nil {
			return cm.Err[cm.Result[cm.List[uint8], cm.List[uint8], string]](err.Error())
		}
		return cm.OK[cm.Result[cm.List[uint8], cm.List[uint8], string]](cm.ToList(partial))
	}

	pp.Exports.Assemble = func(partials cm.List[cm.List[uint8]], options cm.List[[2]string]) cm.Result[pp.OutputShape, pp.Output, string] {
		raw := make([][]byte, 0, partials.Len())
		for _, part := range partials.Slice() {
			raw = append(raw, bytes.Clone(part.Slice()))
		}
		out, err := p.Assemble(raw, optionMap(options))
		if err != nil {
			return cm.Err[cm.Result[pp.OutputShape, pp.Output, string]](err.Error())
		}
		lowered, err := lower(out)
		if err != nil {
			return cm.Err[cm.Result[pp.OutputShape, pp.Output, string]](err.Error())
		}
		return cm.OK[cm.Result[pp.OutputShape, pp.Output, string]](lowered)
	}
}

func optionMap(options cm.List[[2]string]) map[string]string {
	out := make(map[string]string, options.Len())
	for _, kv := range options.Slice() {
		out[strings.Clone(kv[0])] = strings.Clone(kv[1])
	}
	return out
}

// lower converts the friendly Output to the wire shape, rendering each Props
// to the JSON object string the contract carries.
func lower(out Output) (pp.Output, error) {
	nodes := make([]pp.Node, 0, len(out.Nodes))
	for _, n := range out.Nodes {
		properties, err := propString(n.Props)
		if err != nil {
			return pp.Output{}, fmt.Errorf("node %s: %w", n.Key, err)
		}
		nodes = append(nodes, pp.Node{
			Key:         n.Key,
			Label:       n.Label,
			ExtraLabels: cm.ToList(n.ExtraLabels),
			Properties:  properties,
		})
	}
	edges := make([]pp.Edge, 0, len(out.Edges))
	for _, e := range out.Edges {
		properties, err := propString(e.Props)
		if err != nil {
			return pp.Output{}, fmt.Errorf("edge %s-[%s]->%s: %w", e.Src, e.Type, e.Dst, err)
		}
		edges = append(edges, pp.Edge{
			Src:        e.Src,
			Dst:        e.Dst,
			Type:       e.Type,
			Properties: properties,
		})
	}
	facts := uint32(len(nodes) + len(edges))
	return pp.Output{
		Nodes: cm.ToList(nodes),
		Edges: cm.ToList(edges),
		Prose: out.Prose,
		Report: pp.Report{
			Facts:      facts,
			ProseChars: uint32(len(out.Prose)),
			Skipped:    out.Skipped,
			Notes:      cm.ToList(out.Notes),
		},
	}, nil
}

func propString(p Props) (string, error) {
	if len(p) == 0 {
		return "{}", nil
	}
	b, err := json.Marshal(p)
	if err != nil {
		return "", fmt.Errorf("properties would not serialize: %w", err)
	}
	return string(b), nil
}
