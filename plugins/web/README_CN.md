# 插件：web

[English](README.md) · 简体中文

将 HTML **与** CSS 解析为图事实——一个插件同时处理两者，manifest 为
`web@1`，处理 `.html .htm .css`。合为一个插件是刻意的：前端 plane 想要的
跨文件事实——*哪个样式表的 `.btn` 装饰哪个页面*——只能在同一个 assemble
同时看到两侧时解析。基于 tree-sitter 的 html、css **与 javascript** 语法
（第三个用于内联脚本），经 wasi-sdk 工具链构建。

克制即设计：**页面不等于它的每一个 `<div>`。** 默认的事实是页面本身、带
id 的元素、内联脚本函数，以及样式表声明的词汇表。整棵 DOM 可以选择性开启
（见下）。

## 目录结构

```
parser/     drsg-web-parser——逻辑，16 个原生测试
component/  drsg-plugin-web——Guest 实现 + rmp-serde 部分结果（需要 wasi-sdk）
```

## 键——凡平台自有地址处，用平台的

```
index.html                            Page
css/site.css                          Stylesheet
index.html#map                        带 id 的元素——URL fragment 正是平台
                                      自己寻址元素的语法
css/site.css::.btn                    类（保留选择器写法——::.btn 永远不会
                                      被误读为 CSS 伪元素）
css/site.css::--brand                 自定义属性
index.html::initMap                   内联脚本函数（文件作用域，家族的
                                      {file}::{name} 形式）
index.html::.local                    页面自己 <style> 里的类
```

## 节点

| 标签 | 产生于 | `file` / `line` 之外的属性 |
|---|---|---|
| `Page` | 每个 html 文件 | `title` |
| `Stylesheet` | 每个 css 文件 | `rule_count`；`classes` 与 `custom_properties` 为逗号连接的**键清单**——仪表盘可展开并逐项跳转。压缩文件跳过清单 |
| `Element` | 带 id 的元素——**按 DOM 的方式嵌套**，各挂在最近的节点祖先之下 | `tag` |
| `Class` | 样式表（或页面 `<style>`）定义的每个类 | `rules`：described 列表，该类出现的每条规则，**照原样、按源码顺序**（压缩文件跳过——单行巨块是噪音，不是读物） |
| `Const` | 每个 `--自定义属性` | `value` 照原样 |
| `Function` | 内联 `<script>` 函数（声明与 `const f = (…) =>`），用 JS 语法**浅解析**，深度与 C 插件相当——内联脚本天然以文件为作用域；模块世界的 JS 属于 ts 插件 | `signature`；include_source 时有 `_code` |
| 替身 | CDN 脚本/样式表——URL 即身份 | `File` + `External` |

## 边

| 类型 | 含义 | `line` |
|---|---|---|
| `CONTAINS` | 页面 → 元素 → 嵌套元素；样式表 → 类/属性；页面 → 内联函数 | 声明处 |
| `IMPORTS` | 页面 → 样式表（`<link rel=stylesheet>`）/ 脚本文件（`<script src>`）；样式表 → `@import` 目标；CDN URL → 外部 File | 标签 / `@import` 处 |
| `LINKS` | 页面 → 页面（站内 `<a href>`）；`#fragment` 在本次解析过该 id 元素时落到元素本身 | 锚点处 |
| `STYLED_BY` | 页面或元素 → 装饰它的类（来自 `class="…"`） | 属性处 |
| `USES` | 样式表 → 自定义属性（`var(--x)`） | 使用处 |
| `CALLS` | 内联函数 → 内联函数，页面之内——内联脚本的世界就是它的页面 | 调用处 |

## 解析——先就近，再唯一

- **类与自定义属性**就近优先绑定：页面自己的 `<style>` 先于样式表——
  与层叠让页面局部规则显得"局部"的方式一致——然后是**唯一定义**；两个
  样式表都定义的类计数，绝不猜测。
- 附带首个真实语料逼出来的一条特例：**`.min.css` 是其可读同胞的构建产物，
  不是第二种意见**——压缩定义让位于源码定义，仅当没有任何源码定义该名时
  才在压缩定义之间求唯一。（在 sb-admin-2 上，仅此一条规则就把
  `STYLED_BY` 从零——每个类都被其 min 孪生翻倍——带到 1,771 条边。）
- **href 与 import** 对已解析集合解析（相对路径、`../`、根起始绝对路径、
  `#fragment` 尾巴；query 剥除）；外部 URL 计为通向世界之外的链接，CDN
  import 成为外部 File 节点；指向本次未见文件（资源）的引用被计数。
- 内联脚本调用在页面自己的函数中绑定；`fetch()` 之流属于平台——计数。

报告注记：未解析的类/属性/调用引用 · 外部链接 · 指向未见文件的引用。

## 选项（`[plugins.web]`）

| 键 | 效果 |
|---|---|
| `include_source = "true"` | 为内联函数与自定义属性附加 `_code` |
| `dom = "full"` | **每个**元素都成为节点，按位置入键（`p.html::html[1]>body[1]>div[2]`），逐层嵌套；带 id 的元素保持其稳定的 `page#id` 形式。设为可选，因为位置键在快照内确定，标记变动时会漂移 |

## 构建与测试

```console
$ cd parser && cargo test             # 16 个测试
$ just web-plugin                     # 需要 wasi-sdk
$ drsg plugin install component/target/wasm32-wasip2/release/drsg_plugin_web.wasm
```

## 已知局限

SCSS/LESS 编译*成* CSS 且各有语法——v1 特意不处理；框架模板（Vue/Svelte
SFC）不是 html；类名同一性之外的选择器匹配（后代组合、特异性）是浏览器的
事，不是解析器的。
