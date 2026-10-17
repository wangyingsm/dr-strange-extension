# 插件：k8s

[English](README.md) · 简体中文

把 **Kubernetes 清单**读成图中的事实：一棵树声明了哪些工作负载、它们运行哪些
镜像、哪些服务选中它们、哪些路由能到达这些服务，以及它们挂载了什么配置。清单
为 `k8s@1`，**不声明任何文件扩展名**，由显式 handler 路由——见[分发](#分发)。
纯 Rust：`yaml-rust2`、`rmp-serde` 与 SDK。

这是运维图的后一半；模型本身写在主仓库的 `arch/10-ops.md` 里，而
[compose 插件](../compose)读的是同一模型的小方言。Kubernetes 是真正的那一侧：
对象散落在多个文件中，一个工作负载的 Pod 是由**标签选择器**而不是键来指名的，
配置也要经过 Pod 的 volume 列表才能抵达，而不是被直接命名。

## 分发

本插件不声明扩展名，而且与 compose 读取器不同，它也无法按文件名路由——一份清单
可以叫任何名字。

声明 `.yaml` 这个方案被**否决**了：宿主把每个文件分到恰好一个读取器，且没有回退，
因此声明这个扩展名会把树里每一份 `values.yaml`、CI 工作流和 chart 都从文档读取器
那里抢走，并让它们彻底不被读取。一个插件既然声明了某种格式，就应当真的能读它。

所以清单树是被显式消化的：

```console
drsg digest ./k8s --handler k8s
helm template ./chart > rendered.yaml && drsg digest rendered.yaml --handler k8s
kustomize build ./overlays/prod > rendered.yaml && drsg digest rendered.yaml --handler k8s
```

## 两个阶段，因为答案跨越文件

多数插件实现 SDK 的 `Simple` 外观：按 chunk 运行一次，再把结果拼接起来。本插件
不能这样：一个 `Service` 和选中它的 `Deployment` 常常在不同文件里，而宿主完全
可以把它们放进不同的 chunk。按 chunk 累积什么都找不到，更糟的是，它会为每个
chunk 各铸一个命名空间节点。

因此它自己实现 `Guest`，并拥有自己的 partial 格式：`parse` 记下某个 chunk 能够
定论的东西，加上解析所需的事实；`assemble` 做全树那一遍——每个命名空间一个节点，
每个选择器都对全树持有的每个工作负载做匹配。

## 节点与边

| 节点 | 键 | 是什么 |
|---|---|---|
| `Namespace` | `<ns>` | `metadata.namespace`，否则是 `default`。无论多少对象指名它，都只有一个节点 |
| `Workload` | `<ns>/<kind>/<name>` | `Deployment`、`StatefulSet`、`DaemonSet`、`Job`、`CronJob`、`ReplicaSet`、`ReplicationController`、`Pod`。`kind` 记在节点上，另有 `replicas`、`schedule` 以及供选择器匹配的 `pod_labels` |
| `Container` | `<workload>/<name>` | 一个容器，带 `role`（`container` 或 `init`） |
| `Image` | 原样写下的引用 | 它运行的东西——也是下文的连接载体 |
| `Service` | `<ns>/Service/<name>` | 带它的 `type`、`ports` 以及它所要求的 `selector` |
| `Ingress` | `<ns>/Ingress/<name>` | 外部入口 |
| `ConfigMap` | `<ns>/ConfigMap/<name>` | 键**与值**——见下文 |
| `Secret` | `<ns>/Secret/<name>` | 只有键名与 `type`，绝不记录值 |
| `Volume` | `<ns>/Volume/<name>` | 一个 `PersistentVolumeClaim`，带 `storage` 与 `storage_class` |
| `Resource` | `<ns>/<kind>/<name>` | 其他任何 kind：被记录下来，让读者看到这里曾有东西，而不是假装理解它 |

| 边 | 从 → 到 | 携带 |
|---|---|---|
| `IN_NAMESPACE` | 任何节点 → `Namespace` | — |
| `RUNS` | `Workload` → `Container` | — |
| `USES_IMAGE` | `Container` → `Image` | — |
| `SELECTS` | `Service` → `Workload` | `matched_by` |
| `ROUTES_TO` | `Ingress` → `Service` | `host`、`path`、`port` |
| `MOUNTS` | `Container` → `ConfigMap` \| `Secret` \| `Volume` | `as`（`file`、`envFrom`、`env`）、`volume`、`path`、`key` |

`SELECTS` 是唯一一条背后有真正工作量的边。Service 用标签指名 Pod，而 Pod 来自
工作负载的 Pod 模板——所以这条边是 `spec.selector` 与
`spec.template.metadata.labels` 在同一命名空间内的匹配，且选择器里的**每一个**
标签都必须命中。在本树中什么都没匹配上的选择器不产生边；选择器留在节点上，报告
会说明有多少个未匹配，因为它指名的东西完全可能就在别处。

一条 `MOUNTS` 边要经过 Pod 的 `volumes` 列表来解析：容器的 `volumeMounts` 指的是
一个**本地**卷名，而这条边指向真正支撑它的东西——那个 ConfigMap、Secret 或
claim。`envFrom` 以及单个 `valueFrom.configMapKeyRef` 同样是挂载，由 `as` 说明
是哪一种。

CronJob 的 Pod 模板比其他所有工作负载都深一层
（`spec.jobTemplate.spec.template`），这正是那种会悄无声息什么都不记录的差异；
为它写了一个测试。

## 连接在哪里成立，在哪里不成立

回溯到构建某个东西的那次提交，线索是镜像的**标签**，由 SDK
（`dr_strange_ext::ops`）解析，从而让本插件与 compose 插件不可能产生分歧——同一个
引用是同一个键下的同一个节点，两个读取器写出不同的 `revision` 只会留下后写的那个。

形如修订号的标签因构造而成立（`sha-9f2c1b7` → 提交 `9f2c1b7`）；`latest`、
`stable` 和版本号连不到任何东西；digest 精确但对来历沉默；第三方镜像从不连接，
这是对的。完整的表在 [compose 插件的
README](../compose/README_CN.md#连接在哪里成立在哪里不成立) 和
`arch/10-ops.md` §5 里。

## 配置与密钥

**ConfigMap 的值会被记录。** 选择 `ConfigMap` 而不是 `Secret`，就是作者在声明这些
内容不敏感，而一次能看见它们的查询可以回答大多数配置问题。后果值得说清楚：这些值
会落进平面，因而也会落进它的每一次备份与导出；而现实仓库里的 ConfigMap 有时装着
本该放进 Secret 的东西。`binaryData` 只按键名记录。

**Secret 的值绝不被记录**，任何编码都不。节点只带键名与 `type`。base64 不是加密，
一个去读它的插件会把真实凭据放进一张会被四处复制的图里。

容器的 `env` 只按**名字**记录，理由与 compose 读取器相同：一个内联的 `value:`
既可能是日志级别，也同样可能是密码，而清单里没有任何东西说明是哪一种。

## 已知边界

- **声明态，而非运行态。** 没有活集群、没有真实存在的 Pod、没有节点分配、没有指标
  或告警。那些要随 Prometheus 与 OpenTelemetry 的工作到来；`arch/10-ops.md` §1
  划了这条线，§3 为它们预留了标签。
- **不做任何渲染。** Helm chart 或 Kustomize overlay 必须先渲染——`helm template`、
  `kustomize build`——再消化渲染后的 YAML。运行它们不是解析器的职责。
- **一个文档需要同时有 `apiVersion` 与 `kind`。** 其他一切都会被拒收并点名，因此
  被路由到这里的 `values.yaml` 不会被半读成一个看起来像拓扑的形状。
- **`SELECTS` 是标签匹配，不是 endpoint 真相。** 一个选择器匹配到两个工作负载的
  Service 会得到两条边，这正是集群会做的事。工作负载自身 `spec.selector` 下的
  `matchLabels` 不被使用：Service 真正匹配的是 Pod 模板上的标签。
- **不追踪跨命名空间的引用。** 一个指名其他命名空间中服务的 Ingress，指向的是它
  自己命名空间里的键，因为清单说的就是这个。

## 目录条目

```json
{
  "name": "k8s",
  "version": "1.1.0",
  "claims": "Kubernetes manifests",
  "url": "https://github.com/wangyingsm/dr-strange-extension/releases/download/k8s-v1.1.0/k8s.wasm",
  "contract": "1.0.0",
  "min_drsg": "2.7.0"
}
```

## 构建与测试

```console
cargo test                                      # 两个阶段，包括跨 chunk 的情况
cargo build --release --target wasm32-wasip2    # 或 `just k8s-plugin`
```
