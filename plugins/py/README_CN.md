# 插件：py

[English](README.md) · 简体中文

将 Python 解析为图事实。Manifest 为 `py@2`，处理 `.py .pyi .pyw`。基于
[ruff](https://github.com/astral-sh/ruff) 的 `ruff_python_parser`——ruff 与
uv 背后的解析器，与语言同步（含 3.12 的 `type` 语句）——只做解析，不做
推断。

## 目录结构

```
parser/     drsg-py-parser——语言逻辑，39 个原生测试
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

| 标签 | 产生于 | `doc_comment`（docstring）/ `visibility` / `file` / `line` / `end_line` 之外的属性 |
|---|---|---|
| `Module` | 每个文件 | `path`、`imports`；包的 docstring 落在其 `__init__` 模块节点上 |
| `Function` / `Method` | `def` / 类体内的 `def` | `signature` 照原样（`def fetch(url: str, timeout: float = 5.0) -> bytes`）、`is_async` |
| `Class` | 类声明 | `fields`：described 列表，按 Python 书写字段的**两种方式**读取——类级注解，以及 `__init__` 里对 `self` 的赋值（`url: str`、`open`） |
| `Const` / `Var` | 模块级赋值——**由 PEP 8 自己的规则裁决**：全大写即 `Const` | 注解在 `signature`，值在 `value` 照原样 |
| `TypeAlias` | 3.12 的 `type X = …` | 被别名类型在 `signature` |
| 替身 | 外部包/成员 | `Package` / `Function` / `Class`（基类可证明其种类）+ `External` |

`visibility: "exported"` 遵循 Python 的星号导入规则：模块声明了 `__all__`
则以之为准，否则取所有不以下划线开头的名字。

每个声明还带有 `end_line`：它在何处结束。于是 `snippet` 读取的正是这个声明本身，
而不是从首行起固定的若干行；读者也无需打开文件就能看出一样东西是六行还是两百行。
`line` 仍然指向**名字**所在处，因此声明上方的文档不会挪动图中记录的起始位置。

## 边

| 类型 | 含义 | `line` |
|---|---|---|
| `CONTAINS` | 包 → 模块（两端都被解析时）→ 声明、类 → 方法 | 声明处 |
| `CALLS` | 函数 → 被调者；**装饰器就是写下来的调用**（`@app.route` 点名了路由器）。当某处调用不再等待时带 `concurrent`：`scheduled`（交给 `create_task`、`gather`、`to_thread`）或 `unawaited`（协程作为语句被调用，此处无人等待它） | 调用/装饰器处 |
| `IMPORTS` | 模块 → 模块（绝对目标；相对导入在解析阶段就地解析，那里知道当前模块是谁） | import 语句 |
| `REFERENCES` | 函数 → 它**作为值传递**（而非调用）的函数；调度器拿到的若是可调用对象本身，则带 `concurrent: scheduled` | 该实参 |
| `EXTENDS` | 类 → 基类——句法；带下标的基类延伸到它所下标的对象（`Generic[T]` → `typing.Generic`） | 类所在行 |
| `USES_TYPE` | 声明 → 其**注解所指**的类型；`role` 记在边上（`field`、`param`、`return`）。下标、联合类型与字符串前向引用都会被走入——`list[Job]` 就是对 `Job` 的依赖。**仅在写下了注解处**：未加注解的参数不陈述任何类型，其沉默并不等于没有依赖 | — |

## 路由

HTTP 路由是本插件唯一无法变成边的引用：处理它或调用它的代码在另一种语言里，往往还
在另一个仓库里，而两侧之间只有一个字符串。因此插件只记录源码所*声明*的内容，把匹配
留给 drsg 的 `wire`——它在每次调用时计算，从不存储。

记录在本就存在的那个符号上：

| 属性 | 记录在 | 形式 |
|---|---|---|
| `serves` | 被装饰的函数，或 URL conf 所指名的视图 | `GET /api/orders/{}`——动词，加上把每个可变段都归约为 `{}` 的路径 |
| `requests` | 发起调用的函数 | 同一形式；源码未声明动词时记为 `*` |
| `serves_as_written` / `requests_as_written` | 两者 | 归一化之前、源码原样写出的那个路径 |

**声明**覆盖 Python 的两种写法。带路径的装饰器记录在它下面的函数上——
`@app.get("/x")`、`@router.post(...)`、`@app.route(...)`——动词从装饰器自己的名字里
读出。而 URL conf 记录在**它所指名的视图**上：`urlpatterns` 列表里的
`path("orders/<int:pk>", views.one)` 会把路由记到 `views.one` 上，这正是 Django 的
写法，完全不需要装饰器。不是路由注册表的注册表什么也不声明。

**前缀在这里解析得比别处更深**，因为 Django 与 FastAPI 都把前缀写在路由之外：

- `APIRouter(prefix="/api")` 会作用到经由该 router 声明的每一条路径。
- 写成**名字**的前缀可以跨模块解析——从 `app.config` 导入的 `API_PREFIX` 会被追溯到
  它的字面量。
- 被 `include()` 进来的 URL conf 位于引入它的那个路径之下；而在某个前缀下被 include
  的 router，则同时处于**两个**前缀之下。

**请求**是任何带有形如路径的字符串参数的调用，无论被调用方叫什么——`requests`、
`httpx`、`urllib`，以及项目内自己包的那一层，都一样。f-string 会保留它的洞
（`f"/orders/{pk}"` → `/orders/{}`），绝对 URL 或查询串会归约为路径。`mock.patch`
的目标是一个限定名而不是路径，因此不会被误认成路径。

**树内解析不到的视图仍保留它的路由。** 若 `path("health/", elsewhere.check)` 指名的
东西本插件无法绑定，这条路由会记录在该 URL conf 自己的模块上，而不是被丢弃——那依然
是一条成立的事实，挂在唯一确定的那个符号上。

## 解析——确定性的界线

- `from pkg.mod import name` 经模块集合绑定，含别名；`import pkg.util`
  绑定根名，**点链逐模块行走**，一步落在值上即停。
- **并发是调用处的事实，而非声明处的。** `is_async` 说明函数可能挂起；只有
  调用才说明是否有人等待，而 `await f()`、`create_task(f())` 与裸 `f()` 是
  三个不同的程序。只记录偏离：在 async 体内 `await` 才是预期。同一调用者以
  多种方式到达同一被调者时会折叠为一条边，边上带的是这些方式的**并集**，
  因此先写下的 await 处不会吞掉被调度处。`unawaited` 只对协程成立：
  `log(x)` 同样是丢弃结果的语句，但那里本就没有可等待之物。
- **由外部构造器定型的局部变量**，其方法解析到外部替身：`q = asyncio.Queue()`
  使 `q.put(...)` 成为 `asyncio.Queue.put` 上的调用；带注解的形参
  （`q: asyncio.Queue`）同样定型——正是这一点把消费者接到生产者填充的队列
  上。节点是**类型**而非实例：一个程序里的两个队列共用它。未加注解的形参
  未陈述任何类型，留在账本里——Python 自己的检查器对它的说法也是如此。
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

## 测试代码

"谁调用了它"这个问题，测试代码与生产代码给出的答案分量不同；分不清两者的读者
会把一个测试算作一个使用者。两条属性说明此事：`test_flag` 记录**依据是什么**，
`_test_flag_confidence` 记录**该依据有多少分量**——下划线前缀使后者不进入紧凑
渲染，也不进入向量，那里它只会是噪声；`cypher`、`get_node` 与 `export` 仍会返
回它，而衡量此标记的读者正是在那里查看。源码中没有写下依据时，不作任何标记。

| `test_flag` | `_test_flag_confidence` | 依据 |
|---|---|---|
| `base-class` | `definitive` | `unittest.TestCase` 子类（同样包括 `IsolatedAsyncioTestCase`、`FunctionTestCase`），无论该文件用什么名字导入它 |
| `filename` | `strong` | `test_*.py`、`*_test.py`、`conftest.py` |

两条规则分级，留下的是更强的那条：文件名只是运行器**默认**收集的形状，
`pyproject.toml` 可以改动它；而 `TestCase` 子类是 unittest 自己对测试的定义——
因此它会覆盖该类及其方法上较弱的文件名标记。裸写的 `TestCase` 基类，只有当同一
文件中的 import 表明它来自 `unittest` 时才算数：本地同名类是别人的 `TestCase`，
标记其子类就会把生产代码称作测试。

## 选项（`[plugins.py]`）

| 键 | 效果 |
|---|---|
| `include_source = "true"` | 为声明附加 `_code` |

## 构建与测试

```console
$ cd parser && cargo test             # 39 个测试
$ just py-plugin
$ drsg plugin install component/target/wasm32-wasip2/release/drsg_plugin_py.wasm
```

## 已知局限

命名空间包（无 `__init__.py`）按设计在根处断链——对这条规则而言那个目录
*就是* `sys.path` 根；动态属性访问与 monkey-patching 不可见；制造函数的
装饰器（`@functools.wraps` 链）记录的是装饰器调用，而非被制造的表面。
