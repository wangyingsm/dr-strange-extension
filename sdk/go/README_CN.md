# sdk/go — Go SDK

[English](README.md) · 简体中文

Go 插件作者需要的唯一模块：

```console
$ go get github.com/wangyingsm/dr-strange-extension/sdk/go
```

它在 `bindings/` 下携带 `wit-bindgen-go` 生成的 `drsg:preprocess` 契约绑定，
其上是 `ext` 包——你真正接触的东西：实现一个接口、调用一个函数，永远不用
面对 `cm.List`。

## API

```go
// 两阶段契约的 Go 形态。
type Plugin interface {
    Describe() Manifest
    Parse(subject Subject, options map[string]string) ([]byte, error)
    Assemble(partials [][]byte, options map[string]string) (Output, error)
}

func Register(p Plugin)              // 在 package main 的 init() 中调用

// 宿主——全部能力授权。
func List(suffix string) ([]string, error)  // 可读路径，已排序
func Read(path string) ([]byte, error)      // 以根目录为界；../ 按解析后路径拒绝
func Label() (string, bool)                 // 树的名字（如果有）
```

事实是普通结构体——`Node{Key, Label, ExtraLabels, Props}`、
`Edge{Src, Dst, Type, Props}`——`Props map[string]any` 会被 marshal 为契约
携带的 JSON 对象字符串。`Described(desc, value)` 构建数据库的自解释形式
`{"$desc": …, "$value": …}`。

`Parse` 看到一个分块，返回一份**不透明的部分结果**：你自己的 `Assemble`
想读回什么就序列化什么（官方 Go 插件用 `encoding/json` 序列化其逐文件
事实）。它可能在互不共享的实例中并发运行，因此只能依赖输入与宿主。
`Assemble` 只运行一次，按分块顺序收到全部部分结果；跨文件解析属于这里，
且结果不得依赖分块边界落在哪里。

## 唯一的规则：先复制，再使用

凡从 canonical ABI 提升（lift）而来的数据，**使用前必须复制**；返回的结果
也要先复制。`cm` 切片只是 ABI 缓冲区上的一个*视图*；真实解析过程中的内存
分配会诱使 TinyGo 的 GC 把它脚下的内存移走。这是踩过坑才知道的——一个 22
分块的 assemble 解码出 20 分块时从未出现过的乱码——现在 `ext` 包在每个边界
替你完成复制。如果你直接使用 `bindings/`，这条规则由你自己遵守。

## 构建

TinyGo ≥ 0.41，且 PATH 中有 `wasm-tools`（TinyGo 借它把模块提升为组件）：

```console
$ tinygo build -target=wasip2 -scheduler=none -gc=leaking \
    --wit-package ./wit --wit-world drsg:preprocess-build/plugin-go -o mine.wasm .
```

两个参数都缺一不可：

- `-scheduler=none`——否则调度器会在 wasmexport 返回与宿主读取结果之间
  运行，其 GC 可能回收正要返回的缓冲区；
- `-gc=leaking`——即使没有调度器，保守 GC 在 wasmexport 下仍会 trap。
  泄漏之所以可接受，*是因为宿主的设计*：每次调用都在全新的 store 中运行，
  泄漏的内存随调用一起消亡，store 的内存上限就是边界。

你的插件需要一个 `wit/` 构建包，把契约与 TinyGo 运行时启动所需的 WASI
导入组合在一起——整体复制
[`plugins/go/component/wit`](../../plugins/go/component/wit) 即可；其中的
`world.wit`（`drsg:preprocess-build/plugin-go`）只为让
`wasm-tools component new` 能解析运行时的导入而存在。

## 测试

把解析逻辑放在带自身测试的普通 Go 包里（`go test`，与 TinyGo 无关），让
`main` 包只做薄薄的 `Plugin` 适配——[`plugins/go`](../../plugins/go) 演示了
这个形态：解析器 29 个原生测试，组件约 90 行。

## 契约副本

这里的 `wit/preprocess.wit` 是仓库根部 canonical 契约的 vendored 副本；
`bindings/` 由它经 `wit-bindgen-go` 生成（`just go-bindings` 可重新生成）。
副本与 canonical 不一致时 CI 会失败。
