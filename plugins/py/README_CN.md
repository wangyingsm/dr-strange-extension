# 插件：py

[English](README.md) · 简体中文

将 Python 解析为图事实。Manifest 为 `py@1`，处理 `.py .pyi .pyw`。基于
[ruff](https://github.com/astral-sh/ruff) 的 `ruff_python_parser`——ruff 与
uv 背后的解析器，与语言同步（含 3.12 的 `type` 语句）——只做解析，不做
推断。

## 目录结构

```
parser/     drsg-py-parser——语言逻辑，21 个原生测试
component/  drsg-plugin-py——Guest 实现 + rmp-serde 部分结果
```

## 键——语言自己的规则

模块身份遵循 Python 的导入系统，而非我们的约定：自文件向上，**每个含
`__init__.py` 的目录是一个包，第一个不含它的目录就是 `sys.path` 根**——
因此 `src/` 布局根本不需要特例，`__init__.py` 为其目录命名，正如 `mod.rs`
为其父目录命名。

```
mypkg.core.utils                      src/mypkg/core/utils.py
mypkg.core.utils.parse_row            函数
mypkg.core.utils.Config.load          方法
deploy.main                           游离脚本就是它的文件名主干
```

## 节点

| 标签 | 产生于 | `doc_comment`（docstring）/ `visibility` / `file` / `line` 之外的属性 |
|---|---|---|
| `Module` | 每个文件 | `path`、`imports`；包的 docstring 落在其 `__init__` 模块节点上 |
| `Function` / `Method` | `def` / 类体内的 `def` | `signature` 照原样（`def fetch(url: str, timeout: float = 5.0) -> bytes`）、`is_async` |
| `Class` | 类声明 | `fields`：described 列表，按 Python 书写字段的**两种方式**读取——类级注解，以及 `__init__` 里对 `self` 的赋值（`url: str`、`open`） |
| `Const` / `Var` | 模块级赋值——**由 PEP 8 自己的规则裁决**：全大写即 `Const` | 注解在 `signature`，值在 `value` 照原样 |
| `TypeAlias` | 3.12 的 `type X = …` | 被别名类型在 `signature` |
| 替身 | 外部包/成员 | `Package` / `Function` / `Class`（基类可证明其种类）+ `External` |

`visibility: "exported"` 遵循 Python 的星号导入规则：模块声明了 `__all__`
则以之为准，否则取所有不以下划线开头的名字。

## 边

| 类型 | 含义 | `line` |
|---|---|---|
| `CONTAINS` | 包 → 模块（两端都被解析时）→ 声明、类 → 方法 | 声明处 |
| `CALLS` | 函数 → 被调者；**装饰器就是写下来的调用**（`@app.route` 点名了路由器） | 调用/装饰器处 |
| `IMPORTS` | 模块 → 模块（绝对目标；相对导入在解析阶段就地解析，那里知道当前模块是谁） | import 语句 |
| `EXTENDS` | 类 → 基类——句法；带下标的基类延伸到它所下标的对象（`Generic[T]` → `typing.Generic`） | 类所在行 |

## 解析——确定性的界线

- `from pkg.mod import name` 经模块集合绑定，含别名；`import pkg.util`
  绑定根名，**点链逐模块行走**，一步落在值上即停。
- **相对导入是包的几何**（`from ..a import helper`），按文件所在位置
  解析。
- **星号导入**到达目标的导出表面（`__all__`，否则公开名字）。
- `self.m()` / `cls.m()` 词法解析到类自己的方法——解析器唯一确知的
  接收者。
- 值上的方法或属性是 checker 的事：计数。
- builtin（`print`、`len`、`ValueError`……）不产生任何边。
- 一个判断，注释写在它生效的地方：普通的值赋值**让位于同名的导入绑定**，
  因为 `try: from x import y / except ImportError: y = None` 是回退惯用法，
  导入才是本体。
- `if TYPE_CHECKING:` 导入与 `try:` 回退是模块结构——照走，不跳。overload
  存根共享名字；先见者（带文档）胜。`.py` 旁的 `.pyi` 会合并，计数。

报告注记：未解析的成员/属性调用 · 外部调用 · 合并的声明。

## 选项（`[plugins.py]`）

| 键 | 效果 |
|---|---|
| `include_source = "true"` | 为声明附加 `_code` |

## 构建与测试

```console
$ cd parser && cargo test             # 21 个测试
$ just py-plugin
$ drsg plugin install component/target/wasm32-wasip2/release/drsg_plugin_py.wasm
```

## 已知局限

命名空间包（无 `__init__.py`）按设计在根处断链——对这条规则而言那个目录
*就是* `sys.path` 根；动态属性访问与 monkey-patching 不可见；制造函数的
装饰器（`@functools.wraps` 链）记录的是装饰器调用，而非被制造的表面。
