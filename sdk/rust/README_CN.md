# dr-strange-ext — Rust SDK

[English](README.md) · 简体中文

Rust 插件作者需要的唯一依赖。它包含 `drsg:preprocess` 契约的 WIT 生成绑定、
其上的一层符合人体工学的封装，除此之外不含我们的任何东西：依赖列表就是
`wit-bindgen` 和 `serde_json`，仅此而已。guest 没有理由为了给一个属性命名
而编译整个存储引擎。

## crate 里有什么

| 项 | 说明 |
|---|---|
| `bindings` | `wit_bindgen::generate!` 生成的 `plugin` world——契约的原始类型（`Manifest`、`Input`、`Node`、`Edge`、`Output`、`Report`）以及组件要导出的 `Guest` trait |
| `export_plugin!` | wit-bindgen 的导出宏，在根部重导出：`export_plugin!(MyType)` 把一个 `Guest` 实现接到组件导出上 |
| `host` | 宿主接口，可直接调用：`host::list(suffix)`、`host::read(path)`、`host::label()`——**全部**能力授权 |
| `Simple` + `simple_plugin!` | 面向无跨文件结构格式的单函数简化接口（见下） |
| `node(key, label)` / `edge(src, ty, dst)` | 构建器：`.prop(k, v)`、`.described(k, desc, v)`、`.extra_label(l)`、`.build()`——属性会被渲染为契约携带的 JSON 对象字符串 |
| `output()` + `OutputExt::finish()` | `Output` 累加器；`finish()` 根据已推入的内容填写报告中的事实计数 |
| `partial` | 供 `Simple` 使用的辅助：将 `Output` 经由不透明部分结果通道编码/解码/合并 |

## 编写插件的两种方式

**简化接口**——适用于一个输入的事实不依赖另一个输入的场景：

```rust
use dr_strange_ext::{Input, Manifest, Output, OutputExt, Simple, host, node, output, simple_plugin};

struct Mine;

impl Simple for Mine {
    fn describe() -> Manifest {
        Manifest { name: "mine".into(), version: "1".into(), extensions: vec!["xyz".into()] }
    }

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

simple_plugin!(Mine);
```

SDK 从 `process` 派生契约的两个阶段：`parse` 按分块运行它并把结果序列化为
部分结果；`assemble` 按分块顺序拼接。你完全接触不到两阶段的机制。

**完整契约**——真正的语言解析器应直接实现生成的 `Guest` trait：

```rust
use dr_strange_ext::bindings::exports::drsg::preprocess::preprocessor::{Guest, Input, Manifest, Output};
use dr_strange_ext::export_plugin;

struct Mine;

impl Guest for Mine {
    fn describe() -> Manifest { /* … */ }

    /// 一个分块 → 一份不透明的部分结果。可能在互不共享的实例中并发运行，
    /// 因此只能依赖输入与宿主。
    fn parse(subject: Input, options: Vec<(String, String)>) -> Result<Vec<u8>, String> {
        // 解析每个文件，按你喜欢的方式序列化逐文件事实
        //（官方插件使用 rmp-serde：二进制、自描述）
    }

    /// 全部部分结果按分块顺序 → 最终结果。跨文件解析在这里进行——
    /// 结果不得依赖分块边界落在哪里。
    fn assemble(partials: Vec<Vec<u8>>, options: Vec<(String, String)>) -> Result<Output, String> {
        // 解码、跨文件解析、产出节点/边/注记
    }
}

export_plugin!(Mine);
```

本仓库的每个官方插件都把语言逻辑放在普通的 `parser/` 库 crate 中（原生测试，
无需 wasm 工具链），外面只包一层跨越此边界的薄 `component/` 封装——照抄这个
结构即可。

## 属性与 described 形式

节点的 `properties` 以一个 JSON 对象字符串跨越契约。构建器会为你渲染。两种
形式值得注意：

- 普通值：`.prop("signature", "fn parse(text: &str) -> Ast")`
- **described** 值：`.described("fields", "the fields it declares", vec![…])`
  会变为 `{"$desc": …, "$value": …}`——数据库的自解释属性形式；仪表盘渲染
  其值，按需展示描述。

来自官方插件、值得遵循的惯例：键使用语言自己的全限定名；每个定义带有
`file` 与 `line`（从 1 开始）；`_` 前缀的属性（如 `_code`）仅用于检索，
不进入 embedding；解析器无法确定的一切都计数在注记里，绝不猜测。

## 构建

```console
$ cargo build --release --target wasm32-wasip2
$ drsg plugin install target/wasm32-wasip2/release/<name>.wasm
```

crate 类型必须是 `cdylib`。不需要 wasi-sdk、不需要适配器：纯 Rust guest 用
自带的 `wasm32-wasip2` 目标即可构建。

## 契约副本

这里的 `wit/preprocess.wit` 是仓库根部 canonical 契约的 **vendored 副本**
（crate 无法发布自身之外的文件）。两者不一致时 CI 会失败；canonical 变更后
用 `just vendor-wit` 刷新所有副本。
