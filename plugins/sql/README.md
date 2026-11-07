# plugin: sql

English · [简体中文](README_CN.md)

Reads **SQL migrations** into graph facts: the schema a tree declares it
keeps — tables, columns, views, user-defined types and indexes, with the
references between them — and then reads the **queries written in code**
against that schema. Manifest `sql@1`, claims **`.sql`**. Pure Rust
(`sqlparser`), so the sandbox needs no wasi-sdk and no C toolchain.

Every code plugin can see the name of a table in a string literal and nothing
more. What none of them reads is the DDL that says what that table *is* — its
columns, their types, which of them point at another table. That lived in a
migration directory no parser was claiming. With the DDL in hand, a statement
in a literal stops being a string and becomes a fact about those tables.

## A sequence, not a set

This is the one thing a SQL plugin has to get right, and it is why this one
implements the contract's two phases directly instead of taking the `Simple`
facade.

Migrations are a **sequence**. `0001_create_users.sql` adds a column that
`0014_trim.sql` removes. A parser that reads each file on its own and unions
the results reports a schema that never existed at any point in time, with
every column that was ever added and not one of the drops applied.

So `parse` does the expensive half per chunk and in parallel — lex, parse, and
reduce each statement to the change it makes — and `assemble` **replays** those
changes in migration order, emitting only the schema the last migration
leaves. The order is recomputed there from the paths, because the contract
requires a plugin's result not to depend on where the chunk boundaries fell.

Three consequences worth knowing:

- **Digit runs compare as numbers.** Lexicographically `10_x.sql` precedes
  `9_x.sql`, so a tree numbered without zero-padding would replay in the wrong
  order while looking perfectly sorted.
- **The reverse migration is not read**, or it would undo the tree that was
  just read. Four conventions are recognised: `*.down.sql` (golang-migrate,
  dbmate), `down.sql` (Diesel), Flyway's `U1__*.sql` undo scripts, and the
  in-file `-- +goose Down` / `-- +migrate Down` directives, where only the
  forward half of the file is taken.
- **A `DROP` of something no migration here created is counted, not applied.**
  It names an object outside this tree, so a stray teardown fixture cannot
  empty the schema. The report says which.

## Dialects

Declared, never sniffed — guessing a dialect from a file's contents is the kind
of guess this family refuses, and the three supported dialects disagree about
enough syntax that a wrong guess is a parse error attributed to the file.

```toml
[plugins.sql]
dialect = "postgres"   # or "mysql", "sqlite"
```

Left unset, the generic dialect accepts the broadest grammar and handles most
trees. A file it cannot read is counted and named, and the report carries the
hint that naming the dialect is what would change the outcome. MySQL's
backtick quoting and `AFTER <column>` placement are the usual reason to set it.

## Nodes and edges

| Node | Key | Example |
|---|---|---|
| `Table` | the qualified name as written | `users`, `public.users` |
| `Column` | its table's key and its own name | `users.email` |
| `View` | the qualified name as written | `active_authors` |
| `Type` | the qualified name as written | `user_role` |
| `Index` | the qualified name as written | `idx_users_email` |
| `Migration` | the file path | `migrations/0001_users.sql` |
| `Statement` | where the literal was written | `src/db.rs:114` |

| Edge | From → to | Means |
|---|---|---|
| `HAS_FIELD` | `Table`/`View`/`Type` → `Column` | a column, or a composite type's field |
| `IS_TYPE` | `Column` → `Type` | the column is written with a type this tree declares |
| `REFERENCES` | `Column` → `Column`/`Table` | a foreign key |
| `INDEXES` | `Index` → `Table`, `Index` → `Column` | what the index covers |
| `SELECTS_FROM` | `View` → `Table`/`View` | a relation the defining query reads |
| `READS` | `Statement` → `Table`/`View` | a relation a query in code reads |
| `WRITES` | `Statement` → `Table` | the target a query in code writes |
| `USES_COLUMN` | `Statement` → `Column` | a column a query names, with `role` on the edge |
| `DEFINES` | `Migration` → anything | which migration created it |

`READS`, `WRITES` and `USES_COLUMN` are the vocabulary the [`xml`](../xml)
plugin already writes for a MyBatis mapper, deliberately: a query in Rust and
a `<select>` in a mapper are the same kind of fact about the same table, and
one query should answer "who touches this" wherever the SQL was written.

## Queries written in code

A statement in a string literal is read against the schema the migrations
declare and becomes a `Statement` node keyed by where it was written. The
same grammar reads both, so a query is either understood or refused — never
half-guessed.

```
$ drsg context agents
agents  Table
primary_key: row_id
READS ← (2):
  crates/ip-storage/src/postgres/agent.rs:64   Statement  …/agent.rs:64-66
  crates/ip-storage/src/postgres/agent.rs:174  Statement  …/agent.rs:174-175
WRITES ← (13):
  crates/ip-storage/src/postgres/agent.rs:94   Statement  …/agent.rs:94
  …
```

Eight languages' literals are read: Rust (including `r#"…"#`), Go, Python,
TypeScript and JavaScript, Java, Kotlin, C# (including `@"…"`) and C.
Comments are skipped rather than merely ignored — in a language where `'`
quotes a string, the apostrophe in one comment opens a literal that closes at
the apostrophe in the next, swallowing everything between. Literals the
language joins are joined: `+` in Java, C#, Go, TypeScript and Python,
adjacency in C and Python.

**The edge to the enclosing function is not written.** That needs the
function's key, which only that language's own plugin can mint, and an edge
onto a key no node declares fails the *entire* write. What the graph holds
instead is the span: a `Statement` carries `file`, `line` and `end_line`, as
every definition node does, so which function encloses one is arithmetic over
facts already there — and `context` does that arithmetic, reporting it as
position rather than as an edge:

```
$ drsg context crates/ip-storage/src/postgres/identity.rs:114
kind: select
sql: SELECT m.standing FROM memberships m JOIN tenants t ON … JOIN users u ON …
READS → (3):
  tenants  Table
  users  Table
  memberships  Table
inside (by span) (1):
  ip_storage::postgres::identity::read_membership  Function  …/identity.rs:108-132
```

A host too old to join them still holds every fact the join reads, so the
answer is the reader's to assemble rather than absent.

**A hole is read as a bind parameter.** `${…}`, `#{…}`, `{…}`, `$name` and
`%s` stand where a value goes far more often than not, and a grammar shown
the hole refuses the whole statement — which is the query a reader most wants
read. A `?` in its place reads the text the way the driver will see it, and
the node says `parameterized`. Where the hole stood in an identifier position
instead, `FROM ?` is not a statement and the parse fails: the honest outcome,
since a table named at runtime is not a fact about this tree. Holes inside a
SQL string literal are left alone, so a `'{"a":1}'` is still the JSON it was.

**A refused query is retried against generic SQL.** The declared dialect is
about the migrations, where the wrong grammar changes the schema; a query is a
different risk, since a more permissive grammar can only add a statement whose
table names came from its own text. And a repository routinely holds both — a
Postgres primary and a SQLite cache, `$1` and `?` placeholders in neighbouring
modules. On one real tree, declaring `postgres` alone read 115 of 254
statements; retrying read 197. The node says `_resolved_by:
generic-grammar` where the fallback was the one that read it.

**A column edge carries the `role` it was named in** — `projection` for what
the caller gets back (a `SELECT` list or a `RETURNING`), `assignment` for an
`UPDATE … SET` or an `INSERT`'s column list, `reference` for everywhere else.
A column named in two roles keeps the first in that order.

**A statement about no table is not a node.** `SELECT 1`, or a `FROM
generate_series(1, 10)`: a node for one would carry a literal and not one
fact. A table-valued function sits exactly where a table does and is told
apart from one, so it mints nothing.

**A string that opens with one of SQL's own words is not yet a candidate.** A
user interface is full of `Delete` buttons and `Select a workspace`
placeholders; on one real tree the opening word alone sent 178 of them to the
parser, and a report naming 178 strings that were never queries explains
nothing. A statement always names its table with `FROM`, `INTO` or `SET`, and
that is the rest of the gate.

Source files are pulled through the host rather than routed here: the router
hands each file to exactly one handler and a `.rs` belongs to the rust
plugin's bucket, so asking is the only way — which is what the grant is for.
The scan is serial, in `assemble`, because the host hands this plugin one
`.sql` file per `parse` call and resolving a table name needs the replayed
schema. Set `inline = false` to read the migrations alone; a file over 1 MiB
is counted and named rather than lexed.

**The schema is in the key only when the DDL wrote one.** Supplying a default
— `public` for Postgres, the database for MySQL, `main` for SQLite — would
invent a key, and each dialect would invent a different one. An unqualified
name resolves against a qualified one when exactly one object could match;
two candidates is ambiguity, which is reported rather than guessed.

**Built-in types get no node.** A `Type` node for `text` would appear in every
schema, carry nothing, and bury the types a tree actually declares. The type
as written rides on the column as a property, and `IS_TYPE` points only at a
type some migration declares — matched past a modifier and an array marker, so
`user_role[]` and `varchar(40)` both resolve correctly.

**Foreign keys are column-to-column wherever the pair can be named.**
`REFERENCES workspaces (id)` names both ends. `REFERENCES workspaces` names
only the table, and SQL defines that as the referenced table's primary key —
which the replayed schema already holds, so it is resolved rather than
coarsened. Every edge says which case it was in `_resolved_by`:
`foreign-key`, `foreign-key-primary-key`, or `foreign-key-table` where a
composite key and the referencing columns do not line up.

**An edge is never written onto a key no node declares.** A reference to a
table from another database, or an index on a table a later migration dropped,
is skipped — in this graph a dangling edge endpoint fails the *entire* write,
not the one edge.

**An unnamed index is counted rather than keyed.** `CREATE INDEX ON users
(email)` leaves the name to the dialect, and inventing one would key a node on
something no migration wrote. Its columns are already on the table.

## What it does not do

**It writes no edge from a function to a statement** — see *Queries written
in code* above for why, and for the span the graph holds instead.

**It does not read a query assembled at runtime.** A statement built from
branches, or one naming its table through a hole, is counted and named rather
than guessed at. Nor does it follow a query builder: `users.filter(…)` in
Diesel or sqlx's macros declare the same thing in a language's own types, and
reading those is that language's plugin's work.

Also unread: stored procedures and triggers; row-level security policies;
`GRANT`; sequences as objects of their own; and PL/pgSQL anonymous blocks
(`DO $$ … $$`), which sqlparser does not parse — those files are counted and
named.

## Measured

Against real trees, with the dialect set. *Skipped* counts migration files
that would not parse; *refused* counts literals that opened like a query and
would not.

| Tree | Migrations | Skipped | Tables | Columns | Source read | Statements | Refused |
|---|---:|---:|---:|---:|---:|---:|---:|
| a Postgres service | 46 | 0 | 16 | 107 | 210 | 197 | 12 |
| another Postgres service | 22 | 1 | 23 | 202 | 87 | 130 | 3 |
| Yaak (sqlite) | 68 | 0 | 23 | 284 | 1024 | 20 | 2 |
| a MySQL service | 3 | 0 | 2 | 12 | 219 | 22 | 7 |

The one migration skip is a `DO $$ … $$` block. All 23 of Yaak's foreign keys
are written `REFERENCES <table>` with no column, and all 23 resolve to the
primary key. The first tree is the one where declaring `postgres` alone read
115 statements and the generic retry read 197: it holds a Postgres primary and
a SQLite cache side by side. A whole-tree digest of the largest of these,
including every other plugin, takes under three seconds.

## Build and test

```sh
just sql-plugin          # build for wasm32-wasip2
cd plugins/sql && cargo test
```

The tests run natively: everything goes through the real pipeline — lower,
replay, scan, emit — with only `host::list` and `host::read` left out, so
files are given as text.
