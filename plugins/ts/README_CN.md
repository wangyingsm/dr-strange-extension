# 插件：ts

[English](README.md) · 简体中文

将 TypeScript **与** JavaScript 解析为图事实。Manifest 为 `ts@2`，处理
`.ts .tsx .mts .cts .js .jsx .mjs .cjs`——一个解析器覆盖整个生态，混合仓库
因此摄取为事实而不是一半沦为散文。基于 [swc](https://swc.rs) 的
`swc_ecma_parser`（Next.js 背后的解析器），只做解析：不做变换、没有
checker——checker 才能推断的东西，正是这里拒绝去猜的东西。

## 目录结构

```
parser/     drsg-ts-parser——语言逻辑，43 个原生测试
component/  drsg-plugin-ts——Guest 实现 + rmp-serde 部分结果
```

## 键——逻辑模块身份

```
acme                                  包根（index.ts 折叠入其中）
acme/src/util.fmt                     声明
acme/src/api.Client.connect           类成员
@scope/web/src/app.render             作用域包保留两段
```

文件之上最近的 `package.json` 为其命名包（monorepo 解析到最近的 manifest，
如同 workspace 的 crate）；模块 id 是去掉扩展名的 manifest 相对路径，
`/index` 折叠进其目录——`index.ts` 的*本义*，正如 Rust 里的 `mod.rs`。
无 manifest → 宿主的 label。

## 节点

| 标签 | 产生于 | `doc_comment`（JSDoc）/ `visibility` / `file` / `line` / `end_line` 之外的属性 |
|---|---|---|
| `Module` | 每个文件 | `path`（照传入原样）、`imports`（specifier 照原样）；自身无 file/line |
| `Function` | 函数声明**及 `const f = (…) =>` 箭头**——箭头初始化式*就是*函数，如此标注 | `signature`（源码切片，从不重新打印）、`is_async` |
| `Class` | 类声明 | `fields`：described 列表，属性声明的 `name: type`，声明顺序 |
| `Method` | 类成员（可见性照原样：`private`/`protected`）、构造函数、接口方法签名 | `signature` |
| `Interface` | 接口声明 | 属性签名入 `fields`；方法经 `HAS_METHOD` 成为节点 |
| `TypeAlias` / `Enum` | 类型别名、枚举 | 被别名类型在 `signature` / `variants` 为 `Name = 照原样的值` |
| `Const` / `Var` | 非函数初始化式的 `const` / `let`+`var` | 注解在 `signature`，初始化式在 `value`，照原样 |
| 替身 | 其他包及其成员 | `Package` / `Function`（子句证明时为 `Interface`/`Class`）+ `External` |

导出的顶层声明带 `visibility: "exported"`。默认导出有名字时按名入键
（`export default function boot` → `….boot`，可经 `default` 到达），否则
为 `default`。

每个声明还带有 `end_line`：它在何处结束。于是 `snippet` 读取的正是这个声明本身，
而不是从首行起固定的若干行；读者也无需打开文件就能看出一样东西是六行还是两百行。
`line` 仍然指向**名字**所在处，因此声明上方的文档不会挪动图中记录的起始位置。

## 边

| 类型 | 含义 | `line` |
|---|---|---|
| `CONTAINS` | 包 → 模块 → 声明、类 → 成员 | 声明处 |
| `HAS_METHOD` | 接口 → 其方法节点 | 成员 |
| `CALLS` | 函数 → 被调者；`new Foo()` 记为对类的调用；渲染的 JSX 组件（`<Foo />`，大写开头）就是调用。当某处调用不再等待时带 `concurrent`：`scheduled`（交给 `Promise.all`/`race`/`any`、`setTimeout`、`queueMicrotask`）或 `unawaited`（Promise 作为语句被调用，此处无人 await、其后也没有 `.then`） | 调用处 |
| `IMPORTS` | 模块 → 模块（相对）或 → 外部包（裸 specifier） | import 语句 |
| `REFERENCES` | 函数 → 它**作为值传递**（而非调用）的函数；调度器拿到的若是函数本身，则带 `concurrent: scheduled` | 该实参 |
| `IMPLEMENTS` / `EXTENDS` | `class C implements I` / 类→类、接口→接口——TS 里是**句法**，因此确定，Go 的结构性检查做不到这一点 | 类/接口声明 |
| `USES_TYPE` | 声明 → 为其**定型**的类型；`role` 记在边上（`field`、`param`、`return`）。数组元素、联合类型与泛型参数都会被走入——`Job[]` 就是对 `Job` 的依赖。**仅在写下了注解处**：未加注解的参数不陈述任何类型，其沉默并不等于没有依赖 | — |

## 路由

HTTP 路由是本插件唯一无法变成边的引用：处理它的代码在另一种语言里，往往还在另一个
仓库里，而两侧之间只有一个字符串。因此插件只记录源码所*声明*的内容，把匹配留给
drsg 的 `wire`——它在每次调用时计算，从不存储。

记录在本就存在的那个符号上：

| 属性 | 记录在 | 形式 |
|---|---|---|
| `serves` | 注册调用或装饰器所指名的处理函数 | `GET /api/orders/{}`——动词，加上把每个可变段都归约为 `{}` 的路径 |
| `requests` | 发起调用的函数 | 同一形式；源码未声明动词时记为 `*` |
| `serves_as_written` / `requests_as_written` | 两者 | 归一化之前、源码原样写出的那个路径 |

**声明。** 被调用名本身带动词的注册调用——`app.get('/x', h)`、`router.post(...)`、
以及 `.get(...).post(...)` 这样的链式调用——记录在它指名的处理函数上，内联写出的
箭头函数也算。带装饰器的类同样会记录：类上的 `@Controller('/api')` 会与方法上的
`@Get(':id')` 组合；而类装饰器若只有一个形如路径的参数，则无需可识别的名字。不声明
任何路由的装饰器什么也不声明。

**请求。** 任何带有形如路径的字符串参数的调用，无论被调用方叫什么——这也是项目内
自己包的 `request('/x')` 唯一可见的途径。模板字面量会保留它的洞
（`` `/orders/${id}` `` → `/orders/{}`），绝对 URL 或查询串会归约为路径。客户端
自己的 base 会作用到它所请求的路径上：`axios.create({ baseURL })` 与模块级的字面量
常量都会参与拼接。

**挂载不是路由。** `app.use('/api', router)` 给一个 router 一个前缀，这个前缀属于挂
在它上面的每一条路由，而此刻又不属于其中任何一条；因此做挂载的那个函数既不提供也不
请求任何东西。前缀会作用到那些路由上——两层挂载也一样——而没有被任何人挂载的 router
会把自己的路径整条写出。

## 解析——确定性的界线

- **并发是调用处的事实，而非声明处的。** `is_async` 说明函数返回 Promise；
  只有调用才说明是否有人 await 它，而 `await f()`、`Promise.all([f()])` 与
  裸 `f()` 是三个不同的程序。只记录偏离：在 async 体内 `await` 才是预期。
  同一调用者以多种方式到达同一被调者时会折叠为一条边，边上带的是这些方式的
  **并集**，因此先写下的 await 处不会吞掉悬空处。`unawaited` 只对 async
  被调者成立：`log(x)` 同样是丢弃结果的语句，但那里本就没有可等待之物；
  `.then` 已经接手了 Promise，因此它跟随的调用并不悬空。

- **相对 specifier** 只对已解析的文件集合解析——不做文件系统猜测。
  `./x.js` 依次探测 `x.ts`、`x.tsx`……（ESM 写的是产物扩展名），再探
  `x/index.*`。
- **命名/默认/命名空间/别名导入**全部绑定；`ns.foo()` 经命名空间导入
  解析——解析器唯一确知接收者的成员调用。**重导出链**（`export { x }
  from './y'`、`export *`）穿过 barrel 文件被追踪，带环保护。
- **CommonJS 同样被读取**，而非只有 ESM——第一个纯 JS 语料有 524 处
  `require()`，图里却一条 import 也没有，于是：`require` 的所有形态
  （整模块、解构、`.member`、函数体内延迟）都是披着调用外衣的 import；
  `module.exports` / `exports.foo` 就是导出清单（对象字面量、别名、
  模块即函数，全部涵盖）。
- 类体内的 `this.m()` 解析到类自己的方法——词法可知，确定。
- **值上的成员调用**是 checker 的事：计数，绝不猜测。裸 specifier 指向
  包的表面（`zod.z`、`@babel/traverse`）；作用域包保留两段。
- TypeScript 跨文件的**声明合并**保留先见者，计数。

报告注记：未解析的成员调用 · 外部调用 · 指向本次未见文件的 specifier
（资源、样式）· 合并的声明。

## 测试代码

"谁调用了它"这个问题，测试代码与生产代码给出的答案分量不同；分不清两者的读者
会把一个测试算作一个使用者。两条属性说明此事：`test_flag` 记录**依据是什么**，
`_test_flag_confidence` 记录**该依据有多少分量**——下划线前缀使后者不进入紧凑
渲染，也不进入向量，那里它只会是噪声；`cypher`、`get_node` 与 `export` 仍会返
回它，而衡量此标记的读者正是在那里查看。源码中没有写下依据时，不作任何标记。

| `test_flag` | `_test_flag_confidence` | 依据 |
|---|---|---|
| `filename` | `strong` | `*.test.*`、`*.spec.*`，或位于 `__tests__/` 之下 |

是 `strong` 而非 `definitive`，因为 jest/vitest 的配置可以改写这些模式，而解析
器不读构建配置。`describe`/`it` 是另一个候选依据，但更弱：运行器把它们作为全局
量注入，因而只会以未解析调用的形式出现，而一个仅仅**定义**了名为 `it` 的函数的
模块看起来完全一样。

## 选项（`[plugins.ts]`）

| 键 | 效果 |
|---|---|
| `include_source = "true"` | 为声明附加 `_code` |

## 构建与测试

```console
$ cd parser && cargo test             # 43 个测试
$ just ts-plugin
$ drsg plugin install component/target/wasm32-wasip2/release/drsg_plugin_ts.wasm
```

## 已知局限

装饰器只被用来读取它所声明的路由，除此之外不作为事实记录——它的名字与参数不会进入
任何节点，这一点与 java 插件的注解不同。`tsconfig` 的路径别名（`@/…`）
是解析器不掌握的构建配置——计入未命中 specifier；参数非字面量的动态
`import()` 不可见。在运行时拼出的 base（`process.env.API_URL`）不是字面量，因此它
下面的路径仍是相对的，匹配不到任何东西；被交给另一个函数去挂载的 router 会丢掉
自己的前缀。
