# 插件：toml

[English](README.md) · 简体中文

最小的、但仍然完整的插件。Manifest 为 `toml@1`，处理 `.toml`；每个文件
产出一个 `Manifest` 节点，带字节数与 described 的 `path`。

它同时证明四件事：

1. 契约**仅凭 SDK** 即可实现——它的依赖列表是 `dr-strange-ext` 与 `toml`
   crate，不含我们的其他任何东西；
2. 宿主的能力授权（`list` / `read` / `label`）足以做真实的工作；
3. 插件**不需要数据库自己的任何 crate**——这是被检查的，不是被假设的：
   此处的 `cargo tree` 里没有 `dr-strange-core`、没有 `wasmtime`；
4. 没有跨文件结构的格式只需写**一个函数**——它实现
   [`Simple`](../../sdk/rust)，SDK 派生契约的两个阶段：`parse` 按分块运行
   `process`，`assemble` 按分块顺序拼接。

它同时也是模板：如果你在写第一个插件，先把
[`src/lib.rs`](src/lib.rs) 从头读到尾——约 60 行，每一行都在演示一条惯例
（逐文件节点、described 属性、让稀疏的图自我解释的跳过文件计数）。

## 构建与测试

```console
$ just toml-plugin        # cargo build --release --target wasm32-wasip2
$ drsg plugin install target/wasm32-wasip2/release/drsg_plugin_toml.wasm
```

这里没有 parser/component 之分——没有任何 SDK 自身测试未覆盖、值得原生
测试的东西。
