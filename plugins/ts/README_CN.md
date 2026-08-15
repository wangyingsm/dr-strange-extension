# 插件：ts

[English](README.md) · 简体中文

将 TypeScript **与** JavaScript 解析为图事实。Manifest 为 `ts@1`，处理
`.ts .tsx .mts .cts .js .jsx .mjs .cjs`——一个解析器覆盖整个生态，混合仓库
因此摄取为事实而不是一半沦为散文。基于 [swc](https://swc.rs) 的
`swc_ecma_parser`（Next.js 背后的解析器），只做解析：不做变换、没有
checker——checker 才能推断的东西，正是这里拒绝去猜的东西。

## 目录结构

```
parser/     drsg-ts-parser——语言逻辑，30 个原生测试
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

| 标签 | 产生于 | `doc_comment`（JSDoc）/ `visibility` / `file` / `line` 之外的属性 |
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

## 边

| 类型 | 含义 | `line` |
|---|---|---|
| `CONTAINS` | 包 → 模块 → 声明、类 → 成员 | 声明处 |
| `HAS_METHOD` | 接口 → 其方法节点 | 成员 |
| `CALLS` | 函数 → 被调者；`new Foo()` 记为对类的调用；渲染的 JSX 组件（`<Foo />`，大写开头）就是调用 | 调用处 |
| `IMPORTS` | 模块 → 模块（相对）或 → 外部包（裸 specifier） | import 语句 |
| `IMPLEMENTS` / `EXTENDS` | `class C implements I` / 类→类、接口→接口——TS 里是**句法**，因此确定，Go 的结构性检查做不到这一点 | 类/接口声明 |

## 解析——确定性的界线

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

## 选项（`[plugins.ts]`）

| 键 | 效果 |
|---|---|
| `include_source = "true"` | 为声明附加 `_code` |

## 构建与测试

```console
$ cd parser && cargo test             # 30 个测试
$ just ts-plugin
$ drsg plugin install component/target/wasm32-wasip2/release/drsg_plugin_ts.wasm
```

## 已知局限

装饰器在 v1 中跳过（源码中有注释言明）；`tsconfig` 的路径别名（`@/…`）
是解析器不掌握的构建配置——计入未命中 specifier；参数非字面量的动态
`import()` 不可见。
