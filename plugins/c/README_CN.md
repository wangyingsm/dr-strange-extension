# 插件：c

[English](README.md) · 简体中文

将 C 解析为图事实。Manifest 为 `c@1`，处理 `.c .h`——头文件也是 C，且其
声明承载着文档。基于
[tree-sitter-c](https://github.com/tree-sitter/tree-sitter-c)，与 Java 插件
共用 wasi-sdk 工具链。预处理器**只记录，不展开**。

## 目录结构

```
parser/     drsg-c-parser——语言逻辑，17 个原生测试
component/  drsg-plugin-c——Guest 实现 + rmp-serde 部分结果（构建需要 wasi-sdk）
```

## 键——`{file}::{name}`，C++ 对"此文件即作用域"的自有写法

```
src/common.c                          File 节点（路径照原样——util.c 与 util.h
                                      是两个文件；include 连扩展名一起点名头
                                      文件，所以扩展名就是身份的一部分）
src/common.c::readVarInt              函数
src/wire/msgtx.c::main                每个工具的 main 共存——重复定义天然是
                                      两个事实，永不冲突
stdio.h                               <system> 头：外部 File
```

扁平命名空间键（裸的 `main`）是第一版设计，对仓库而言它是错的：一个链接
后的*程序*每名一定义，而一个仓库是许多程序——首个验证语料中 146 个多处
定义的名字（每个工具的 `main`、每个变体一份的参考实现）在"先定义者胜"
之下无声消失。文件命名空间键让它们各自成立；**绑定**仍遵循 C 的链接模型
（见下）。

## 节点

| 标签 | 产生于 | `doc_comment` / `visibility` / `file` / `line` 之外的属性 |
|---|---|---|
| `File` | 每个 `.c`/`.h` | `includes`：include 清单**以解析后的键呈现**——树内头按路径、`<system>` 头按名——每一项都可追随；无法解析的照原样保留 |
| `Function` | 定义与（未合并的）原型 | `signature`（返回类型 + 声明子，照原样）；文件局部的带 `visibility: "static"` |
| `Struct` / `Union` / `Enum` | 带体的具名类型（`struct foo;` 是前置声明，不定义任何东西） | `fields`（`name: type`）/ `variants`（`WARN = 10`） |
| `TypeAlias` | typedef | 底层类型在 `signature`（内联结构体裁剪到其头部） |
| `Const` / `Macro` | `#define`——对象式是 `Const`，值照原样；函数式是 `Macro`，带参数表 | `value` / `signature`。include guard 属于簿记，不入图 |
| `Var` | 全局量 | 类型在 `signature`，初始化式在 `value`；`extern` 声明让位于定义 |
| 替身 | 按名识别的 libc（`memcpy` 对读者有实义）、`<system>` 头 | `Function` / `File` + `External` |

文档：声明上方的 `/** */`、`/* */` 与连续 `//` 都算——C 用三种方式写文档。

## 边

| 类型 | 含义 | `line` |
|---|---|---|
| `CONTAINS` | 文件 → 其声明；原型被定义合并时移到定义处 | 声明/定义处 |
| `CALLS` | 函数 → 被调者（裸名字——见解析） | 调用处 |
| `IMPORTS` | 文件 → 被 include 的文件：`#include "x.h"` 先同目录、再全树内无歧义的尾部匹配——include 路径是解析器不掌握的构建配置，歧义即计数；`<system>` include 指向外部 File 节点 | `#include` 处 |

## 解析——链接器的模型，就近优先

1. **调用者自己的文件**——static 遮蔽同名全局（编译器的规则），文件自己
   的定义永远是其调用者的最佳答案。
2. **唯一的全局定义**，当整棵树恰好持有一个。
3. **唯一的声明**，当此处无人定义该名——头文件的接口是真实的，即使函数体
   在别处。
4. **按名识别的 libc**（一份精选清单）：外部，因为 `memcpy` 值得看见。
5. 多个文件定义的名字**计数，绝不猜测**——哪个定义被链接是构建配置。

头文件的声明**并入任何存在的定义**：定义赢得节点——它的函数体、它的行号、
它的文件——原型剩下的贡献是其文件的 `IMPORTS` 边。

`#ifdef` 分支与 `extern "C"` 块照走——两个 `platform_init` 变体都是事实。
函数指针与 `ops->read()` 是值的事：计数。

报告注记：未解析（指针、未展开的宏、缺席的库）· 指向多处定义名字的未绑定
调用 · libc 调用 · 无法解析的 include · 合并的声明 · 各自成立的多处定义
名字。

## 选项（`[plugins.c]`）

| 键 | 效果 |
|---|---|
| `include_source = "true"` | 为定义附加 `_code` |

## 构建与测试

```console
$ cd parser && cargo test             # 17 个测试
$ just c-plugin                       # 需要 wasi-sdk；WASI_SDK 环境变量可覆盖
$ drsg plugin install component/target/wasm32-wasip2/release/drsg_plugin_c.wasm
```

## 已知局限

宏不展开，因此宏定义的函数缺席、对函数式宏的调用计入未解析；被当作 `.h`
喂入的 C++ 头大多解析为错误并计为跳过；K&R 风格定义任凭语法处置。
