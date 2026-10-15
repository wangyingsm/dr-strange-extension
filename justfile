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

# `catalog.json` is what `drsg plugin install` reads — the official list, kept
# here rather than in the database's source tree so a plugin release needs no
# drsg release. Shape only; `check-catalog-online` also verifies every pinned
# hash against the release it names.
check-catalog:
    ./scripts/check-catalog.sh

# What CI runs, and what to run before hand-editing catalog.json.
check-catalog-online:
    ./scripts/check-catalog.sh --online

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
#
# `wasip2-deep-stack.json` for the stack, because go/parser and go/printer
# recurse over the shape of what they read and this stack is not the host's
# to grow: it is fixed when the plugin is linked, sits at the bottom of
# linear memory (`--stack-first`), and has no guard page under it — so
# exhausting it walks the stack pointer past zero, wraps, and faults as an
# out-of-bounds access that never says "stack". `Config::max_wasm_stack` is a
# different stack and does nothing here. At the 64 KiB default a nested
# composite literal traps between 120 and 160 levels deep, measured; 1 MiB
# buys about sixteen times that, for fifteen more pages of a store whose
# limit is gigabytes.
#
# It has to be a target file. TinyGo's own `-stack-size` is the *goroutine*
# stack — under `-scheduler=none` there are no goroutines and it changes
# nothing, verified: the build accepts `-stack-size=1MB` and still emits
# `__stack_pointer = 65536`. What sets the wasm stack is wasm-ld's
# `-z stack-size`, which TinyGo never passes, so the target adds it.
#
# Hence the assertion, which is how that was caught and is how the next
# silent revert will be. wasm-tools is already a build dependency — TinyGo's
# wasip2 target shells out to it to lift the module into a component.
#
# Build the Go plugin: no scheduler, leaking GC, and a 1 MiB guest stack.
go-plugin:
    cd plugins/go/component && tinygo build -target=wasip2-deep-stack.json \
        -scheduler=none -gc=leaking \
        --wit-package ./wit --wit-world drsg:preprocess-build/plugin-go -o go.wasm .
    @wasm-tools print plugins/go/component/go.wasm \
        | grep -q '__stack_pointer.*i32.const 1048576' \
        || { echo "go-plugin: the guest stack is not 1 MiB — see wasip2-deep-stack.json" >&2; exit 1; }

# Build the Rust plugins.
rust-plugin:
    cd plugins/rust/component && cargo build --release --target wasm32-wasip2

ts-plugin:
    cd plugins/ts/component && cargo build --release --target wasm32-wasip2

py-plugin:
    cd plugins/py/component && cargo build --release --target wasm32-wasip2

web-plugin:
    cd plugins/web/component && \
      CC_wasm32_wasip2=${WASI_SDK:-$HOME/.local/opt/wasi-sdk-33.0-x86_64-linux}/bin/clang \
      AR_wasm32_wasip2=${WASI_SDK:-$HOME/.local/opt/wasi-sdk-33.0-x86_64-linux}/bin/llvm-ar \
      CFLAGS_wasm32_wasip2="--sysroot=${WASI_SDK:-$HOME/.local/opt/wasi-sdk-33.0-x86_64-linux}/share/wasi-sysroot" \
      cargo build --release --target wasm32-wasip2

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

# Kotlin and C# are tree-sitter grammars too, and build the same way.
kotlin-plugin:
    cd plugins/kotlin/component && \
      CC_wasm32_wasip2=${WASI_SDK:-$HOME/.local/opt/wasi-sdk-33.0-x86_64-linux}/bin/clang \
      AR_wasm32_wasip2=${WASI_SDK:-$HOME/.local/opt/wasi-sdk-33.0-x86_64-linux}/bin/llvm-ar \
      CFLAGS_wasm32_wasip2="--sysroot=${WASI_SDK:-$HOME/.local/opt/wasi-sdk-33.0-x86_64-linux}/share/wasi-sysroot" \
      cargo build --release --target wasm32-wasip2

csharp-plugin:
    cd plugins/csharp/component && \
      CC_wasm32_wasip2=${WASI_SDK:-$HOME/.local/opt/wasi-sdk-33.0-x86_64-linux}/bin/clang \
      AR_wasm32_wasip2=${WASI_SDK:-$HOME/.local/opt/wasi-sdk-33.0-x86_64-linux}/bin/llvm-ar \
      CFLAGS_wasm32_wasip2="--sysroot=${WASI_SDK:-$HOME/.local/opt/wasi-sdk-33.0-x86_64-linux}/share/wasi-sysroot" \
      cargo build --release --target wasm32-wasip2

toml-plugin:
    cd plugins/toml && cargo build --release --target wasm32-wasip2

# XML, by the dialect a file declares. Pure Rust like the two above: quick-xml
# needs no C, so this one builds without wasi-sdk.
xml-plugin:
    cd plugins/xml && cargo build --release --target wasm32-wasip2

# The manifest reader: pure Rust, no grammar and no C — what it reads is JSON,
# TOML, XML and two line formats, and every reader for those is a crate.
deps-plugin:
    cd plugins/deps && cargo build --release --target wasm32-wasip2

# The git plugin: pure Rust, no grammar and no C, because what it reads is a
# binary format rather than a language.
git-plugin:
    cd plugins/git/component && cargo build --release --target wasm32-wasip2

# Every native test suite: the parsers prove their facts without a wasm
# toolchain anywhere near them.
test:
    cd plugins/rust/parser && cargo test
    cd plugins/go/parser && go test ./...
    cd plugins/ts/parser && cargo test
    cd plugins/py/parser && cargo test
    cd plugins/java/parser && cargo test
    cd plugins/kotlin/parser && cargo test
    cd plugins/csharp/parser && cargo test
    cd plugins/c/parser && cargo test
    cd plugins/web/parser && cargo test
    cd plugins/git/parser && cargo test
    cd plugins/toml && cargo test
    cd plugins/deps && cargo test

# The P0 eval board: every known resolution gap as an ignored test, red
# until its phase lands. Failing here is the expected state — this recipe
# exists to watch the reds turn green, not to gate CI.
eval:
    -cd plugins/py/parser && cargo test -- --ignored
    -cd plugins/rust/parser && cargo test -- --ignored
    -cd plugins/go/parser && DRSG_EVAL=1 go test ./...
