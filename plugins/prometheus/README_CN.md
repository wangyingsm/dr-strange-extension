# 插件：prometheus

[English](README.md) · 简体中文

把 **Prometheus** 配置读成图中的事实：它抓取哪些 job、这些 job 指向哪些地址、
它加载了哪些告警规则，以及这些规则在看哪些指标。清单为 `prometheus@1`，
**不声明任何文件扩展名**——见[分发](#分发)。纯 Rust：`yaml-rust2` 与 SDK。

这是继 Compose 与 Kubernetes 之后的第三个运维读者——模型本身写在主仓库的
`arch/10-ops.md` 里——也是第一个其主体不是「在运行的东西」而是「在看着运行的
东西」的读者。一个抓取 job 写下一个地址；那个地址上已经有一个 `Service` 节点；
把两者连起来的那条边，就是本插件的意义所在。

## 分发

别处的路由都按扩展名，因为输入本身就是扩展名。而 Prometheus 配置是一个
**文件名**：声明 `.yml` 会把树里每一份 Kubernetes 清单、CI 工作流和 Helm chart
都从本应读它们的读者那里抢走。

所以本插件不声明任何扩展名，由**宿主**把 `prometheus.yml` 与 `prometheus.yaml`
路由给以 `prometheus` 之名安装的那个插件——与它把 Compose 文件交给 `compose`
是同一种表述。本插件不在场时，这些文件仍按从前当作散文读取。没有任何东西靠猜。

被路由到这里、却既没有 `scrape_configs`、也没有 `rule_files` 或 `groups` 的
文件会被**谢绝**，计入跳过数，并在注记中写明。

**规则文件是被跟随的，不是被猜出来的。** 规则文件没有固定名字，因而任何按文件名
的路由都认领不到它。配置自己在 `rule_files:` 下声明了它们，本插件就按这份声明
去解析那些 glob，在宿主暴露的那棵树里寻找。若某棵规则树旁边没有配置文件，可以
显式地把宿主指向它：`--handler prometheus`。

## 节点与边

| 节点 | 键 | 是什么 |
|---|---|---|
| `ScrapeJob` | `<config>/job/<name>` | 一条 `scrape_configs` 条目。写了的话还带上 `metrics_path`、`scheme`、各个间隔、`auth`、`discovery` 与 `relabelled` |
| `ScrapeTarget` | `<config>/job/<name>/target/<address>` | 一个被声明的地址，拆成 `host` 与 `port`；地址本身就是一个键时另记 `names`。它自成一个节点，是因为它同时承载了关联本身，以及——当没有任何服务应答时——「这里没有对上」这个事实 |
| `Alert` | `<config>/alert/<group>/<name>` | 一条告警规则：`expr` 按原样保留，另有写了的 `for`、`severity` 与 `summary` |
| `Metric` | `metric/<name>` | 规则所指名的指标。刻意采用全局键：一个指标名无论写在哪里含义相同，于是两份配置看同一个指标时会在它上面相遇。记录规则产出的指标带 `recorded: true` 与它的 `expr` |

| 边 | 从 → 到 | 携带 |
|---|---|---|
| `HAS_TARGET` | `ScrapeJob` → `ScrapeTarget` | — |
| `SCRAPES` | `ScrapeTarget` → `Service` \| `Workload` | 由**宿主**画出，不由本插件画：`matched_by`（取自目标的 `names` 时为 `dns`，按名字匹配时为 `name`）、`confidence` |
| `ALERTS_ON` | `Alert` → `Metric` \| `ScrapeJob` | — |
| `DERIVES_FROM` | `Metric` → `Metric` | 记录规则的指标，指向它由哪些指标算出 |
| `IN_NAMESPACE` | `ScrapeJob` → `Namespace` | 当 Kubernetes SD 配置写明了它只看哪些命名空间 |

`<config>` 前缀取文件所在的目录名，没有目录则取文件名主干——与 Compose 读者给
项目取名的规则相同，理由也相同：一个仓库里的两份配置描述的是两套部署，它们的
job 不能撞在一起。从配置跟随下去读到的规则文件继承**该配置的**前缀，于是一条
告警可以连到它所监视的那个 job。

## 关联在哪里成立，在哪里不成立

只有一种地址形式能指名唯一一个服务：Kubernetes 集群 DNS。
`api.prod.svc.cluster.local` 就是 `prod/Service/api`——Kubernetes 读者铸出的那个
键，是推导出来的而不是猜出来的。目标节点把它记在 `names` 上，而这条边由**宿主**
画出，带 `matched_by: dns` 与 `confidence: exact`。

这条边归宿主而不归本插件，理由是实际跑出来的：这棵树是否*声明*了那个服务，不是
一个只握着一个文件的解析器能知道的事；而一条指向无人铸出的键的边会让整次 digest
失败（`bulk edge endpoint … not found`）。所以读者只陈述地址指名了什么，由能看见
整个平面的宿主判断是否有东西应答。没有任何东西应答时，这个目标就是未关联。

而一个光秃秃的 `api:9090`——Compose 栈与短名形式的 Kubernetes 目标都这么写——
没有指明命名空间，因而也就没有键。**本插件对它不画任何边。** 宿主能看到平面里
所有运维节点，可以按名字去匹配；它匹配到的结果标记为 `matched_by: name`，置信度
更低，而当两个服务同名时它会列出候选而不是替你选一个。在这里猜一个键，会产出一
条与确定的那条毫无区别的边——而这恰是运维模型唯一不肯做的事。

IP 地址、外部域名，以及被 `relabel_configs` 改写过的目标，都出于同样的理由不做
关联。`ScrapeTarget` 节点仍然在那里，地址按原样保留，于是「没有对上」是看得见
的，而不是不存在的。

## 已知边界

- **指标名是词法读出来的，不是解析出来的。** 一个标识符被计入，当它不是函数调用、
  不是关键字、不是分组标签、不是时长，也不在标签匹配器或字符串里面。在查询时才
  拼出来的名字——`{__name__=~"http_.*"}`——不是它能看见的选择。
- **只有精确的 `job="…"` 匹配器才指名一个 job。** `job=~"api.*"` 是一个覆盖若干
  可能并不存在的 job 的模式，而模式不是名字。
- **被发现的目标不是目标。** `kubernetes_sd_configs` 及其同类在运行时才产出目标
  集合。这里把机制记在 job 的 `discovery` 上；一个没有声明任何静态目标的 job 会
  在注记里说明，而不是看起来什么都没抓。
- **不记录任何凭据**——密码、bearer token、凭据文件、TLS 私钥都不记。只记录一个
  job 用的是哪种认证，写作 `auth: basic`，因为那是关于拓扑的事实，而密钥不是。
- **只改了规则文件、没动配置文件**时，这次改动会在下一次完整 digest 时才被读到，
  而不是由增量 fold 读到：fold 只重新解析发生变化的文件，而那个文件不被任何插件
  认领。
- **`--code-only` 会让规则文件对本插件不可见。** 该参数把所有没有处理器认领的
  文件排除在宿主暴露的那棵树之外，而规则文件正是这一类——于是 glob 匹配不到任何
  文件，注记里会这样写明。同一棵树上实测：普通 digest 得到 16 条事实，加上
  `--code-only` 只有 10 条。
- **Alertmanager 自身的配置不在范围内。** 告警*去往*何处是路由，不是拓扑。
- **声明的状态，不是运行时状态。** 没有时间序列、没有正在触发的告警、没有抓取
  健康度。见 `arch/10-ops.md` §1。

## 目录条目

```json
{
  "name": "prometheus",
  "version": "1.0.0",
  "claims": "Prometheus configurations and rule files",
  "url": "https://github.com/wangyingsm/dr-strange-extension/releases/download/prometheus-v1.0.0/prometheus.wasm",
  "contract": "1.0.0",
  "min_drsg": "2.31.0"
}
```

`min_drsg` 是开始按名字路由 `prometheus.yml`、并把这些事实送进运维平面的那个
发行版。更老的宿主会把该文件留给从前读它的那一方。

## 构建与测试

```console
cargo test                                      # 模型，以及上面那些边界
cargo build --release --target wasm32-wasip2    # 或 `just prometheus-plugin`
```
