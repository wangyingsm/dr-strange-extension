# plugin: xml

[English](README.md) · 简体中文

**按文件自己声明的方言**读取 XML——而不是给一个本就没有统一模式的格式套上一套模式。
清单 `xml@1`，声明 `.xml`。纯 Rust——`quick-xml`，与 `deps` 读 `pom.xml` 用的是同一个
读取器——因此这里没有 C 语法，构建也不需要 wasi-sdk。

第一个方言是 **MyBatis mapper**，Spring + MyBatis 后端的 SQL 就住在那里。此前图走到
Mapper 接口便止步：语句在它旁边的文件里，它们所碰到的表和列哪里都没有。「某个查询用了
哪些列」无从回答。

## 分派——文件自己说明它是什么

XML 是一种容器格式。这个 `.xml` 是 mapper，下一个是 logback 配置，二者除了尖括号别无
共同之处。所以本插件读取文档打开的**第一个起始元素**——文件对自身的声明——并据此分派：

| 根元素 | 读作 |
|---|---|
| `<mapper namespace="…">` | 一个 MyBatis mapper（见下） |
| 其他 | **散文**，与今天无人声明的文件的文本完全一样 |

声明一个扩展名不该悄悄把文件从图里拿掉，第二行就是为此存在的：本插件没有读取器的配置
文件，仍以其原本的文本离开，交给读散文的那一方。完全没有打开任何元素的文件被计数并记
名，绝不致命。`DOCTYPE` 被跳过而非抓取——此沙箱没有网络，而根元素本就比一个 URL 更有
力的证据。

日后新增的方言接在同一处；插件的其余部分无须移动。

## 键

mapper 以其**路径**为键，语句以其路径加 `id` 为键。携带文件路径的键不会与其他插件的键
相撞，而语句本就是另一个文件里、有自己行号的一处声明——这是借用 Java 名字的键说不出
来的。

```
resources/mapper/OrderMapper.xml              该 mapper
resources/mapper/OrderMapper.xml#selectById   其中一条语句
resources/mapper/OrderMapper.xml#now@mysql    按厂商各写一份中的一条
com.acme.OrderMapper.selectById               它实现的那个 Java 方法
orders                                        一张表，照 SQL 所写
orders.user_id                                它的一列
```

## 节点与边

| 标签 | 每个…一个 | 属性 |
|---|---|---|
| `Mapper` | mapper 文件 | `namespace`——其语句所实现的 Java 接口 |
| `Statement` | `<select>` / `<insert>` / `<update>` / `<delete>` | `kind`、`sql`、照原样的 `parameter_type` / `result_type` / `result_map` / `database_id`、`unbound_columns`、`file`、`line`、`end_line` |
| `Method` + `External` | 语句所指名的方法 | — |
| `Table` + `External` | SQL 指名的表 | — |
| `Column` + `External` | 绑定到某表的列 | — |

| 类型 | 含义 |
|---|---|
| `CONTAINS` | mapper → 语句 |
| `IMPLEMENTS` | 语句 → Java 方法，标记 `_resolved_by: mybatis-namespace` |
| `READS` / `WRITES` | 语句 → 表，按引入该表的关键字分成两种边，而非记在边的属性上——因为 Cypher 目前还读不到边属性（dr-strange ROADMAP §15） |
| `USES_COLUMN` | 语句 → 列 |
| `HAS_COLUMN` | 表 → 列 |

后三者上的 `External` 是**一项断言：该键指向本树之外的东西**——一个数据库模式，或另一
个插件解析过的方法。两个处理器指名同一个外部键是在彼此同意，而不是冲突；这正是下面那个
连接得以成立的原因，也会让日后读取模式本身的读取器（DDL 文件、迁移脚本）与这些键相遇，
而不是另造一份。

## 连接在哪里成立，在哪里不成立

mapper 的 `namespace` **就是** Java 接口的限定名，而语句的 `id` 就是方法名。于是
`namespace.id` 正是 java 插件早已铸出的那个键，语句以 `Method` + `External` 指名它：

- java 插件解析过该接口时，它的声明盖过这个替身并保留自己的标签——一个节点，两边都能
  走到；
- 未解析过时，这条边依然成立，并说明该端点在所读范围之外，而不是作为悬空引用被丢弃。

此后对那个 Java 方法用 `context`，就会看到 `IMPLEMENTS ←` 指向该语句，而语句之下便是
它碰到的表和列。

## 语句所持的 SQL

由语句的文本及其动态子元素拼装，除此之外**与所写完全一致**——`#{…}` 与 `${…}` 都在其
中。参数标记最终变成什么，是驱动的事。

- `<include refid="…">` 嵌入**同一文件内**它所指名的 `<sql>` 片段；只展开一层，因为
  片段成环绝不可以把一次摄取挂死。
- `<if>`、`<choose>`、`<when>`、`<otherwise>` 贡献它们的文本。
- `<where>`、`<set>`、`<trim prefix= suffix=>` 与 `<foreach open= close=>` 把 SQL 写在
  *属性*里，这部分也会被贡献进来：没有它，
  `UPDATE t <set><if>paid = 1,</if></set>` 拼出来会缺少 `SET` 一词，而 `paid` 会被读成
  表的别名，而不是它的一列。
- `<selectKey>` 不自成一条语句：它与其 insert 一同执行，故其文本并入那条语句的 SQL。
- `${…}` 被计数并报告——它粘贴的是文本，而所粘贴的可能就是表名本身。

## 表与列——一个容错扫描器，而非解析器

MyBatis 的语句是由带洞的分支拼起来的，因此一套文法若拒绝整条语句，拒绝的恰是读者最想读
的那一条。这里只扫描它确信的形状，并**把没能绑定的名字说出来**。

- **表**——`FROM`、`JOIN`、`USING`（MERGE 的来源）、`INSERT INTO`、`UPDATE`、
  `MERGE INTO`、`DELETE FROM` 之后的名字，以及方言允许省略介词时裸写的 `DELETE` /
  `INSERT` 之后的名字。逗号分隔的列表每一项都算；子查询的 `(` 不是表。
- **列**在引用由表名或别名限定时、或语句恰好只指名一张表时绑定到该表。其余的作为
  `unbound_columns` 挂在语句上，不出边：把列挂到错的表下，比把列说出来却不绑定更糟。
- 后面跟着 `(` 的名字是函数。`AS` 或 `::` 之后的名字是输出别名或类型，绝不是列。注释与
  字符串字面量什么也不指名。
- **`<resultMap>`** 会指名 SQL 可能不指名的列——对 `SELECT *` 而言它是唯一的答案——所以
  它的映射也会被收取，包含 `extends` 链，并覆盖 `<association>`、`<collection>` 与
  `<discriminator>`，它们的 `column` 驱动一次嵌套查询。

### 标识符，各方言的写法

名字**照原样**保留：Linux 上的 MySQL 默认对表名大小写敏感，折叠大小写会把数据库并不视
为同一张的两张表并到一起。

| 写法 | 读作 | 为什么 |
|---|---|---|
| `` `orders` `` | 名字 | 反引号是 MySQL 独有的 |
| `[dbo].[Orders]` | 名字 | SQL Server 与 Sybase 的写法；没有方言用它开启字符串 |
| `"orders"` | **看情况** | PostgreSQL 与标准 SQL 的标识符，MySQL 的字符串字面量 |
| `'orders'` | 字面量 | 在所有方言里都是字符串 |

点连起来的链是一个名字，上述写法混用亦然——`"sales"."regions"`、`` `db`.`t` ``、
`s."region_id"`——而成链者只可能是标识符，因为没有方言会把字符串点在一个名字上。

**孤立的** `"…"` 才是含糊的那一种，由语句自己来定：一个带引号的名字出现在只能站表的位
置，就证明了这条语句的引号是什么意思，它其余的带引号名字随之而定。没有这项证据时，该名
字挂在 `unbound_columns` 上，而不是凭一个字符串字面量造出一列。

## 已知局限

- **跨文件的 `<include refid="other.ns.frag">`** 被计数而不解析：它需要同时拿到所有
  mapper，而每次 `parse` 只给一个文件，故留作后续切片。
- **`<foreach>` 的 `separator`** 不在条目之间产出——条目本身不可知——`<selectKey>` 的
  `keyColumn` 也未收取。
- 只给**列**加引号而从不给表加引号的 PostgreSQL mapper 不给扫描器任何证据，那些列便只
  被指名而不被绑定。
- `${…}` 藏住了它所粘贴的东西，表名也可能在内。计数，绝不猜测。
- 任何方言词表都没有的词可能被读成列；那会是语句上的一个名字，而不是通往某表的一条边。
- PostgreSQL 的嵌套块注释止于第一个 `*/`，其 `$$…$$` 字符串不被识别。MySQL 中 `--` 后
  不跟空格的写法会被读作注释。
- `<bind>` 的 OGNL 值不被读取，其他 OGNL 亦然。
- **iBatis 2 的 `<sqlMap>`** 根元素不属于本方言；此类文件以散文离开。
- 注解式 mapper（接口方法上的 `@Select`）把 SQL 写在 Java 里，归 java 插件，不在此处。

## 目录条目

| 字段 | 值 | 为什么 |
|---|---|---|
| `claims` | `.xml` | 扩展名是路由的单位，方言在内部决定 |
| `min_drsg` | `2.0.0` | 契约下限——本插件不需要宿主更新的任何东西 |

## 构建与测试

```console
$ cargo test                          # 50 个测试——原生运行
$ just xml-plugin                     # 纯 Rust：无需 wasi-sdk
$ drsg plugin install target/wasm32-wasip2/release/drsg_plugin_xml.wasm
```
