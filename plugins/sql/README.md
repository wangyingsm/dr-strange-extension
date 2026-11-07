# plugin: sql

English · [简体中文](README_CN.md)

Reads **SQL migrations** into graph facts: the schema a tree declares it
keeps — tables, columns, views, user-defined types and indexes, with the
references between them. Manifest `sql@1`, claims **`.sql`**. Pure Rust
(`sqlparser`), so the sandbox needs no wasi-sdk and no C toolchain.

Every code plugin can see the name of a table in a string literal and nothing
more. What none of them reads is the DDL that says what that table *is* — its
columns, their types, which of them point at another table. That lived in a
migration directory no parser was claiming.

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

| Edge | From → to | Means |
|---|---|---|
| `HAS_FIELD` | `Table`/`View`/`Type` → `Column` | a column, or a composite type's field |
| `IS_TYPE` | `Column` → `Type` | the column is written with a type this tree declares |
| `REFERENCES` | `Column` → `Column`/`Table` | a foreign key |
| `INDEXES` | `Index` → `Table`, `Index` → `Column` | what the index covers |
| `SELECTS_FROM` | `View` → `Table`/`View` | a relation the defining query reads |
| `DEFINES` | `Migration` → anything | which migration created it |

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

**It does not read SQL embedded in code**, and so writes no edge from a
function to a table. That needs the enclosing function's key, which only that
language's own plugin knows, and an edge onto a key no node declares fails the
whole write rather than the one edge. The honest route is the one `wire`
already takes for HTTP routes — facts on both sides, matched by a verb at
query time — and it is separate work.

Also unread: stored procedures and triggers; row-level security policies;
`GRANT`; sequences as objects of their own; and PL/pgSQL anonymous blocks
(`DO $$ … $$`), which sqlparser does not parse — those files are counted and
named.

## Measured

Against real migration trees, with the dialect set:

| Tree | Files | Skipped | Tables | Columns | Types | Indexes |
|---|---:|---:|---:|---:|---:|---:|
| Yaak (`yaak-models`, sqlite) | 71 | 0 | 21 | 284 | 0 | 3 |
| a Postgres service | 44 | 0 | 15 | 107 | 0 | 20 |
| another Postgres service | 20 | 1 | 23 | 202 | 8 | 20 |

The one skip is a `DO $$ … $$` block. All 23 of Yaak's foreign keys are
written `REFERENCES <table>` with no column, and all 23 resolve to the
primary key.

## Build and test

```sh
just sql-plugin          # build for wasm32-wasip2
cd plugins/sql && cargo test
```

The tests run natively: everything goes through the real pipeline — lower,
replay, emit — with only `host::read` left out, so files are given as text.
