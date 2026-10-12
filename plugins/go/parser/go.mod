module github.com/wangyingsm/dr-strange-extension/plugins/go/parser

go 1.26

require github.com/wangyingsm/dr-strange-extension/sdk/go v0.0.0

// The route convention (`route`) ships from this repository; until the SDK is
// tagged, the path beside this module is the version.
replace github.com/wangyingsm/dr-strange-extension/sdk/go => ../../../sdk/go
