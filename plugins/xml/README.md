# plugin: xml

English · [简体中文](README_CN.md)

Reads XML **by the dialect a file declares**, not by one schema for a format
that has none. Manifest `xml@1`, claims `.xml`. Pure Rust — `quick-xml`, the
same reader `deps` uses for `pom.xml` — so there is no C grammar here and no
wasi-sdk in its build.

The first dialect is the **MyBatis mapper**, where the SQL of a Spring + MyBatis
backend lives. The graph used to reach the Mapper interface and stop: the
statements sat in a file beside it, and the tables and columns they touch were
nowhere. *Which columns does this query use* had no answer.

## Dispatch — the file says what it is

XML is a container format. One `.xml` is a mapper, the next is a logback
configuration, and they share nothing but their angle brackets. So this plugin
reads the **first start element** a document opens — the file's own statement
about itself — and dispatches on it:

| Root element | Read as |
|---|---|
| `<mapper namespace="…">` | a MyBatis mapper (below) |
| anything else | **prose**, exactly as an unclaimed file's text is today |

Claiming an extension must not quietly remove a file from the graph, which is
why the second row exists: a configuration this plugin has no reader for leaves
as the text it was, for whatever reads prose. A file that opens no element at
all is counted and named, never fatal. The `DOCTYPE` is skipped rather than
fetched — this sandbox has no network, and a root element is better evidence
than a URL.

A dialect added later attaches at that same point; nothing else about the
plugin has to move.

## Keys

A mapper is keyed by its **path**, and a statement by its path and `id`. A key
that carries its file cannot collide with another plugin's, and the statement
is a declaration in its own file with its own line — which a key borrowed from
Java could not say.

```
resources/mapper/OrderMapper.xml              the mapper
resources/mapper/OrderMapper.xml#selectById   one statement
resources/mapper/OrderMapper.xml#now@mysql    one of several written per vendor
com.acme.OrderMapper.selectById               the Java method it implements
orders                                        a table, as the SQL names it
orders.user_id                                a column of it
```

## Nodes and edges

| Label | One per | Properties |
|---|---|---|
| `Mapper` | mapper file | `namespace` — the Java interface its statements implement |
| `Statement` | `<select>` / `<insert>` / `<update>` / `<delete>` | `kind`, `sql`, `parameter_type` / `result_type` / `result_map` / `database_id` as written, `unbound_columns`, `file`, `line`, `end_line` |
| `Method` + `External` | the method a statement names | — |
| `Table` + `External` | a table the SQL names | — |
| `Column` + `External` | a column bound to a table | — |

| Type | Meaning |
|---|---|
| `CONTAINS` | mapper → statement |
| `IMPLEMENTS` | statement → the Java method, stamped `_resolved_by: mybatis-namespace` |
| `READS` / `WRITES` | statement → table, split by the keyword that introduced it rather than carried as an edge property, because Cypher cannot read edge properties yet (dr-strange ROADMAP §15) |
| `USES_COLUMN` | statement → column |
| `HAS_COLUMN` | table → column |

`External` on the last three is an **assertion that the key names something
outside the tree** — a database schema, or a method another plugin parsed. Two
handlers naming one foreign key are agreeing, not colliding, which is what
makes the join below work and what will let a reader of the schema itself (a
DDL file, a migration) meet these same keys rather than duplicate them.

## Where the join holds, and where it does not

A mapper's `namespace` **is** the Java interface's qualified name, and a
statement's `id` is the method's. So `namespace.id` is the key the java plugin
already minted, and the statement names it as `Method` + `External`:

- where the java plugin parsed that interface, its declaration outranks the
  stand-in and keeps its label — one node, reached from both sides;
- where it did not, the edge still stands and says the endpoint is outside what
  was read, instead of being dropped as a dangling reference.

`context` on the Java method then shows `IMPLEMENTS ←` the statement, and the
statement shows the tables and columns underneath it.

## The SQL a statement holds

Assembled from the statement's text and its dynamic children, and otherwise
**exactly as written** — `#{…}` and `${…}` included. What a parameter marker
turns into is the driver's business.

- `<include refid="…">` splices the `<sql>` fragment it names **in the same
  file**; one level deep, because a fragment cycle must not hang an ingest.
- `<if>`, `<choose>`, `<when>`, `<otherwise>` contribute their text.
- `<where>`, `<set>`, `<trim prefix= suffix=>` and `<foreach open= close=>`
  carry SQL in their *attributes*, and it is contributed too: without it
  `UPDATE t <set><if>paid = 1,</if></set>` assembles without the word `SET`,
  and `paid` reads as an alias of the table rather than a column of it.
- `<selectKey>` is not a statement of its own: it runs with its insert, so its
  text rides that statement's SQL.
- `${…}` is counted and reported — it pastes text, and what it pastes may be
  the table itself.

## Tables and columns — a scanner, not a parser

A MyBatis statement is assembled from branches with holes in it, so a grammar
that refuses the whole statement refuses exactly the one a reader most wants
read. This scans for the shapes it is sure of and **names what it could not
bind**.

- **Tables** — the name after `FROM`, `JOIN`, `USING` (MERGE's source),
  `INSERT INTO`, `UPDATE`, `MERGE INTO`, `DELETE FROM`, and after a bare
  `DELETE` or `INSERT` where the dialect allows the preposition to be dropped.
  A comma-separated list is each of them; a subquery's `(` is not a table.
- **Columns** bind to a table when the reference is qualified by its name or
  alias, or when the statement names exactly one table. Anything else rides the
  statement as `unbound_columns`, with no edge: a column under the wrong table
  is a worse answer than a column named and left alone.
- A name followed by `(` is a function. A name after `AS` or `::` is an output
  alias or a type, never a column. Comments and string literals name nothing.
- **`<resultMap>`** names columns the SQL may not — it is the only answer for
  `SELECT *` — so its mappings are harvested too, `extends` chain included, and
  that covers `<association>`, `<collection>` and `<discriminator>`, whose
  `column` drives a nested select.

### Identifiers, in every dialect's spelling

Names are kept **as written**: MySQL on Linux is case-sensitive for table names
by default, so folding case would merge two tables the database does not.

| Spelling | Read as | Why |
|---|---|---|
| `` `orders` `` | a name | a backtick is MySQL's and nothing else's |
| `[dbo].[Orders]` | a name | SQL Server's and Sybase's; no dialect opens a string with it |
| `"orders"` | **it depends** | PostgreSQL's and standard SQL's identifier, MySQL's string literal |
| `'orders'` | a literal | a string in every dialect there is |

A dotted chain is one name in any mix of those spellings — `"sales"."regions"`,
`` `db`.`t` ``, `s."region_id"` — and a chain can only be an identifier, since
no dialect dots a string onto a name.

A **lone** `"…"` is the ambiguous case, and the statement settles it: a quoted
name standing where only a table can stand proves what that statement's quotes
mean, and its other quoted names follow. Without that evidence the name rides
`unbound_columns` rather than becoming a column invented from a string literal.

## Known limits

- **Cross-file `<include refid="other.ns.frag">`** is counted, not resolved: it
  needs every mapper at once, which one-file-per-`parse` makes a later slice.
- **`<foreach>`'s `separator`** is not emitted between items — the items are not
  known — and `<selectKey>`'s `keyColumn` is not harvested.
- A PostgreSQL mapper that quotes **only columns** and never a table gives the
  scanner no evidence, so those columns are named rather than bound.
- `${…}` hides what it pastes, including a table name. Counted, never guessed.
- A word no dialect list holds may be read as a column; it will be a name on a
  statement, not an edge to a table.
- PostgreSQL's nested block comments end at the first `*/`, and its `$$…$$`
  strings are not recognised. MySQL's `--` without a trailing space is read as a
  comment.
- `<bind>`'s OGNL value is not read, and neither is anything else OGNL.
- **iBatis 2's `<sqlMap>`** root is not this dialect; such a file leaves as
  prose.
- Annotation-based mappers (`@Select` on the interface method) hold their SQL in
  Java, so they belong to the java plugin, not here.

## Catalog entry

| Field | Value | Why |
|---|---|---|
| `claims` | `.xml` | the extension is the routing unit; the dialect is decided inside |
| `min_drsg` | `2.0.0` | the contract floor — this plugin needs nothing newer of the host |

## Build & test

```console
$ cargo test                          # 50 tests — native
$ just xml-plugin                     # pure Rust: no wasi-sdk needed
$ drsg plugin install target/wasm32-wasip2/release/drsg_plugin_xml.wasm
```
