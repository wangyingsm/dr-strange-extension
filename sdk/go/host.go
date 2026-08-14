package ext

import (
	"bytes"
	"errors"
	"strings"

	"github.com/wangyingsm/dr-strange-extensions/sdk/go/bindings/drsg/preprocess/host"
)

// Results are copied out of the canonical-ABI buffers before they are
// returned, for the same reason Register copies its arguments: a view into
// lifted memory does not survive the caller's own allocations.

// List returns the readable paths under the host's root ending with `suffix`
// (`""` for all), sorted. This is the whole capability grant: what these
// three functions will answer is exactly what a plugin can reach.
func List(suffix string) ([]string, error) {
	r := host.List(suffix)
	if r.IsErr() {
		return nil, errors.New(*r.Err())
	}
	view := r.OK().Slice()
	out := make([]string, 0, len(view))
	for _, p := range view {
		out = append(out, strings.Clone(p))
	}
	return out, nil
}

// Read returns one file's bytes. Paths outside the root are refused by the
// host, on the resolved path.
func Read(path string) ([]byte, error) {
	r := host.Read(path)
	if r.IsErr() {
		return nil, errors.New(*r.Err())
	}
	return bytes.Clone(r.OK().Slice()), nil
}

// Label is what to call the tree when its contents do not say — typically
// the repository's directory name.
func Label() (string, bool) {
	if l := host.Label(); l.Some() != nil {
		return strings.Clone(*l.Some()), true
	}
	return "", false
}
