# 插件：compose

[English](README.md) · 简体中文

把 **Docker Compose** 文件读成图中的事实：一个项目声明了哪些服务、它们运行哪些
镜像，以及它们所连接的配置、卷与网络。清单为 `compose@1`，**不声明任何文件扩展
名**——见[分发](#分发)。纯 Rust：`yaml-rust2` 与 SDK。

这是**运维图**的前一半——模型本身写在主仓库的 `arch/10-ops.md` 里，本插件实现
其中的 Compose 方言。先做 Compose 而不是 Kubernetes，是因为它是小的那一侧：一个
文件、没有模板渲染，而且一个服务的身份就是它被写在哪个键下面。

## 分发

别处的路由都按扩展名，因为输入本身就是扩展名。而 Compose 文件是一个**文件名**：
声明 `.yml` 会把树里每一份 Kubernetes 清单、CI 工作流和 Helm chart 都从本应读它们
的读者那里抢走。

所以本插件不声明任何扩展名，由**宿主**把它认识的那些名字——`compose.yaml`、
`compose.yml`、`docker-compose.yaml`、`docker-compose.yml` 以及它们的
`.override.` 变体——路由给以 `compose` 之名安装的插件，这与它把构建清单发给
`deps` 是同一个声明。本插件缺席时，这些文件仍按散文读取，与此前完全一致。不做
任何猜测。

宿主路由到这里、但没有 `services:` 块的文件会被**拒收**，计入 skipped，并在
notes 中点名。一份恰好叫 `compose.yml` 的 Kubernetes 清单，不会被半解析成一个
看起来像拓扑的东西。

## 节点与边

| 节点 | 键 | 是什么 |
|---|---|---|
| `Namespace` | `<project>` | Compose 项目：它自己的 `name:`，否则是所在目录，否则是文件名主干——这与 `docker compose` 自身的做法一致。`name_source` 说明取自哪一种 |
| `Workload` | `<project>/service/<name>` | 一个服务。`kind: service`，另有写下来的 `ports`、`command`、`replicas` |
| `Container` | `<project>/service/<name>/<name>` | 该服务的容器。单独成节点，是为了让 `USES_IMAGE` 与 `MOUNTS` 在这里的含义，与将来面对一个带 sidecar 的 Kubernetes pod 模板时完全一致 |
| `Image` | 原样写下的引用 | 容器运行的东西——也是下文的连接载体 |
| `ConfigMap` | `<project>/configmap/<file>` | 一个 `env_file`，按路径。其内容不被读取 |
| `Volume` | `<project>/volume/<name>` | 一个命名卷 |
| `Network` | `<project>/network/<name>` | 声明或被引用的网络 |

| 边 | 从 → 到 | 携带 |
|---|---|---|
| `IN_NAMESPACE` | 任何节点 → `Namespace` | — |
| `RUNS` | `Workload` → `Container` | — |
| `USES_IMAGE` | `Container` → `Image` | — |
| `MOUNTS` | `Container` → `Volume` \| `ConfigMap` | `as`（`file`、`envFrom`）、文件挂载的 `path` |
| `DEPENDS_ON` | `Workload` → `Workload` | 长写法给出条件时带 `condition` |
| `ATTACHED_TO` | `Workload` → `Network` | — |

标量**原样保留**。`${TAG:-latest}` 就保持这个字符串，因为一个针对本解析器看不到
的环境求出来的标签，是关于任何东西的事实。

## 连接在哪里成立，在哪里不成立

从一个运行中的东西回溯到构建它的那次提交，线索是镜像的**标签**，因为构建流水线
通常把它构建的修订号放在那里。形如修订号的标签成分会成为 `revision` 属性，再由
一次查询把它连到历史平面（`arch/10-ops.md` §5——是一个属性加一次查询，绝不是一条
边，因为跨平面的边会被存储层拒绝）。

- **带修订号的标签因构造而成立。** `api:sha-9f2c1b7` 与提交 `9f2c1b7` 是同一个
  东西，因为是流水线把后者放进了前者。`git-`、`rev-`、`sha256-` 前缀以及裸十六
  进制串都按同样方式读取。
- **浮动标签连不到任何东西。** `latest`、`stable`、`v2.1.0` 不指名任何修订号。
  镜像仍被记录，只是没有 `revision`。全数字标签是版本号的可能远大于修订号，因此
  不被当作修订号。
- **digest 精确而沉默。** `api@sha256:…` 钉住了字节，却对来历一无所言；它被记作
  `digest`，而不是 revision。
- **第三方镜像从不连接，这是对的。** `postgres:17` 不是由本仓库构建的，不该有任何
  东西把它连到这里的某次提交。
- **`build:` 的服务根本没有引用。** 它被标记为 `source: build` 并带上
  `build_context`，因为一个还不存在的镜像无法被指名。

## 已知边界

- **环境变量的值不被记录，只记录名字。** Compose 的 `environment:` 块把 ConfigMap
  和 Secret 混在同一处，而且没有任何东西区分它们，`POSTGRES_PASSWORD` 常常就在
  里面。记录名字已经能回答"这个服务由什么配置"，同时不把凭据复制进一张会被导出
  和备份的图里。运维模型中**确实**记录值的地方——Kubernetes 的 ConfigMap——是因为
  作者选了 `ConfigMap` 而不是 `Secret`，从而声明了它不敏感。Compose 不提供这样的
  声明，所以本插件不去推断一个。
- **`env_file` 的内容不被读取。** 路径成为节点，里面有什么则不。理由同上。
- **绑定挂载不是实体。** `./local:/mnt/local` 是某台机器上的路径，所以它作为
  `host_mounts` 挂在容器上，而不是变成一个 `Volume` 节点。
- **不处理 `extends`、profiles 与插值。** 通过 `extends` 组合另一个文件的文件，只
  按它自身读取。profiles 被忽略，因此文件中每个服务都会被记录，无论某个 profile
  是否会启动它。
- **多文档 YAML 只读第一个文档。** Compose 不使用 `---` 分隔；用了的文件不是
  Compose 文件。
- **声明态，而非运行态。** 没有容器、没有健康状况、没有正在使用的端口——这些都不
  在文件里。见 `arch/10-ops.md` §1。

## 目录条目

```json
{
  "name": "compose",
  "version": "1.0.0",
  "claims": "Docker Compose files",
  "url": "https://github.com/wangyingsm/dr-strange-extension/releases/download/compose-v1.0.0/compose.wasm",
  "contract": "1.0.0",
  "min_drsg": "2.7.0"
}
```

## 构建与测试

```console
cargo test                                      # 模型，以及上面那些边界
cargo build --release --target wasm32-wasip2    # 或 `just compose-plugin`
```
