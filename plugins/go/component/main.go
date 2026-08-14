// The drsg Go preprocessor plugin: the parser, wrapped for the wasm
// contract through the Go SDK. Everything language-shaped lives in the
// parser package, natively tested; this file only crosses the boundary.
package main

import (
	"encoding/json"
	"fmt"

	"github.com/wangyingsm/dr-strange-extensions/plugins/go/parser"
	ext "github.com/wangyingsm/dr-strange-extensions/sdk/go"
)

type goPlugin struct{}

func (goPlugin) Describe() ext.Manifest {
	return ext.Manifest{
		Name:       "go",
		Version:    "1",
		Extensions: []string{"go"},
	}
}

// hostFiles adapts the SDK's host functions to the parser's Files interface.
type hostFiles struct{}

func (hostFiles) List(suffix string) ([]string, error) { return ext.List(suffix) }
func (hostFiles) Read(path string) ([]byte, error)     { return ext.Read(path) }
func (hostFiles) Label() (string, bool)                { return ext.Label() }

func (goPlugin) Parse(subject ext.Subject, options map[string]string) ([]byte, error) {
	include := options["include_source"] == "true"
	var facts []parser.FileFacts
	if subject.Doc != nil {
		facts = parser.ParseDocument(subject.Doc.Name, subject.Doc.Bytes, include)
	} else {
		facts = parser.ParseChunk(hostFiles{}, subject.Files, include)
	}
	// The partial is ours to shape; the host shuttles it and never looks.
	return json.Marshal(facts)
}

func (goPlugin) Assemble(partials [][]byte, _ map[string]string) (ext.Output, error) {
	// In the order given — the host guarantees chunk order, and order is
	// what keeps the result independent of where the boundaries fell.
	var all []parser.FileFacts
	for _, raw := range partials {
		var chunk []parser.FileFacts
		if err := json.Unmarshal(raw, &chunk); err != nil {
			return ext.Output{}, fmt.Errorf("a partial did not decode: %w", err)
		}
		all = append(all, chunk...)
	}
	a := parser.Assemble(all)

	out := ext.Output{Skipped: uint32(a.Skipped), Notes: a.Notes}
	for _, n := range a.Nodes {
		out.Nodes = append(out.Nodes, ext.Node{
			Key:         n.Key,
			Label:       n.Label,
			ExtraLabels: n.ExtraLabels,
			Props:       ext.Props(n.Props),
		})
	}
	for _, e := range a.Edges {
		out.Edges = append(out.Edges, ext.Edge{Src: e.Src, Dst: e.Dst, Type: e.Type})
	}
	return out, nil
}

func init() { ext.Register(goPlugin{}) }

// A component exports; it does not run.
func main() {}
