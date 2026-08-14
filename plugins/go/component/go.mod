module github.com/wangyingsm/dr-strange-extensions/plugins/go/component

go 1.26

require (
	github.com/wangyingsm/dr-strange-extensions/plugins/go/parser v0.0.0
	github.com/wangyingsm/dr-strange-extensions/sdk/go v0.0.0
)

require go.bytecodealliance.org/cm v0.3.0 // indirect

// The SDK and the parser ship from this repository; until they are tagged,
// the paths beside this module are the versions.
replace github.com/wangyingsm/dr-strange-extensions/sdk/go => ../../../sdk/go

replace github.com/wangyingsm/dr-strange-extensions/plugins/go/parser => ../parser
