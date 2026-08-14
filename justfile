# Refresh every vendored copy of the canonical contract. Run after editing
# `wit/preprocess.wit`; CI fails if a copy has drifted.
vendor-wit:
    cp wit/preprocess.wit sdk/rust/wit/preprocess.wit
    cp wit/preprocess.wit sdk/go/wit/preprocess.wit
    cp wit/preprocess.wit plugins/go/component/wit/deps/preprocess/preprocess.wit

# What CI runs: no copy may differ from the canonical file.
check-wit:
    diff -u wit/preprocess.wit sdk/rust/wit/preprocess.wit
    diff -u wit/preprocess.wit sdk/go/wit/preprocess.wit
    diff -u wit/preprocess.wit plugins/go/component/wit/deps/preprocess/preprocess.wit

# Regenerate the Go SDK's bindings after a contract change. Needs
# `wit-bindgen-go` (go install go.bytecodealliance.org/cmd/wit-bindgen-go@latest).
go-bindings:
    cd sdk/go && wit-bindgen-go generate --world plugin --out bindings ./wit

# Build the Go plugin. `-scheduler=none` because a component exports calls
# rather than running a program — and TinyGo's scheduler runs between a
# wasmexport's return and the host reading the result, where its GC can col-
# lect the very buffer being returned. `-gc=leaking` because the conservative
# collector still trapped under wasmexport; the host runs every call in a
# fresh store, so what leaks dies with the call and the store's memory limit
# is the bound.
go-plugin:
    cd plugins/go/component && tinygo build -target=wasip2 -scheduler=none -gc=leaking \
        --wit-package ./wit --wit-world drsg:preprocess-build/plugin-go -o go.wasm .

# Build the Rust plugins.
rust-plugin:
    cd plugins/rust/component && cargo build --release --target wasm32-wasip2

ts-plugin:
    cd plugins/ts/component && cargo build --release --target wasm32-wasip2

py-plugin:
    cd plugins/py/component && cargo build --release --target wasm32-wasip2

# The Java and C grammars are C (tree-sitter); wasi-sdk's clang compiles them
# for the sandbox — set WASI_SDK to your install
# (https://github.com/WebAssembly/wasi-sdk).
c-plugin:
    cd plugins/c/component && \
      CC_wasm32_wasip2=${WASI_SDK:-$HOME/.local/opt/wasi-sdk-33.0-x86_64-linux}/bin/clang \
      AR_wasm32_wasip2=${WASI_SDK:-$HOME/.local/opt/wasi-sdk-33.0-x86_64-linux}/bin/llvm-ar \
      CFLAGS_wasm32_wasip2="--sysroot=${WASI_SDK:-$HOME/.local/opt/wasi-sdk-33.0-x86_64-linux}/share/wasi-sysroot" \
      cargo build --release --target wasm32-wasip2

java-plugin:
    cd plugins/java/component &&       CC_wasm32_wasip2=${WASI_SDK:-$HOME/.local/opt/wasi-sdk-33.0-x86_64-linux}/bin/clang       AR_wasm32_wasip2=${WASI_SDK:-$HOME/.local/opt/wasi-sdk-33.0-x86_64-linux}/bin/llvm-ar       CFLAGS_wasm32_wasip2="--sysroot=${WASI_SDK:-$HOME/.local/opt/wasi-sdk-33.0-x86_64-linux}/share/wasi-sysroot"       cargo build --release --target wasm32-wasip2

toml-plugin:
    cd plugins/toml && cargo build --release --target wasm32-wasip2

# Every native test suite: the parsers prove their facts without a wasm
# toolchain anywhere near them.
test:
    cd plugins/rust/parser && cargo test
    cd plugins/go/parser && go test ./...
    cd plugins/ts/parser && cargo test
    cd plugins/py/parser && cargo test
    cd plugins/java/parser && cargo test
    cd plugins/c/parser && cargo test
