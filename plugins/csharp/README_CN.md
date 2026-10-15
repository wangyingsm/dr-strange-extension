# 插件：csharp

[English](README.md) · 简体中文

将 C# 解析为图事实。Manifest 为 `csharp@1`，处理 `.cs`。基于
[tree-sitter-c-sharp](https://github.com/tree-sitter/tree-sitter-c-sharp)
——不存在纯 Rust 的 C# 前端，而每个编辑器都已信任的语法比一个还需赢得信任的
语法是更好的地基。C 运行时与语法经 wasi-sdk 的 clang 编译到 `wasm32-wasip2`，
与 Java、Kotlin、C 插件同一套工具链。

## 目录结构

```
parser/     drsg-csharp-parser——语言逻辑，60 个原生测试
component/  drsg-plugin-csharp——Guest 实现 + rmp-serde 部分结果（构建需要 wasi-sdk）
```

## 键

C# 自己的全限定名。**文件不是节点**：类型是单元，命名空间是容器——而 `partial`
类型是写在多个文件里的一个声明，它汇成持有每一半成员的那一个节点。

```
Acme.Core                             命名空间
Acme.Core.Engine                      类型
Acme.Core.Engine.Start                成员
Acme.Core.Engine.Engine               构造函数，用类型自己的简名
Acme.Core.Engine.Builder              嵌套类型经由外层串接
```

## 节点

| 标签 | 产生于 | `doc_comment`（`///`）/ `visibility` / `file` / `line` / `end_line` 之外的属性 |
|---|---|---|
| `Namespace` | 每个 `namespace` 声明一个，花括号式与文件作用域式皆然 | `name`。命名空间在两端都被声明时经 `CONTAINS` 嵌套 |
| `Class` / `Interface` / `Struct` / `Record` / `Enum` | 类型声明 | `fields`：described 的 `name: type` 列表（record 的位置参数就是它的属性，计入此处）；枚举成员入 `variants`；`modifiers` 照原样记下（`partial`、`abstract`、`sealed`、`static`……） |
| `Delegate` | `delegate` 声明 | `signature`（`Job Handler(Job input)`） |
| `Method` | 方法、运算符与构造函数（用类型自己的简名）——重载共享键：一个节点，先见者（带文档）胜 | `signature`（`bool Connect(int timeout)`） |
| `Property` | 属性与索引器——C# 的属性带类型，往往还带一个装着代码的访问器，是读者会导航到的声明 | `signature`（`Helper Dep`） |
| 替身 | 外部类型/成员 | `Class` / `Interface` / `Annotation` / `Function`（引用证明了什么）+ `External` |

**字段**只承载一个值，所以它是其类型 `fields` 里的一行，而不是节点；**属性**可以
装代码，所以两者都是。

每个声明还带有 `end_line`：它在何处结束。于是 `snippet` 读取的正是这个声明本身，
而不是从首行起固定的若干行；读者也无需打开文件就能看出一样东西是六行还是两百行。
`line` 仍然指向**名字**所在处，因此声明上方的文档不会挪动图中记录的起始位置。XML
文档只保留其文本，标记（`<summary>`、`<param>`）被去掉——那是文档工具的结构，而
标签之间的词句才是读者想要的。

## 边

| 类型 | 含义 | `line` |
|---|---|---|
| `CONTAINS` | 命名空间 → 类型 → 成员、外层类型 → 嵌套类型 | 声明处 |
| `HAS_METHOD` | 接口 → 其要求的成员 | 成员 |
| `CALLS` | 成员 → 被调者；`new Foo(…)` 是对类型的调用，而 `Foo f = new()` 点的是它被声明为的那个类型；`base.M()` 沿基类链行走；`x?.M()` 到达与 `x.M()` 相同的成员，因为它是否运行并不是图上的事实 | 调用处 |
| `IMPORTS` | 文件的每个顶层类型 → 文件 `using` 的对象（C# 的 `using` 以文件为作用域；读者靠类型导航） | using 指令 |
| `EXTENDS` / `IMPLEMENTS` | 类型 → 其基类型。C# 把基类写在第一位且只写在那里，所以当本树持有目标时由目标自己的声明定夺，不持有时则由 .NET 自己的命名约定定夺：`IRepo` 是接口，`ControllerBase` 是类 | 类型声明 |
| `ANNOTATED_BY` | 声明 → 其特性——ASP.NET 代码库里特性*就是*架构（`[ApiController]`、`[Route]`、`[Authorize]`），且 C# 允许源码省略的 `Attribute` 后缀也会被试一次，于是 `[Marker]` 能到达 `MarkerAttribute`；编译器与调试器记号（`[CompilerGenerated]`、`[DebuggerStepThrough]`……）是噪音，不入图。边上的 `args` **照原样**记下参数（`[Route("api/[controller]")]`），不作解释 | 特性处 |
| `USES_TYPE` | 声明 → 为其**定型**的类型；`role` 记在边上（`field`、`property`、`param`、`return`）。泛型参数会被走入——`List<Job>` 就是对 `Job` 的依赖——且只有本树声明的类型才成边。预定义类型（`int`、`string`）属于语言自己，从不属于本树。由容器接线的依赖另外带上 `injected` 与 `_injected_confidence`（见下） | — |

## 解析——按编译器读引用的方式读

顺序：**原样已限定** → **外层命名空间，再沿其祖先向外**（所以
`Acme.Core.Deep` 看得见 `Acme.Core` 的类型） → **`using` 别名** →
**`using` 的命名空间** → 整个编译的 **`global using`** 命名空间 → **`System`**
（每个文件都在用、却不必在文件头导入的那些类型） → **原样的名字**。

最后那一步是 C# 与 Java 不同之处，而且是故意的。C# 文件导入的是整个命名空间，所
以文件里没有任何东西说出一个裸的 `ControllerBase` 来自其中哪一个。记下源码写的那
个名字保住了这条引用——作为一个不带命名空间的外部替身——而为它虚构一个命名空间
是猜测，丢掉它则会丢掉指向框架的每一个基类、特性与静态调用。

- **PascalCase 的接收者**（`Helper.Create()`、`Acme.Util.Deep.Thing()`）是写下来
  的类型引用。**camelCase 或以 `_` 开头的接收者**是值，只在源码陈述了它的类型时
  定型——声明的局部变量、参数、字段、属性，或写了类型的 `foreach` 变量——只有编
  译器才知道的则计入未解析。`var h = new Helper()` 通过它调用的构造陈述了自己的
  类型。
- **继承来的调用**——无接收者的 `Helper()` 与 `base.Helper()`——沿本树内的基类
  链走到声明该成员的类型。
- `using static Acme.Util.Helper` 把那个类型的成员以它们自己的名字带入作用域，裸
  调用可以到达它们。
- **`partial` 类型**是一个类型：一半向另一半的调用可以解析，因为两半的成员面被合
  并了。
- 最终无处可解的，计数，绝不猜测。

报告备注：未解析的值接收者调用 · 外部调用 · 合并的声明 · 服务注册 · 注入点 · 被
接到实现的服务 · 歧义而停住的服务。

## 依赖注入——从源码读出容器自己的注册表

在依赖注入下，调用点处的类型是**服务**，而非实现：`_orders.Find(id)` 点的是一个
接口，而由哪个类回答它是容器的决定。停在那里的图会说每个实现都是死代码，并让每条
调用链都止于接口。

在 .NET 里，那个决定不是解析器必须去猜的配置——它是一次**调用**，两个类型都写在
里面：

```csharp
services.AddScoped<IOrderService, DefaultOrderService>();
```

所以插件去读它。`AddSingleton`、`AddScoped`、`AddTransient`，它们的 `TryAdd` 与
`AddKeyed` 写法，以及单类型的自注册（`AddScoped<Engine>()`），就是容器自己的 API
所由构成的那四个词。

**注入点**是容器所构建的类型的构造函数参数——构造函数注入是 ASP.NET Core 唯一做
的注入——像 `test_flag` 一样作为分级证据记在 `USES_TYPE` 边上：

| `injected` | `_injected_confidence` | 来自 |
|---|---|---|
| `attribute` | `definitive` | 参数上写了 `[FromServices]` / `[FromKeyedServices]` |
| `constructor` | `strong` | **已注册**类型的构造函数，容器会调用它——包括 C# 12 的主构造函数 |

无人注册的类型的构造函数不是注入点：容器从不调用它，而调用它的人已经选了参数。

**经由被注入服务的调用**会得到第二条 `CALLS` 边，指向实现的成员，并以
`_resolved_by` 盖上挑中它的规则、`_confidence`，以及 `_via`——点明源码写下的那个
服务成员。服务自身的边留在它旁边：一条是源码所说，另一条是容器所做。

| `_resolved_by` | `_confidence` | 规则 |
|---|---|---|
| `di-keyed` | `high` | 注入点的 `[FromKeyedServices("fast")]` 点名了某次注册所用的键 |
| `di-registration` | `high` | 只有一次无键注册能回答该服务与该成员 |
| `di-registration-collection` | `medium` | 该类型索要服务的 `IEnumerable` / `IList` / `ICollection` / `IReadOnlyList`，于是容器把**每一次**注册都交给它——某一刻究竟哪个在跑，任何地方都没写 |

边指向**声明**该成员的类型，那可能是一个基类：读者要的代码在那里。

当多次注册都能回答而没有任何东西在它们之间作选择时，什么都不产出——报告说出有多
少次。.NET 解析到**最后**运行的那次注册，而注册运行的顺序并不写在任何单个文件
里，所以点名其中一个的解析器只是在猜。对不是注入点的服务值也什么都不产出：构造它
的人已经选了实现。带键的注册只回答它注册时所用的那个键。只有**接口**会被向下派
发；具体类型的方法有函数体，所以已记录的那条边本就指向会运行的代码。

## 路由

HTTP 路由是本插件唯一无法变成边的引用：处理它或调用它的代码在另一种语言里，往往
还在另一个仓库里，而两侧之间只有一个字符串。因此插件只记录源码所*声明*的内容，把
匹配留给 drsg 的 `wire`——它在每次调用时计算，从不存储。

记录在本就存在的那个符号上：

| 属性 | 记于 | 形状 |
|---|---|---|
| `serves` | 处理方法，以及与之组合出该路由的类型 | `GET /api/Orders/{}`——动词，以及每个可变段都归约为 `{}` 的路径 |
| `requests` | 发出调用的成员 | 同一形状；源码未陈述动词处记 `*` |
| `serves_as_written` / `requests_as_written` | 两者 | 规范化之前，源码原样写下的路径 |

**声明**来自插件本就连参数一起捕获的那些特性。类型级的
`[Route("api/[controller]")]` 与方法的 `[HttpGet("{id}")]` 组合，动词则从特性自己
的名字里读出——`HttpGet`、`HttpPost`——无论框架把动词那个词放在哪里。**ASP.NET
的路由记号按 ASP.NET 自己填充的方式填充**：`[controller]` 是类型去掉
`Controller` 后缀后的名字，`[action]` 是方法的名字。**不**陈述动词的映射记作
`*`，它匹配该路径上的每个动词，并在答案里说明这一点。

**不带前导斜杠写下的路径仍然是路径**，而 ASP.NET 就是这样写它们每一个的——所以
当特性或被调者本就点明了一个动词或一条路由时，不带斜杠的字面量也算。`[Produces]`
旁边的 `"application/json"` 两者都不点明，仍是散文。

**请求**是任何带路径形状字符串参数的调用，不论被调者叫什么——`HttpClient`、
Refit，以及项目自己的包装一视同仁。绝对 URL 或查询串归约为路径。内插字符串不是字
面量，什么也不陈述。

**声明式客户端什么也不声明。** 一个 Refit 接口看起来与控制器一模一样，含义却相
反：它是在*索要*那个路径，而不是服务它。在那里记 `serves` 会虚构一个并不存在的处
理方法，所以插件目前对两侧都不记，而不是记错一侧。

## 测试代码

测试代码对"谁调用了这个"的回答与生产代码不同，而分不清两者的读者会把一个测试算作
一个使用者。两个属性说出此事：`test_flag` 承载**证据是什么**，
`_test_flag_confidence` 承载**那份证据值多少**——前缀 `_` 让后者不出现在紧凑渲染
里、也不进入向量，在那里它只是噪音；`cypher`、`get_node` 与 `export` 仍会返回它，
而那正是权衡此标记的读者所在之处。什么都没写下来时，什么都不标。

| `test_flag` | `_test_flag_confidence` | 来自 |
|---|---|---|
| `attribute` | `definitive` | xUnit / NUnit / MSTest 的特性——`[Fact]`、`[Theory]`、`[Test]`、`[TestCase]`、`[TestMethod]`，以及 setup/teardown 钩子，带不带 `Attribute` 后缀都算 |
| `project-layout` | `strong` | 位于测试项目目录下——`Acme.Tests` / `Acme.Test` 项目，或 `tests/` 树 |

分级，于是留下的是最强的证据：真正能定此事的 `.csproj` 并不点名任何 `.cs` 文件，
所以目录就是本插件所持有的布局证据，而特性是 xUnit 自己对测试的定义。它既到达方
法，也到达持有该方法的类型——类里有一个 `[Fact]`，这个类就是测试类。

## 选项（`[plugins.csharp]`）

| 键 | 作用 |
|---|---|
| `include_source = "true"` | 给声明附上 `_code` |

## 构建与测试

```console
$ cd parser && cargo test             # 60 个测试——原生，C 语法也会为宿主编译
$ just csharp-plugin                  # 需要 wasi-sdk；WASI_SDK 环境变量可覆盖默认路径
$ drsg plugin install component/target/wasm32-wasip2/release/drsg_plugin_csharp.wasm
```

## 已知限制

重载按名字解析（所有重载共享一个节点）；基类链止于本树未持有的类型；lambda 的函
数体把其中的调用归于外层成员（读者会去看的地方），所以
`handlers.ForEach(h => h.Handle())` 仍未解析，而
`foreach (IHandler h in handlers)` 可以解析——lambda 参数没有写下来的类型。扩展
方法按它们被声明成的静态方法解析，而不在把它们当成员使用的那些调用点解析。声明了
多个命名空间的文件会对所有这些命名空间求解它的引用，比编译器会用的作用域多一层。
泛型类型参数与其他名字无异，所以恰好与某个已声明类型同名的 `T` 会解析到它。

容器自己的那些边：经由工厂 lambda（`AddScoped<IRepo>(sp => new JpaRepo(…))`）、
`typeof` 对、程序集扫描（Scrutor）或第三方容器自己的 DSL 所做的注册不被读取——类
型没有写在调用的类型参数里。装饰器与 `IOptions<T>` 解析不会越过外层包装。构造函
数参数按 .NET 自己的拼法绑定到**同名**成员——`repo` 填 `_repo` 或 `Repo`——这是
每个代码库都遵守、而没有任何东西保证的约定；名字相差超出此范围时，由类型独自定夺。

路由：path base、反向代理的重写与 `MapControllers` 自己的约定属于部署与启动配置，
所以声明按源码陈述的样子记录。Minimal API（`app.MapGet("/x", …)`）被记作**请求**
而非声明，因为它们是带路径形状参数的调用，而调用里没有任何东西说它是一个服务端；
那是下一步要补的。
