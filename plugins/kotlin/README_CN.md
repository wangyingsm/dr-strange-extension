# 插件：kotlin

[English](README.md) · 简体中文

将 Kotlin 解析为图事实。Manifest 为 `kotlin@1`，处理 `.kt` 与 `.kts`。基于
[tree-sitter-kotlin-ng](https://github.com/tree-sitter-grammars/tree-sitter-kotlin)
——不存在纯 Rust 的 Kotlin 前端，而编辑器都已信任的语法比一个还需赢得信任的
语法是更好的地基。C 运行时与语法经 wasi-sdk 的 clang 编译到
`wasm32-wasip2`，与 Java、C 插件同一套工具链。

回应 [issue #45](https://github.com/wangyingsm/dr-strange/issues/45)。

## 目录结构

```
parser/     drsg-kotlin-parser——语言逻辑，56 个原生测试
component/  drsg-plugin-kotlin——Guest 实现 + rmp-serde 部分结果（构建需要 wasi-sdk）
```

## 键

Kotlin 自己的全限定名。**文件不是节点**：声明是单元，包是容器——所以顶层函数
直接挂在它的包下，而不是 JVM 为它虚构的那个 `FileKt` 类里。

```
com.acme.core                         包
com.acme.core.Engine                  类型
com.acme.core.Engine.start            成员
com.acme.core.Engine.Companion        伴生对象，用 Kotlin 给它的名字
com.acme.core.Engine.Builder          嵌套类型经由外层串接
com.acme.core.helper                  顶层函数
```

## 节点

| 标签 | 产生于 | `doc_comment`（KDoc）/ `visibility` / `file` / `line` / `end_line` 之外的属性 |
|---|---|---|
| `Package` | 每个 `package` 声明一个 | `name`。包在两端都被解析时经 `CONTAINS` 嵌套 |
| `Class` / `Interface` / `Enum` / `Annotation` / `Object` | 类型声明——`object`、`companion object` 与匿名对象都是 `Object` | `fields`：described 的 `name: type` 列表（主构造器中写了 `val`/`var` 的参数就是属性，计入此处）；枚举条目入 `variants`；`modifiers` 照原样记下（`data`、`sealed`、`value`、`abstract`……） |
| `TypeAlias` | `typealias` | `signature`（`Jobs = List<Job>`） |
| `Method` | 成员函数、次构造器（用类型自己的简名）与 `init` 块——重载共享键：一个节点，先见者（带文档）胜 | `signature`（`connect(timeout: Int): Boolean`）；扩展函数另有 `receiver` |
| `Function` | 顶层函数 | 同上 |
| `Property` | 类型的属性——Kotlin 的属性带类型，往往还带一个装着代码的 getter，是读者会导航到的声明 | `signature`（`helper: Helper`） |
| `Const` / `Var` | 顶层 `val` / `var` | `signature` |
| 替身 | 外部类型/成员 | `Class` / `Interface` / `Annotation` / `Function`（引用证明了什么）+ `External` |

每个声明还带有 `end_line`：它在何处结束。于是 `snippet` 读取的正是这个声明本身，
而不是从首行起固定的若干行；读者也无需打开文件就能看出一样东西是六行还是两百行。
`line` 仍然指向**名字**所在处，因此声明上方的文档不会挪动图中记录的起始位置。

## 边

| 类型 | 含义 | `line` |
|---|---|---|
| `CONTAINS` | 包 → 声明 → 成员、外层类型 → 嵌套类型、类型 → 其属性与伴生对象 | 声明处 |
| `HAS_METHOD` | 接口 → 其要求的函数 | 成员 |
| `CALLS` | 函数 → 被调者；`Foo(…)` 是对类型的调用，因为 Kotlin 就是这样写构造；`super.m()` 沿构造调用点名的那一个父类型行走 | 调用处 |
| `IMPORTS` | 文件的每个顶层声明 → 文件导入的对象（Kotlin 的 import 以文件为作用域；读者靠声明导航） | import 语句 |
| `EXTENDS` / `IMPLEMENTS` | 类型 → 其父类型。Kotlin 的语法只说出父类型是否**被构造**——父类必须被构造，接口不可能被构造——所以 `: Base()` 是 extends、`: Runner` 是 implements；而当本树持有目标时，由目标自己的声明定夺 | 类型声明 |
| `ANNOTATED_BY` | 声明 → 其注解——Spring 代码库里注解*就是*架构（`@Service`、`@Transactional`、`@GetMapping`），Android 上它们承载框架（`@Composable`、`@HiltViewModel`、`@Entity`）；编译器指令与 JVM 互操作记号（`@Override`、`@Suppress`、`@JvmStatic`……）是噪音，不入图。边上的 `args` **照原样**记下参数（`@Qualifier("fast")`、`@GetMapping("/orders")`），不作解释：其含义归框架所有 | 注解处 |
| `USES_TYPE` | 声明 → 为其**定型**的类型；`role` 记在边上（`property`、`param`、`return`、`receiver`、`alias`）。泛型参数会被走入——`List<Job>` 就是对 `Job` 的依赖——且只有本树声明的类型才成边。由容器注入的依赖另外带上 `injected` 与 `_injected_confidence`（见下） | — |

## 解析——按编译器读引用的方式读

顺序：**原样已限定** → **同包**（无需 import） → **import**，写了别名则用别名 →
**星号 import**，对本树实际持有的内容求解 → **Kotlin 的默认导入**（`String`、
`List`、常见异常——无需 import 即已知）。

- **首字母大写的接收者**（`Helper.create()`、`com.acme.Helper.create()`）是写
  下来的类型引用。**小写的接收者**是值，只在源码陈述了它的类型时定型——声明的
  局部变量、参数、属性，或写了类型的 `for` 变量——只有编译器才知道的则计入未
  解析。`val h = Helper()` 通过它调用的构造陈述了自己的类型。
- **伴生对象**会回答经由其类型名发出的调用：`Engine.create()` 到达
  `Engine.Companion.create`，那正是 Kotlin 放置 Java 写作 `static` 的东西的地
  方。具名伴生对象保留它的名字。
- **继承来的调用**——无接收者的 `helper()` 与 `super.helper()`——沿本树内的父
  类型走到声明该函数的类型：裸调用会走父类与接口，于是接口的默认实现可以回答
  它；`super.helper()` 只走**被构造**的那个父类型。
- **裸调用**还会到达**同包的顶层函数**，而**成员 import**（`import a.b.spin`）
  绑定名字本身。
- 最终无处可解的，计数，绝不猜测。

报告备注：未解析的值接收者调用 · 外部调用 · 合并的声明 · 注入点 · 被接到 bean 的
端口 · 歧义而停住的端口。

## Spring 注入——从源码读出容器自己的接线

在依赖注入下，调用点处的类型是**端口**，而非实现：`orderService.place(id)` 点的
是一个接口，而由哪个 bean 回答它是容器的决定。停在那里的图会说每个 `@Service`
都是死代码，并让每条调用链都止于接口。

那个决定是写下来的，所以它被读取而非猜测——在 Kotlin 里它写在**主构造器**中，
而那是几乎每个 Kotlin bean 声明自身所需之物的方式。

**注入点**是容器提供的依赖，像 `test_flag` 一样作为分级证据记在 `USES_TYPE`
边上：

| `injected` | `_injected_confidence` | 来自 |
|---|---|---|
| `annotation` | `definitive` | 属性或参数上写了 `@Autowired` / `@Inject` / `@Resource` |
| `constructor` | `strong` | 带标记注解的 bean 的**唯一**构造器——Spring 无需注解即注入它；在 Kotlin 里就是主构造器，连 `val` 参数一起 |

**bean** 是带有 `@Service`、`@Component`、`@Repository`、`@Controller`、
`@RestController`、`@Configuration` 或两种 `ControllerAdvice` 之一的类型，或
`@Bean` 函数所声明的返回类型。它的名字取注解自己的参数，否则取按 Java
`Introspector` 规则首字母小写后的简名——连续两个大写则照原样保留（`URLFetcher`）。

**经由被注入端口的调用**会得到第二条 `CALLS` 边，指向 bean 的成员，并以
`_resolved_by` 盖上挑中它的规则、`_confidence`，以及 `_via`——点明源码写下的那个
端口成员。端口自身的边留在它旁边：一条是源码所说，另一条是容器所做。

| `_resolved_by` | `_confidence` | 规则 |
|---|---|---|
| `spring-qualifier` | `high` | 注入点的 `@Qualifier` / `@Resource` 点名了某个 bean |
| `spring-bean` | `high` | 只有一个 bean 实现该端口并能回答该成员 |
| `spring-primary` | `high` | 有多个，而恰好一个是 `@Primary` |
| `spring-bean-collection` | `medium` | 该类型索要端口的 `List` / `Set` / `Collection` / `Iterable` / `Map` / `Sequence`，于是 Spring 把**每一个** bean 都交给它——某一刻究竟哪个在跑，任何地方都没写 |

边指向**声明**该成员的类型，那可能是一个抽象基类：读者要的代码在那里。

当多个 bean 实现同一端口而没有 `@Primary` 或 `@Qualifier` 作选择时，什么都不产
出——报告说出有多少个，而那正是 Spring 自己会启动失败的情形。对不是注入点的端口
值也什么都不产出：构造它的人已经选了实现。只有**接口**会被向下派发；抽象类的成
员有函数体，所以已记录的那条边本就指向会运行的代码。

## 路由

HTTP 路由是本插件唯一无法变成边的引用：处理它或调用它的代码在另一种语言里，往往
还在另一个仓库里，而两侧之间只有一个字符串。因此插件只记录源码所*声明*的内容，把
匹配留给 drsg 的 `wire`——它在每次调用时计算，从不存储。

记录在本就存在的那个符号上：

| 属性 | 记于 | 形状 |
|---|---|---|
| `serves` | 处理函数，以及与之组合出该映射的类 | `GET /api/orders/{}`——动词，以及每个可变段都归约为 `{}` 的路径 |
| `requests` | 发出调用的函数 | 同一形状；源码未陈述动词处记 `*` |
| `serves_as_written` / `requests_as_written` | 两者 | 规范化之前，源码原样写下的路径 |

**声明**来自插件本就连参数一起捕获的那些注解。类级的
`@RequestMapping("/bridge")` 与函数的 `@GetMapping("/check/{id}")` 组合，动词则
从注解自己的名字里读出——`GetMapping`、`POST`、`HttpGet`——无论框架把动词那个词
放在哪里，所以 Spring、JAX-RS 与 Micronaut 都不需要另一张表。**不**陈述动词的映
射记作 `*`，它匹配该路径上的每个动词，并在答案里说明这一点。

**请求**是任何带路径形状字符串参数的调用，不论被调者叫什么——`RestTemplate`、
`WebClient`、Ktor、Retrofit，以及项目自己的包装一视同仁。绝对 URL 或查询串归约
为路径。字符串模板不是字面量，什么也不陈述。

**声明式客户端什么也不声明。** `@HttpExchange` / `@GetExchange`（以及
`@FeignClient`）看起来与控制器一模一样，含义却相反：接口是在*索要*那个路径，而不
是服务它。在那里记 `serves` 会虚构一个并不存在的处理函数，所以插件目前对两侧都不
记，而不是记错一侧。

## 测试代码

测试代码对"谁调用了这个"的回答与生产代码不同，而分不清两者的读者会把一个测试算作
一个使用者。两个属性说出此事：`test_flag` 承载**证据是什么**，
`_test_flag_confidence` 承载**那份证据值多少**——前缀 `_` 让后者不出现在紧凑渲染
里、也不进入向量，在那里它只是噪音；`cypher`、`get_node` 与 `export` 仍会返回它，
而那正是权衡此标记的读者所在之处。什么都没写下来时，什么都不标。

| `test_flag` | `_test_flag_confidence` | 来自 |
|---|---|---|
| `annotation` | `definitive` | JUnit / TestNG / kotlin.test 的注解——`@Test`、`@ParameterizedTest`、`@RepeatedTest`，以及 `@Before*`/`@After*` 钩子 |
| `build-layout` | `strong` | 位于测试源集下：`src/test`、`src/androidTest`（Android 插件的插桩测试）或 `src/commonTest` |

分级，于是留下的是最强的证据：源集是 Gradle 与 Maven 都强制、而构建文件仍可重划
的布局，而注解是 JUnit 自己对测试的定义。它既到达函数，也到达持有该函数的类型——
类里有一个 `@Test`，这个类就是测试类——所以 `src/main` 里带注解的测试照样被标，而
`src/test` 里不带注解的辅助函数保留较弱的那个标记。

## 选项（`[plugins.kotlin]`）

| 键 | 作用 |
|---|---|
| `include_source = "true"` | 给声明附上 `_code` |

## 构建与测试

```console
$ cd parser && cargo test             # 56 个测试——原生，C 语法也会为宿主编译
$ just kotlin-plugin                  # 需要 wasi-sdk；WASI_SDK 环境变量可覆盖默认路径
$ drsg plugin install component/target/wasm32-wasip2/release/drsg_plugin_kotlin.wasm
```

## 已知限制

**语法会错解两种写法，两者都被恢复而非丢弃。** 当文件后面还有第二个声明时，类上
方不带参数的注解会被重读成一个独立的表达式——否则这个类会连 `@RestController`
及其全部参数一起丢失——而 `fun interface` 与只有一个成员的具名 `companion
object` 则会被包进一个 error 节点里返回。插件把被剥离的注解重新接回它们本来写给
的声明，并走入 error 节点下的子树而不是丢掉它。两者都有测试覆盖，所以修好它们的
语法新版不会弄坏这里的任何东西。

重载按名字解析（所有重载共享一个节点）；父类型链止于本树未持有的类型；lambda 的
函数体把其中的调用归于外层函数（读者会去看的地方），所以
`handlers.forEach { it.handle() }` 仍未解析，而
`for (h: Handler in handlers)` 可以解析——`it` 没有写下来的类型。扩展函数记作其接
收者上的一条 `USES_TYPE` 边，而不是它的成员，因为它并不是。委托属性（`by lazy`、
`by viewModels()`）解析不会越过委托自身的那次调用。`.kts` 构建脚本能解析，但
Gradle DSL 块是带隐式接收者的 lambda，所以其中能解析的东西不多。

Spring 自己的那些边：`@ComponentScan`、`@Profile` 与 `@ConditionalOn*` 一族不被读
取，所以扫描包之外带标记注解的类也会算作 bean；Spring Data 仓库在树里没有实现
——那是 Spring 生成的——所以那些链正确地止于端口。构造器参数绑定到**同名**属性，
在 Kotlin 里 `val` 让它正是那个属性，在别处也是通行的约定。

路由：servlet 的 context path 与网关的重写属于部署配置，所以声明按源码陈述的样子
记录，请求实际携带的前缀在这里并不知晓。
