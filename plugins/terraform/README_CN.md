# 插件：terraform

[English](README.md) · 简体中文

把 **Terraform 配置**读成图中的事实：一棵树声明了哪些资源、用了哪些模块、哪些
变量在配置它们，以及——真正要紧的那部分——它们之间的依赖。清单为
`terraform@1`，声明 `.tf`（以及 `.tfvars`/`.tfstate`，为的是**拒收**它们）。
纯 Rust：`hcl-rs`、`rmp-serde` 与 SDK。

这是运维图的第三个读取器，也是另外两个之下的那一层：[compose](../compose) 与
[k8s](../k8s) 描述跑的是什么，Terraform 描述它跑在**什么之上**——集群、数据库、
负载均衡器、DNS 记录。

## 它的图在哪里：引用

Terraform 的依赖图并没有被写在任何地方。它是从引用中**推导**出来的：
`subnet_id = aws_subnet.main.id` 就是对 `aws_subnet.main` 的一次依赖，而
`terraform plan` 也正是这样算出来的。

所以这里的边来自遍历每一个表达式，寻找其根指名了配置所声明之物的 traversal——
一个资源类型、一个数据源、一个模块、一个变量。显式的 `depends_on` 也会被记录，
边上会说明是哪一种（`via: reference` 或 `via: depends_on`），但它是这个答案里
**不那么**有意思的一半。

指名了树中任何文件都未声明之物的引用会被**丢弃，而不是画出来**：那会指向一个
没人铸造过的节点。报告会说明有多少条，因为通常的原因是某个模块的内部实现，而
它们住在另一个目录里。

## 分发

`.tf` 属于 Terraform，不属于别人，所以与 k8s 读取器不同，本插件直接声明它的
扩展名，一棵树无需被告知就会被读取。

`.tfvars` 与 `.tfstate` 被声明，**为的是拒收它们**。宿主把每个文件交给恰好一个
读取器，且没有回退，因此声明扩展名是唯一能阻止内置文档读取器拿走它、并把它的
内容放进散文的办法——而对一个 `.tfvars` 来说，那意味着一份凭据进了图。不声明
它们曾是一个缺陷，是通过消化一棵真实的树、再去读报告发现的：*"no installed
plugin claims .tfvars — these files were read as plain text"*。

还剩一个口子，而且无法用同样的办法堵上：`terraform.tfstate.backup` 以
`.backup` 结尾，这个扩展名太通用，不能去声明。Terraform 自己推荐的
`.gitignore` 会排除 `*.tfstate*`，而把状态文件放进版本控制的树，有比这个插件
更大的问题。

## 节点与边

| 节点 | 键 | 是什么 |
|---|---|---|
| `Namespace` | 所在目录 | 根模块——`terraform apply` 运行的单位。Terraform 自己没有命名空间 |
| `Infra` | `<type>.<name>` | 一个被管理的资源，按 Terraform 自己的地址来键：`aws_instance.web`。类型作为属性，因为它有成千上万种 |
| `DataSource` | `data.<type>.<name>` | 被读取而不是被管理的东西 |
| `Module` | `module.<name>` | 一个 module 块，带它的 `source` 与 `version` |
| `Variable` | `var.<name>` | 一个输入。它的 `default` 由下文约束 |
| `Output` | `output.<name>` | 这个模块对外发布的东西 |
| `Provider` | `provider.<name>` | 一个已配置的 provider，带它的 `region` |

用 `Infra` 而不是 `Resource` 是刻意的：k8s 读取器已经把 `Resource` 用作它不建模
的 kind 的兜底，而一个标签同时表示这两件事，会让任何一侧的查询都出错。

| 边 | 从 → 到 | 携带 |
|---|---|---|
| `IN_NAMESPACE` | 任何节点 → `Namespace` | — |
| `DEPENDS_ON` | `Infra` → `Infra` \| `DataSource` | `via`（`reference`、`depends_on`） |
| `USES_VARIABLE` | `Infra` \| `Module` → `Variable` | `via` |
| `USES_MODULE` | `Namespace` → `Module` | — |
| `PROVIDED_BY` | `Infra` → `Provider` | — |
| `EXPOSES` | `Output` → `Infra` | — |
| `USES_IMAGE` | `Infra` → `Image` | — |

## 这个连接，在这里收益翻倍

在 Terraform 中指名的容器镜像——一个 ECS task definition、一个 Cloud Run
service、一个 `docker_container`——铸造出的是 k8s 与 compose 读取器**铸造的同一个
`Image` 节点**，同一个键，由同一份 SDK 代码（`dr_strange_ext::ops`）解析出同一个
`revision`。所以带修订号的镜像从这里也能抵达一次提交，正如它从 Pod 模板出发一样；
一棵通过 Terraform 部署的树不是盲区。

`image`、`image_uri` 与 `container_image` 都会被读取，最深到四层嵌套块，因为不同
provider 把它放在不同深度。一个**被插值的**镜像
（`image = "${var.registry}/api:${var.tag}"`）不是一个镜像引用，而是一组引用，
因此被记作依赖。

## sensitive，以作者声明的为准

被标记 `sensitive = true` 的 `variable` 或 `output`，其值会被**扣下**；其他一切都
会被记录。这与 k8s 读取器对 ConfigMap 与 Secret 的规则相同，理由也相同：作者说了
它是哪一种，所以不需要去猜。被扣下的默认值读作 `(sensitive)`，并带一句说明为什么，
而不是悄无声息地缺失。

## 已知边界

- **不做任何求值。** `count`、`for_each`、条件表达式与函数都按原样记录，不被解析。
  无论 `count = 1` 还是 `count = 50`，`aws_instance.web` 都是一个节点，因为配置
  声明的就是一个资源块。
- **模板字符串内部的插值不被追踪。** `"${aws_vpc.main.id}/24"` 是一个模板，`hcl`
  把它作为字符串交出来；其中的引用因此会被漏掉。这被写成边界，而不是用一个针对
  文本的正则去猜。
- **不从 source 读取模块的内容。** 一个 `module` 块是一个带 source 与 version 的
  节点；这个模块声明了什么，在另一个目录或某个 registry 里，而这里都不去取。
- **`locals` 不是节点。** local 是某个表达式的名字，经由它的引用会被丢弃，而不是
  追到它所指的东西。
- **一个目录一个根模块。** 目录就是命名空间，这正是 `terraform apply` 对待它的
  方式；有 `infra/prod` 与 `infra/staging` 的树会得到两个。
- **声明态，而非现实。** 配置说它会建出什么，而不是账号里实际存在什么。
  `arch/10-ops.md` §1 划了这条线。

## 目录条目

```json
{
  "name": "terraform",
  "version": "1.0.0",
  "claims": "Terraform configuration",
  "url": "https://github.com/wangyingsm/dr-strange-extension/releases/download/terraform-v1.0.0/terraform.wasm",
  "contract": "1.0.0",
  "min_drsg": "2.7.0"
}
```

## 构建与测试

```console
cargo test                                      # 两个阶段，包括跨 chunk 的情况
cargo build --release --target wasm32-wasip2    # 或 `just terraform-plugin`
```
