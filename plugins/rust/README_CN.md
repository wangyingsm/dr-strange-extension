# 插件：rust

[English](README.md) · 简体中文

将 Rust 源码解析为图事实。Manifest 为 `rust@2`，处理 `.rs`。基于
[syn](https://crates.io/crates/syn)——宏生态自身运行其上的解析器——只做
解析：不做类型推断、不展开宏，而这正是重点。`@2` 是**事实格式版本**
（事实的形状从库内原型起变更过一次），与 release 标签相互独立。

## 目录结构

```
parser/     drsg-rust-parser——语言逻辑，普通库，37 个原生测试
component/  drsg-plugin-rust——wasm 封装：Guest 实现 + rmp-serde 部分结果
```

## 键

条目的身份是它的**模块路径**——Rust 程序员如何称呼它，模型就如何认得它：

```
my_crate                              lib.rs（命名 crate 根，而非 "lib"）
my_crate::api::cache                  模块（文件或内联）
my_crate::api::cache::brute_force_search
my_crate::Thing::read                 固有方法
<my_crate::Thing as core::fmt::Display>::fmt    trait 实现方法——真实的
                                      限定路径语法，也是让六个 `impl From<…>`
                                      块不至于都抢占同一个键的唯一方法
```

crate 名来自最近的 `Cargo.toml` 的 `[package] name`（`-` → `_`），经宿主
读取——因此以 `…/foo/src` 为根的摄取仍然键为 `foo::…`，两个 crate 的
`api::Thing` 永不合并。

## 节点

| 标签 | 产生于 | `doc_comment` / `visibility` / `file` / `line` 之外的属性 |
|---|---|---|
| `Module` | 每个文件与每个内联 `mod` | `path`（相对 crate 根：`src/compute/cache.rs`）、`imports`（解析后的 use 目标，连接为串） |
| `Function` / `Method` | 自由函数、impl 函数（带 `self` 才是 `Method`） | `signature`、`returns`、`receiver`、`local_bindings`、`is_async`（仅为真时出现） |
| `Struct` / `Enum` / `Union` | 类型声明 | `fields`（described 列表，`vis name: type`，声明顺序）/ `variants`（described 列表，`Unit`、`Lit(i64)`、`A = 1`）；标注时有 `non_exhaustive` |
| `Trait` | trait 声明 | 其成员成为节点，经 `HAS_METHOD` 到达 |
| `Const` / `Static` | 常量与静态量 | 类型在 `signature`，初始化式在 `value`——**照原样记录，绝不求值**（`256 * 1024` 保持为表达式） |
| `TypeAlias` | `type X = …` | 被别名的类型在 `signature` |
| `Macro` | `macro_rules!` 定义 | — |
| 替身（stand-in） | 被引用但未在此声明的一切 | 标签表明引用证明了什么（`Function`、`Trait`、`Type`，仅见 `use` 时为裸 `External`）+ 额外标签 `External`；**无属性**——对替身而言键即事实 |

配置 `include_source = "true"`（来自 `[plugins.rust]`）时，每个条目附带
`_code`：照原样的源码，described 为仅供检索——`_` 前缀使其不进入 embedding
与模式摘要。

## 边

| 类型 | 含义 | `line` |
|---|---|---|
| `CONTAINS` | 模块 → 条目、类型 → 变体 | 声明处 |
| `HAS_METHOD` | trait/类型 → 其方法 | 方法所在行 |
| `CALLS` | 函数 → 它调用的对象 | **调用处** |
| `IMPLEMENTS` | 类型 → trait（`impl` 块）；`From<i64>` 作为边上的 `impl` 属性存在，而非另铸一个 `From` 节点 | `impl` 关键字 |
| `IMPORTS` | 模块 → 其 `use` 语句命名的对象（有别名时带 `as_written`） | `use` 语句 |
| `INVOKES` | 模块 → 条目位置的宏调用，`arguments` described 在边上——一个**被标记的盲区**：没有任何东西展开宏，其定义的条目缺席，但定义发生之处可寻 | 调用处 |

## 解析——确定性的界线

- 写成**路径**的调用（`fs::read(…)`、`Vec::new()`）按文件自身的 `use`
  列表展开并精确绑定；此处无人声明的路径成为外部替身（"这个 crate 用了
  那个东西"需要的正是这个）。
- **裸名字**按作用域就近绑定；有两个等距候选的名字**视为歧义——计数，
  不猜测**。
- **方法调用**（`.read()`）不写路径，而接收者的类型正是解析器无从知晓
  的：计数，绝不猜测。
- **重导出**（`pub use`，含 `pub(crate) use`）创建后续引用赖以解析的
  门面路径。
- 同一键出现两次几乎总是同一条目的两个 `#[cfg]` 分支——在此解决（先见者
  胜）并计数，不当作冲突。

每项计数都落入报告注记，让稀疏的图自己解释自己：未解析的方法调用、外部
调用、歧义名字、未展开的宏调用。

## 选项（drsg.toml 的 `[plugins.rust]`）

| 键 | 效果 |
|---|---|
| `include_source = "true"` | 为条目附加 `_code` |

## 构建与测试

```console
$ cd parser    && cargo test          # 37 个测试，无需 wasm 工具链
$ just rust-plugin                    # → component/target/wasm32-wasip2/release/drsg_plugin_rust.wasm
$ drsg plugin install …/drsg_plugin_rust.wasm
```

部分结果以 **MessagePack**（`rmp-serde`）跨越阶段边界：选二进制因为大树的
部分结果以兆计，选自描述因为事实携带 `serde_json::Value` 属性——部分结果
的格式是插件自己的事，宿主从不查看。

## 已知局限

宏生成的条目缺席（由 `INVOKES` 标记）；泛型接收者上的 trait 方法调用属于
方法调用，因而被计数；`#[cfg]` 的取舍不做求值——两个分支的条目都存在，
重复者计数。
