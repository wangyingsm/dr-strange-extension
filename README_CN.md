<p align="center">
  <img src="assets/logo.svg" alt="dr-strange-extension" width="240">
</p>

<h1 align="center">dr-strange-extension</h1>

<p align="center">
  <a href="https://github.com/wangyingsm/dr-strange">Dr-STRANGE</a> 图数据库的<b>官方扩展仓库</b>：
  在模型阅读源代码之前，先由沙箱化的 WebAssembly 预处理插件将其解析为图事实；
  本仓库同时提供编写自定义插件的 SDK。
</p>

<p align="center">
  <a href="https://github.com/wangyingsm/dr-strange-extension/actions/workflows/ci.yml"><img
    src="https://github.com/wangyingsm/dr-strange-extension/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/wangyingsm/dr-strange-extension/releases"><img
    src="https://img.shields.io/github/v/release/wangyingsm/dr-strange-extension?sort=date&label=latest%20release&color=d9a441" alt="Releases"></a>
  <a href="#许可证与贡献"><img
    src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue" alt="License: MIT OR Apache-2.0"></a>
</p>

<p align="center"><a href="README.md">English</a> · 简体中文</p>

---

## 一段话介绍 Dr-STRANGE

[Dr-STRANGE](https://github.com/wangyingsm/dr-strange) 是一个 AI 原生的嵌入式
图数据库：以软模式（soft schema）组织的节点与边平面（plane）、向量 + 关键词 +
图邻近度的混合检索、时间旅行、变更订阅、自然语言查询——以及 `drsg digest`，
它将文档与代码仓库摄取为知识图谱。当 `digest` 遇到**源代码**时，它不会让模型
去猜测结构：每个文件被路由到一个**预处理插件**，由编译器级的解析器将其解析为
确定无疑的事实。一个只产出事实的仓库，摄取过程**完全不需要调用模型**——AST
不需要推断 `parse()` 调用了 `lex()`，它本来就知道。

这些插件独立于数据库存放于此，是有意为之：官方并不意味着同步发布。解析器
修复问题无需等待数据库发版，数据库发版也无需等待八条工具链。

## 支持的扩展

每个插件都是沙箱化的 `wasm32-wasip2` 组件，直接从 Release URL 安装。drsg 在
安装时固定制品的 sha256，并在每次加载时重新校验。

| 插件 | 处理的扩展名 | 底层解析器 | 安装 |
|---|---|---|---|
| `rust` | `.rs` | [syn](https://crates.io/crates/syn) | [最新版](https://github.com/wangyingsm/dr-strange-extension/releases?q=rust-v&expanded=true) |
| `go` | `.go` | Go 自带的 `go/parser`（经 TinyGo 编译） | [最新版](https://github.com/wangyingsm/dr-strange-extension/releases?q=go-v&expanded=true) |
| `ts` | `.ts .tsx .mts .cts .js .jsx .mjs .cjs` | [swc](https://swc.rs) —— 同时支持 ESM 与 CommonJS | [最新版](https://github.com/wangyingsm/dr-strange-extension/releases?q=ts-v&expanded=true) |
| `py` | `.py .pyi .pyw` | [ruff](https://github.com/astral-sh/ruff) 的解析器 | [最新版](https://github.com/wangyingsm/dr-strange-extension/releases?q=py-v&expanded=true) |
| `java` | `.java` | [tree-sitter-java](https://github.com/tree-sitter/tree-sitter-java) | [最新版](https://github.com/wangyingsm/dr-strange-extension/releases?q=java-v&expanded=true) |
| `c` | `.c .h` | [tree-sitter-c](https://github.com/tree-sitter/tree-sitter-c) | [最新版](https://github.com/wangyingsm/dr-strange-extension/releases?q=c-v&expanded=true) |
| `web` | `.html .htm .css` | tree-sitter html/css/js —— 一个插件同时处理两种语言，`class="btn"` 才能绑定到定义 `.btn` 的样式表 | [最新版](https://github.com/wangyingsm/dr-strange-extension/releases?q=web-v&expanded=true) |
| `toml` | `.toml` | [toml](https://crates.io/crates/toml) —— 最小的、但仍然完整的插件 | [最新版](https://github.com/wangyingsm/dr-strange-extension/releases?q=toml-v&expanded=true) |

每个「最新版」链接会将[发布页](https://github.com/wangyingsm/dr-strange-extension/releases)
过滤到该插件的标签，最新的排在最前；每个发布都带有 `<plugin>.wasm` 与其
`.sha256`。最省事的方式不需要任何 URL：不带参数的 `drsg plugin install`
会交互式列出这份目录，固定在与你的 drsg 构建兼容的已验证版本上。

```console
$ drsg plugin install https://github.com/wangyingsm/dr-strange-extension/releases/download/<tag>/rust.wasm
installed rust@2  sha256:8e3c32be0add
  handles: .rs
```

所有解析器遵循同一条纪律：键（key）使用语言**自己的**全限定名
（`crate::module::fn`、`pkg.Type.Method`、`file.c::func`、`index.html#map`），
每个定义带有 `file` 与 `line`，每条边带有其书写位置的行号——而解析器无法
确定的一切（方法接收者的类型、多个定义中链接的是哪一个、被两个样式表定义的
类），一律**在报告中如实计数，绝不猜测**。

## 契约

插件与宿主之间的契约是一个小的 [WIT](wit/preprocess.wit) world
`drsg:preprocess`，以本仓库为准，drsg 侧保留一份 vendored 副本：

```wit
interface host {
  %list: func(suffix: string) -> result<list<string>, string>;
  read:  func(path: string) -> result<list<u8>, string>;
  label: func() -> option<string>;
}

interface preprocessor {
  describe: func() -> manifest;                          // 名称、版本、扩展名
  parse:    func(subject: input, options: list<tuple<string, string>>)
              -> result<list<u8>, string>;               // 一个分块 → 一份不透明的部分结果
  assemble: func(partials: list<list<u8>>, options: list<tuple<string, string>>)
              -> result<output, string>;                 // 全部部分结果（按序）→ 事实
}
```

**两个阶段，刻意为之。** 宿主把路由到的文件切成分块，并行地对每块调用
`parse`——每次调用一个全新的 store，互不共享——然后以分块顺序把所有部分
结果一次性交给 `assemble`。跨文件解析（import、头文件、barrel 重导出、接口
实现关系）发生在 `assemble` 里、发生在插件内部，因为那是语言语义，而数据库
不持有任何语言语义。部分结果是不透明的字节：你自己的 `assemble` 想读回什么，
就序列化什么。

**输入是拉取，不是推送。** 上面三个 `host` 函数就是**全部**能力授权。插件
拉取它手中文件周围的其他文件——代码解析器就是这样跟随 import 的——读取以
被摄取目录为根，并按解析后的真实路径校验。除此之外沙箱一无所予：没有网络
（`wasi:sockets` 在加载时按名拒绝）、文件系统 preopen 表为空、时钟被冻结、
熵为固定序列、指令与内存均有预算——因此对同一棵树重复摄取，产出的事实
字节级一致。插件产出的一切都以**返回值**交回；只有宿主写入数据库。

## 编写插件

### Rust

依赖 SDK——除 `wit-bindgen` 与 `serde_json` 外不依赖我们的任何东西——然后
实现两阶段契约；无需跨文件处理的场景可用单函数简化接口：

```toml
[dependencies]
# crates.io 发布仍在计划中；在那之前使用 git 依赖：
dr-strange-ext = { git = "https://github.com/wangyingsm/dr-strange-extension" }

[lib]
crate-type = ["cdylib"]
```

```rust
use dr_strange_ext::{Input, Manifest, Output, OutputExt, Simple, host, node, output, simple_plugin};

struct MyPlugin;

impl Simple for MyPlugin {
    fn describe() -> Manifest {
        Manifest { name: "mine".into(), version: "1".into(), extensions: vec!["xyz".into()] }
    }

    /// 每次处理一个 subject；SDK 据此派生 parse/assemble。
    fn process(subject: Input, _options: &[(String, String)]) -> Result<Output, String> {
        let mut out = output();
        if let Input::Files(paths) = subject {
            for path in paths {
                let bytes = host::read(&path)?;
                out.nodes
                    .push(node(&path, "Thing").prop("bytes", bytes.len() as i64).build());
            }
        }
        Ok(out.finish())
    }
}

simple_plugin!(MyPlugin);
```

构建并安装：

```console
$ cargo build --release --target wasm32-wasip2
$ drsg plugin install target/wasm32-wasip2/release/my_plugin.wasm
```

真正的解析器应直接实现生成的 `Guest` trait（参见
[`plugins/rust`](plugins/rust)——`parse` 为每个分块返回序列化的部分结果，
`assemble` 在全部分块之上做解析），并通过
`dr_strange_ext::bindings::drsg::preprocess::host` 拉取相邻文件。

### Go

依赖 SDK 模块，实现 `ext.Plugin` 接口，用 TinyGo（≥ 0.41，且 PATH 中有
`wasm-tools`）构建：

```console
$ go get github.com/wangyingsm/dr-strange-extension/sdk/go
```

```go
package main

import ext "github.com/wangyingsm/dr-strange-extension/sdk/go"

type mine struct{}

func (mine) Describe() ext.Manifest {
    return ext.Manifest{Name: "mine", Version: "1", Extensions: []string{"xyz"}}
}

func (mine) Parse(subject ext.Subject, options map[string]string) ([]byte, error) {
    // 通过 ext.List / ext.Read 拉取文件；序列化你的部分结果。
    return []byte{}, nil
}

func (mine) Assemble(partials [][]byte, options map[string]string) (ext.Output, error) {
    return ext.Output{Nodes: []ext.Node{{Key: "k", Label: "Thing"}}}, nil
}

func init() { ext.Register(mine{}) }
func main() {}
```

```console
$ tinygo build -target=wasip2 -scheduler=none -gc=leaking \
    --wit-package ./wit --wit-world drsg:preprocess-build/plugin-go -o mine.wasm .
```

这些编译参数缺一不可（原因写在 [`justfile`](justfile) 的注释里）；构建用的
world 可直接复制 [`plugins/go/component/wit`](plugins/go/component/wit)。
Go SDK 贯穿一条规则：凡是从 ABI 提升（lift）而来的数据，使用前必须先复制——
`cm` 切片只是一个视图，GC 可能把它脚下的内存移走。

无论用什么语言：构建前先运行 `just check-wit`（vendored 的契约副本必须与
canonical 一致）；并以原生方式测试你的解析器——所有官方插件都把解析器写成
普通库、外面只包一层薄薄的组件封装，因此测试完全不需要 wasm 工具链。

## 许可证与贡献

在 [Apache License 2.0](LICENSE-APACHE) 与 [MIT license](LICENSE-MIT) 之间
任选其一——与数据库本体相同的条款。

欢迎贡献：

- **新语言或新格式**请先开 issue，说明你打算基于哪个解析器构建（惯例是：
  选一个成熟的、最好是该语言官方级的解析器——syn、swc、ruff、tree-sitter——
  包装为 `plugins/<name>/{parser,component}`）。
- **解析器修复**请附带一个"修复前失败、修复后通过"的原生测试；CI 会在每次
  push 上运行所有解析器的测试套件、无缓存的 `clippy -D warnings`，并把全部
  八个组件构建到 `wasm32-wasip2`。
- **契约变更**是唯一必须与数据库同步演进的部分——请先在
  [dr-strange](https://github.com/wangyingsm/dr-strange) 侧发起讨论。

除非你另有明确声明，你有意提交并纳入本作品的任何贡献（定义见 Apache-2.0
许可证），都将按上述方式双重许可，且不附加任何额外条款或条件。
