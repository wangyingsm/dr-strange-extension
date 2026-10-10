# 插件：java

[English](README.md) · 简体中文

将 Java 解析为图事实。Manifest 为 `java@2`，处理 `.java`。基于
[tree-sitter-java](https://github.com/tree-sitter/tree-sitter-java)——不存在
成熟的纯 Rust Java 前端，而每个编辑器都已信任的语法比一个还需赢得信任的
语法是更好的地基。C 运行时与语法经 wasi-sdk 的 clang 编译到
`wasm32-wasip2`。

## 目录结构

```
parser/     drsg-java-parser——语言逻辑，42 个原生测试
component/  drsg-plugin-java——Guest 实现 + rmp-serde 部分结果（构建需要 wasi-sdk）
```

## 键

Java 自己的全限定名。**文件不是节点**：在 Java 里类型是单元，包是容器。

```
com.acme.core                         包
com.acme.core.Engine                  类型
com.acme.core.Engine.start            成员
com.acme.core.Engine.Builder          嵌套类型经由外层串接
```

## 节点

| 标签 | 产生于 | `doc_comment`（javadoc）/ `visibility` / `file` / `line` / `end_line` 之外的属性 |
|---|---|---|
| `Package` | 每个 `package` 声明一个 | `name`；`package-info.java` 的 javadoc。包在两端都被解析时经 `CONTAINS` 嵌套 |
| `Class` / `Interface` / `Enum` / `Record` / `Annotation` | 类型声明 | `fields`：described 的 `name: type` 列表（record 的来自其头部）；枚举常量入 `variants` |
| `Method` | 方法与构造函数——重载共享键：一个节点，先见者（带文档）胜 | `signature`（`void connect(int timeout)`） |
| 替身 | 外部类型/成员 | `Class` / `Interface` / `Annotation` / `Function`（引用证明了什么）+ `External` |

每个声明还带有 `end_line`：它在何处结束。于是 `snippet` 读取的正是这个声明本身，
而不是从首行起固定的若干行；读者也无需打开文件就能看出一样东西是六行还是两百行。
`line` 仍然指向**名字**所在处，因此声明上方的文档不会挪动图中记录的起始位置。

## 边

| 类型 | 含义 | `line` |
|---|---|---|
| `CONTAINS` | 包 → 类型 → 成员、外层类型 → 嵌套类型 | 声明处 |
| `HAS_METHOD` | 接口/注解 → 其要求的方法 | 成员 |
| `CALLS` | 方法 → 被调者；`new Foo(…)` 是对类型的调用；`super.m()` 沿 extends 链行走 | 调用处 |
| `IMPORTS` | 文件的每个顶层类型 → 文件导入的对象（Java 的 import 以文件为作用域；读者靠类型导航） | import 语句 |
| `EXTENDS` / `IMPLEMENTS` | 类 → 父类 / 接口；接口 → 被扩展接口；泛型基类延伸到它所下标的对象（`ArrayList<Double>` → `java.util.ArrayList`） | 类型声明 |
| `ANNOTATED_BY` | 类型/方法 → 其注解——Spring 代码库里注解*就是*架构（`@Service`、`@Transactional`、`@GetMapping`）；`java.lang` 自带的记号（`@Override`、`@Deprecated`……）是噪音，不入图。边上的 `args` **照原样**记下参数（`@Qualifier("fast")`、`@PostMapping("/orders")`），不作解释：其含义归框架所有 | 注解处 |
| `USES_TYPE` | 声明 → 为其**定型**的类型；`role` 记在边上（`field`、`param`、`return`），record 的组件计为字段。泛型参数会被走入——`List<Job>` 就是对 `Job` 的依赖——且只有本树声明的类型才成边。由容器注入的依赖另外带上 `injected` 与 `_injected_confidence`（见下） | — |

## 解析——按 javac 的方式读引用

顺序：**照原样已限定** → **同包**（无需 import——这是语言自己的规则）→
**单类型 import** → **通配 import**，对树内实际持有的类型解析 →
**`java.lang`**（无需 import 即知：`String`、`System`、常见异常）。

- **大写开头的接收者**（`Helper.create()`、`com.acme.Helper.create()`）
  是写下来的类型引用。**小写开头的接收者**是值：源码写明其类别处即可定型
  ——声明的局部变量（含增强 for 的循环变量）、参数、字段——唯编译器才知道
  的，计为未解析。
- **继承调用**——无接收者的 `helper()` 与 `super.helper()`——沿树内继承
  关系行走，找到声明该方法的类型：裸调用走父类与接口两者，故接口的
  `default` 可以应答；而 `super.helper()` **只走父类**，这正是 Java 的去
  向（另一种写法 `Greeter.super.helper()` 不作解析）。
- **静态 import** 直接绑定方法名。
- 上述途径均未命中的引用被计数，绝不猜测。

报告注记：值接收者的未解析调用 · 外部调用 · 合并的声明 · 注入点 ·
连到 bean 的端口调用 · 无从决断的端口调用。

## Spring 注入——从源码里读出容器自己的装配

在依赖注入之下，调用处的类型是**端口**，从不是实现：`orderService.place(id)`
写的是一个接口，由哪个 bean 应答是容器的决定。图若止步于此，便会说每个
`@Service` 都是死代码，并让每条调用链都终止于接口。

这个决定是写在源码里的，所以它被读取，而非猜测。Rust 或 Go 把选择留给构造该
值的人——那是数据流，本插件拒绝猜测；Spring 则把它写进注解，而**凡是写得无从
决断之处，Spring 自己也拒绝启动**。

**注入点**是容器提供的依赖，按分级依据记在 `USES_TYPE` 边上，与 `test_flag`
同一形制：

| `injected` | `_injected_confidence` | 依据 |
|---|---|---|
| `annotation` | `definitive` | 字段、setter 或构造器上写有 `@Autowired` / `@Inject` / `@Resource` |
| `constructor` | `strong` | 带标记注解的 bean **唯一**的构造器，Spring 无需注解即注入——含 Lombok 的 `@RequiredArgsConstructor`，因为字段仍是声明出来的 |

**bean** 是带有 `@Service`、`@Component`、`@Repository`、`@Controller`、
`@RestController`、`@Configuration` 或两种 `ControllerAdvice` 的类型，或是
`@Configuration` 中某个 `@Bean` 方法所声明的返回类型。其名字取注解自身的参数，
否则按 Java `Introspector` 的规则将简单名首字母小写，而连续两个大写字母照原样
保留（`URLFetcher`）。

**经注入端口发出的调用**会另得一条指向 bean 方法的 `CALLS` 边，并以
`_resolved_by` 记下选中它的规则、`_confidence` 记下分量、`_via` 记下源码写下
的那个端口方法。指向端口的边仍留在一旁：一条是源码所言，一条是容器所为。

| `_resolved_by` | `_confidence` | 规则 |
|---|---|---|
| `spring-qualifier` | `high` | 注入点的 `@Qualifier` / `@Resource` 指名了某个 bean |
| `spring-bean` | `high` | 只有一个 bean 实现该端口并应答该方法 |
| `spring-primary` | `high` | 有多个，而其中恰有一个标了 `@Primary` |
| `spring-bean-collection` | `medium` | 该类型要的是端口的 `List` / `Set` / `Collection` / `Iterable` / `Map` / `Stream`，于是 Spring 把**每一个** bean 都交给它——某一刻究竟哪一个在跑，哪里都没有写 |

边指向**声明**该方法的类型，那可能是一个抽象基类：读者要找的代码正在那里。

若有多个 bean 实现该端口而 `@Primary` 与 `@Qualifier` 都未作选择，则不出任何
边——报告会说有多少条，那正是 Spring 本身也会启动失败的情形。对并非注入点的端
口值同样不出边：构造它的人已经选定了实现。唯一的放宽是：某类型注入了一整个容
器的端口，即已声明它要走每一个 bean，故其内该端口的任何值都作分派。

只有**接口**会被分派穿过。抽象类的方法有方法体，已记下的边本就指向会运行的代
码；由此再去找到覆写是另一个问题，不是本插件回答的问题。

## 测试代码

"谁调用了它"这个问题，测试代码与生产代码给出的答案分量不同；分不清两者的读者
会把一个测试算作一个使用者。两条属性说明此事：`test_flag` 记录**依据是什么**，
`_test_flag_confidence` 记录**该依据有多少分量**——下划线前缀使后者不进入紧凑
渲染，也不进入向量，那里它只会是噪声；`cypher`、`get_node` 与 `export` 仍会返
回它，而衡量此标记的读者正是在那里查看。源码中没有写下依据时，不作任何标记。

| `test_flag` | `_test_flag_confidence` | 依据 |
|---|---|---|
| `annotation` | `definitive` | JUnit/TestNG 注解——`@Test`、`@ParameterizedTest`、`@RepeatedTest`、`@TestFactory`、`@TestTemplate`，以及 `@Before*`/`@After*` 生命周期钩子 |
| `build-layout` | `strong` | 位于 `src/test/java` 之下 |

两条规则分级，留下的是更强的那条：源码集是 Maven 与 Gradle 共同强制的布局，但
构建文件仍可改写；而注解是 JUnit 自己对测试的定义。它同时覆盖方法与持有该方法
的类型——类中含有 `@Test` 即为测试类——因此位于 `src/main/java` 的带注解测试
照样被标记，而 `src/test/java` 中未带注解的辅助方法保留较弱的标记。

## 选项（`[plugins.java]`）

| 键 | 效果 |
|---|---|
| `include_source = "true"` | 为声明附加 `_code` |

## 构建与测试

```console
$ cd parser && cargo test             # 42 个测试——原生运行，C 语法同样为宿主编译
$ just java-plugin                    # 需要 wasi-sdk；WASI_SDK 环境变量可覆盖默认路径
$ drsg plugin install component/target/wasm32-wasip2/release/drsg_plugin_java.wasm
```

## 已知局限

重载按名字归并（所有重载共享一个节点）；extends 链止步于树外类型；lambda
体内的调用归于外层方法（读者查看之处）；`var` 局部变量是值，经其调用被
计数。

Spring 相关的边：`@ComponentScan`、`@Profile` 与 `@ConditionalOn*` 一族不被读
取，故扫描范围之外带标记注解的类同样算作 bean；XML 的 bean 定义与编程式注册完
全不读；Spring Data 的仓库在树内没有实现——由 Spring 生成——故那些链条正确地终
止于端口。构造器或 setter 的参数绑定到**同名**字段，这是每个代码库都遵循、而
没有任何代码库能保证的约定；名字不同时仅由类型决断，`@Qualifier` 不随之带过。
lambda 的参数没有类型，故 `handlers.forEach(h -> h.handle())` 仍未解析，而
`for (Handler h : handlers)` 可以解析。`Optional<Port>` 与
`ObjectProvider<Port>` 的注入解析不过包装类型本身。
