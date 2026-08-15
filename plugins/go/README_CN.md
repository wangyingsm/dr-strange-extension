# 插件：go

[English](README.md) · 简体中文

将 Go 源码解析为图事实。Manifest 为 `go@1`，处理 `.go`。基于 **Go 自带的
`go/parser` 与 `go/ast`**——语言的正统前端——经 TinyGo 编译为组件。特意用
Go 编写：它比第二个 Rust 插件更能证明契约是语言中立的。

## 目录结构

```
parser/     语言逻辑，纯 Go，29 个原生测试（go test——与 TinyGo 无关）
component/  基于 sdk/go 的 TinyGo 封装，附 wit/ 构建包
```

## 键

Go 自己的全限定名，这正是键在子树与多次摄取间保持稳定的原因：

```
example.com/demo                      包（来自 go.mod 的 module 行）
example.com/demo/sub.Do               函数
example.com/demo/sub.Counter.Add      方法——path.Type.Method
```

模块路径来自文件之上**最近的 `go.mod`**（嵌套模块各自成体，如同 workspace
的 crate），经宿主读取；处处无 manifest 时以宿主的 label 命名整棵树。

## 节点

| 标签 | 产生于 | `doc_comment` / `visibility` / `file` / `line` 之外的属性 |
|---|---|---|
| `Package` | 每个包一个 | `name`、`imports`（包内各文件的并集，已排序）；`doc.go` 的文档并入。**无 file/line**——包横跨多个文件，任选其一都是武断 |
| `Function` / `Method` | 声明（`Method` 挂在接收者类型下） | `signature`（含接收者，照原样） |
| `Struct` | 类型声明 | `fields`：described 列表，`name: type`，声明顺序——Go 没有可见性关键字可前缀；首字母大小写*就是*可见性，而它已在名字里 |
| `Interface` | 接口声明 | 其要求的方法成为 `Method` 节点，经 `HAS_METHOD` 到达（无可见性：与接口同样公开） |
| `Type` / `TypeAlias` | `type X Y` / `type X = Y` | 底层类型在 `signature` |
| `Const` / `Var` | 值声明 | 类型在 `signature`，初始化式在 `value` 照原样；**iota 阶梯**重复上一条的表达式——语言自己的规则，记录，绝不求值 |
| 替身 | 外部包及其成员 | `Package` / `Function` + `External`，无属性 |

`visibility: "exported"` 遵循 Go 的规则：首字母大写。`init` 特意缺席——
一个包里的所有 `init` 共享一个名字，作为节点只能是键冲突，而其调用属于
接线，不是 API。

## 边

| 类型 | 含义 | `line` |
|---|---|---|
| `CONTAINS` | 包 → 声明、接收者类型 → 方法 | 声明处 |
| `HAS_METHOD` | 接口 → 其要求的方法 | 成员所在行 |
| `CALLS` | 函数 → 被调者 | 调用处 |
| `IMPORTS` | 包 → 包（树内或外部） | import 语句 |
| `IMPLEMENTS` | 类型 → 接口——**特意无行号**：Go 的实现关系是结构性的，哪里都没写下来 |

## 解析——确定性的界线

- **非限定调用**绑定到同包另一文件声明的函数（这正是 assemble 存在的
  理由）；`pkg.Type(x)` 被识别为转换而非调用；builtin 不产生任何边。
- **限定调用**经文件自身的 import 表绑定——尊重别名，树内真实的包名优先
  于目录名。
- **值上的方法调用**不点名任何包，接收者的类型正是解析器无从知晓的：
  计数。
- **接口实现**按结构决定，但服从确定性规则：包内按签名文本相等；跨包
  仅当双方签名全部由预声明类型拼写（本地的 `Thing` 在两个包里拼写相同、
  含义不同——文本不再是身份，比较即被拒绝）。接口嵌入了本树未声明的
  东西时整体不做匹配并计数——检查一半的实现关系不过是披着边外衣的猜测。
  接收者的指针性被忽略：边声称的是指针方法集。
- **接收者类型位于本次未见文件**（build-tag 变体、切分的子树）的方法，
  隐含出一个裸 `Type` 节点，而不是指向虚无的边。
- 同包两个文件出现同名 = **build-tag 变体**：先见者留，计数。

## 选项（`[plugins.go]`）

| 键 | 效果 |
|---|---|
| `include_source = "true"` | 为声明附加 `_code` |

## 构建与测试

```console
$ cd parser && go test ./...          # 29 个测试，纯 Go
$ just go-plugin                      # → component/go.wasm
$ drsg plugin install component/go.wasm
```

TinyGo 参数（`-scheduler=none -gc=leaking`）缺一不可——原因见
[`sdk/go`](../../sdk/go) 的 README 与 justfile 注释，那里也有每个
wasmexport 边界都遵守的"先复制再使用"规则。

## 已知局限

从嵌入结构体提升的方法不会挂到外层类型上；泛型实例化不做跟踪（调用绑定
到声明）；`init` 函数体按设计不计。
