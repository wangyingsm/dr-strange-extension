//! The plugin's own tests, running natively.
//!
//! Everything goes through the real pipeline — lower, replay, emit — with only
//! `host::read` left out, so the files are given as text. The replay is what
//! most of these assert: a fact that is right per-file and wrong for the
//! sequence is the failure this plugin exists to avoid.

use super::*;
use dr_strange_ext::Value;

/// Run the whole pipeline over files given as `(path, sql)`, in the order a
/// chunk would hand them over — which is deliberately *not* migration order,
/// so every test also exercises the sort in `replay`.
fn digest(files: &[(&str, &str)]) -> Output {
    digest_with(files, &[])
}

fn digest_with(files: &[(&str, &str)], options: &[(String, String)]) -> Output {
    let dialect = dialect_from(options);
    let mut chunk = Partial::default();
    for (path, sql) in files {
        if !migrate::is_up(path) {
            continue;
        }
        let file = read(path, sql, dialect.as_ref(), &mut chunk);
        chunk.files.push(file);
    }
    let mut out = output();
    out.report.skipped = chunk.skipped;
    out.report.notes = chunk.notes;
    let schema = schema::replay(chunk.files);
    emit::emit(&schema, &mut out);
    out.finish()
}

/// Node keys of one label.
fn keys(o: &Output, label: &str) -> Vec<String> {
    let mut found: Vec<String> = o
        .nodes
        .iter()
        .filter(|n| n.label == label)
        .map(|n| n.key.clone())
        .collect();
    found.sort();
    found
}

/// Edges of one type, as `(src, dst)`.
fn edges(o: &Output, ty: &str) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = o
        .edges
        .iter()
        .filter(|e| e.type_ == ty)
        .map(|e| (e.src.clone(), e.dst.clone()))
        .collect();
    found.sort();
    found
}

/// A node property's value — the `$value` of a described one, or the bare
/// value of a plain one like `file` or `line`.
fn prop(o: &Output, key: &str, name: &str) -> Option<Value> {
    let n = o.nodes.iter().find(|n| n.key == key)?;
    let props: Value = n.properties.parse().ok()?;
    let held = props.get(name)?;
    Some(held.get("$value").unwrap_or(held).clone())
}

const USERS: &str = "
CREATE TABLE users (
    id        bigserial PRIMARY KEY,
    email     text NOT NULL UNIQUE,
    nickname  varchar(40)
);
";

// ---- keys and shape ------------------------------------------------------

/// A column's key is the table's key and its own name, which is what someone
/// reading the migration calls it.
#[test]
fn a_column_is_keyed_under_its_table() {
    let out = digest(&[("migrations/0001_users.sql", USERS)]);
    assert_eq!(keys(&out, "Table"), ["users"]);
    assert_eq!(
        keys(&out, "Column"),
        ["users.email", "users.id", "users.nickname"]
    );
    assert_eq!(
        edges(&out, "HAS_FIELD"),
        [
            ("users".into(), "users.email".into()),
            ("users".into(), "users.id".into()),
            ("users".into(), "users.nickname".into()),
        ]
    );
}

/// The schema is part of the key only when the DDL wrote one. Defaulting it
/// to `public` would invent a key, and differently per dialect.
#[test]
fn a_schema_is_in_the_key_only_when_written() {
    let out = digest(&[(
        "m/0001.sql",
        "CREATE TABLE public.users (id int); CREATE TABLE sessions (id int);",
    )]);
    assert_eq!(keys(&out, "Table"), ["public.users", "sessions"]);
    assert_eq!(
        prop(&out, "public.users", "schema").and_then(|v| v.as_str().map(str::to_string)),
        Some("public".into())
    );
    assert!(prop(&out, "sessions", "schema").is_none());
}

/// Column facts a reader would ask for ride as properties.
#[test]
fn a_column_carries_its_type_and_nullability() {
    let out = digest(&[("m/0001.sql", USERS)]);
    assert_eq!(
        prop(&out, "users.nickname", "type").and_then(|v| v.as_str().map(str::to_string)),
        Some("VARCHAR(40)".into())
    );
    assert_eq!(prop(&out, "users.email", "nullable"), Some(false.into()));
    assert_eq!(prop(&out, "users.nickname", "nullable"), Some(true.into()));
    // A primary key is not nullable whether or not the DDL says NOT NULL.
    assert_eq!(prop(&out, "users.id", "nullable"), Some(false.into()));
    assert_eq!(
        prop(&out, "users", "primary_key"),
        Some(Value::Array(vec!["id".into()]))
    );
}

// ---- the replay ----------------------------------------------------------

/// The reason this plugin is two-phase. A column added by one migration and
/// dropped by a later one is in neither the final schema nor the graph —
/// unioning the files would report it.
#[test]
fn a_dropped_column_is_not_in_the_final_schema() {
    let out = digest(&[
        (
            "m/0003_drop_nickname.sql",
            "ALTER TABLE users DROP COLUMN nickname;",
        ),
        ("m/0001_users.sql", USERS),
        (
            "m/0002_add_locale.sql",
            "ALTER TABLE users ADD COLUMN locale text NOT NULL DEFAULT 'en';",
        ),
    ]);
    assert_eq!(
        keys(&out, "Column"),
        ["users.email", "users.id", "users.locale"],
        "nickname was dropped; locale was added"
    );
    assert_eq!(
        prop(&out, "users.locale", "default").and_then(|v| v.as_str().map(str::to_string)),
        Some("'en'".into())
    );
}

/// A dropped table takes its columns and its indexes with it.
#[test]
fn a_dropped_table_leaves_nothing_behind() {
    let out = digest(&[
        ("m/0001.sql", USERS),
        ("m/0002.sql", "CREATE INDEX users_email ON users (email);"),
        ("m/0003.sql", "DROP TABLE users;"),
    ]);
    assert!(
        out.nodes.iter().all(|n| n.label != "Table"),
        "{:?}",
        keys(&out, "Table")
    );
    assert_eq!(keys(&out, "Column"), [] as [String; 0]);
    assert_eq!(keys(&out, "Index"), [] as [String; 0]);
}

/// Digit runs compare as numbers. Lexicographically `0010` follows `009`, but
/// `10` precedes `9` — and a tree numbered without zero-padding would then
/// replay its migrations in the wrong order while looking sorted.
#[test]
fn migrations_are_ordered_numerically_not_lexicographically() {
    let out = digest(&[
        ("m/10_drop.sql", "ALTER TABLE users DROP COLUMN email;"),
        ("m/9_users.sql", USERS),
    ]);
    assert_eq!(
        keys(&out, "Column"),
        ["users.id", "users.nickname"],
        "9 must run before 10, so the drop applies"
    );
    assert_eq!(
        migrate::migration_order("m/9.sql", "m/10.sql"),
        std::cmp::Ordering::Less
    );
}

/// A rename carries the table's identity, and every foreign key pointing at
/// the old name follows it rather than dangling.
#[test]
fn a_renamed_table_takes_its_references_with_it() {
    let out = digest(&[
        ("m/0001.sql", USERS),
        (
            "m/0002.sql",
            "CREATE TABLE posts (id int PRIMARY KEY, author bigint REFERENCES users(id));",
        ),
        ("m/0003.sql", "ALTER TABLE users RENAME TO accounts;"),
    ]);
    assert_eq!(keys(&out, "Table"), ["accounts", "posts"]);
    assert_eq!(
        edges(&out, "REFERENCES"),
        [("posts.author".into(), "accounts.id".into())]
    );
}

/// A `DROP` of something no migration here created refers to an object
/// outside this tree. Applying it would let a stray teardown script empty the
/// schema, so it is counted and named instead.
#[test]
fn dropping_an_unknown_table_is_reported_not_applied() {
    let out = digest(&[
        ("m/0001.sql", USERS),
        ("fixtures/teardown.sql", "DROP TABLE legacy_sessions;"),
    ]);
    assert_eq!(keys(&out, "Table"), ["users"]);
    assert!(
        out.report
            .notes
            .iter()
            .any(|n| n.contains("legacy_sessions") && n.contains("not applied")),
        "{:?}",
        out.report.notes
    );
}

/// `CREATE TABLE IF NOT EXISTS` run twice, or a fixture restating the schema:
/// the first definition stands rather than the column list doubling.
#[test]
fn a_table_defined_twice_keeps_one_definition() {
    let out = digest(&[("m/0001.sql", USERS), ("m/0002.sql", USERS)]);
    assert_eq!(keys(&out, "Table"), ["users"]);
    assert_eq!(keys(&out, "Column").len(), 3);
}

/// The contract requires the result not to depend on where the chunk
/// boundaries fell, so `replay` sorts rather than trusting the order it got.
#[test]
fn the_result_does_not_depend_on_chunk_order() {
    let files = [
        ("m/0001_users.sql", USERS),
        (
            "m/0002_locale.sql",
            "ALTER TABLE users ADD COLUMN locale text;",
        ),
        ("m/0003_drop.sql", "ALTER TABLE users DROP COLUMN nickname;"),
    ];
    let forwards = digest(&files);
    let mut backwards_files = files;
    backwards_files.reverse();
    let backwards = digest(&backwards_files);
    assert_eq!(keys(&forwards, "Column"), keys(&backwards, "Column"));
    assert_eq!(
        edges(&forwards, "HAS_FIELD"),
        edges(&backwards, "HAS_FIELD")
    );
}

// ---- which files run forwards -------------------------------------------

/// Four conventions put the reverse migration in a file of its own. Reading
/// one undoes the tree that was just read.
#[test]
fn reverse_migration_files_are_not_read() {
    for down in [
        "migrations/0002_users.down.sql",
        "migrations/0002/down.sql",
        "db/U2__drop_users.sql",
    ] {
        assert!(!migrate::is_up(down), "{down} runs backwards");
    }
    for up in [
        "migrations/0002_users.up.sql",
        "migrations/0002/up.sql",
        "db/V2__add_users.sql",
        "db/users.sql",
        "db/update_counts.sql",
    ] {
        assert!(migrate::is_up(up), "{up} runs forwards");
    }

    let out = digest(&[
        ("m/0001_users.up.sql", USERS),
        ("m/0001_users.down.sql", "DROP TABLE users;"),
    ]);
    assert_eq!(keys(&out, "Table"), ["users"]);
}

/// goose and sql-migrate keep both directions in one file, split by a comment
/// directive. Only the forward half is the schema.
#[test]
fn an_in_file_down_section_is_not_read() {
    let both = "
-- +goose Up
CREATE TABLE users (id int PRIMARY KEY, email text);
-- +goose Down
DROP TABLE users;
";
    let out = digest(&[("m/0001_users.sql", both)]);
    assert_eq!(keys(&out, "Table"), ["users"]);
    assert_eq!(keys(&out, "Column"), ["users.email", "users.id"]);

    let migrate_style = "
-- +migrate Up
CREATE TABLE posts (id int);
-- +migrate Down
DROP TABLE posts;
";
    assert_eq!(
        keys(&digest(&[("m/1.sql", migrate_style)]), "Table"),
        ["posts"]
    );
}

// ---- custom types --------------------------------------------------------

/// An enum the migrations declare is a node, and the column written with it
/// points at it. This is the `IS_TYPE` edge the graph is for.
#[test]
fn a_declared_enum_is_a_type_the_column_points_at() {
    let out = digest(&[
        (
            "m/0001.sql",
            "CREATE TYPE user_role AS ENUM ('admin', 'member', 'guest');",
        ),
        (
            "m/0002.sql",
            "CREATE TABLE users (id int PRIMARY KEY, role user_role NOT NULL, name text);",
        ),
    ]);
    assert_eq!(keys(&out, "Type"), ["user_role"]);
    assert_eq!(
        prop(&out, "user_role", "labels"),
        Some(Value::Array(vec![
            "admin".into(),
            "member".into(),
            "guest".into()
        ]))
    );
    assert_eq!(
        edges(&out, "IS_TYPE"),
        [("users.role".into(), "user_role".into())],
        "only the column written with a declared type gets an edge"
    );
}

/// A built-in type gets no node. `Type` nodes for `text` and `int` would
/// appear in every schema, carry nothing, and bury the types a tree declares.
#[test]
fn builtin_types_get_no_node() {
    let out = digest(&[("m/0001.sql", USERS)]);
    assert_eq!(keys(&out, "Type"), [] as [String; 0]);
    assert_eq!(edges(&out, "IS_TYPE"), [] as [(String, String); 0]);
}

/// A type modifier and an array marker are not part of the type's name, so a
/// column written `user_role[]` still points at `user_role`.
#[test]
fn a_type_is_matched_past_its_modifier_and_array_marker() {
    let out = digest(&[
        ("m/0001.sql", "CREATE TYPE user_role AS ENUM ('admin');"),
        (
            "m/0002.sql",
            "CREATE TABLE users (id int, roles user_role[] NOT NULL);",
        ),
    ]);
    assert_eq!(
        edges(&out, "IS_TYPE"),
        [("users.roles".into(), "user_role".into())]
    );
}

/// A composite type has fields, and they are the same kind of fact as a
/// table's columns — so the same label and the same edge.
#[test]
fn a_composite_type_has_fields() {
    let out = digest(&[(
        "m/0001.sql",
        "CREATE TYPE address AS (street text, city text, postcode varchar(12));",
    )]);
    assert_eq!(keys(&out, "Type"), ["address"]);
    assert_eq!(
        edges(&out, "HAS_FIELD"),
        [
            ("address".into(), "address.city".into()),
            ("address".into(), "address.postcode".into()),
            ("address".into(), "address.street".into()),
        ]
    );
    assert_eq!(
        prop(&out, "address", "kind").and_then(|v| v.as_str().map(str::to_string)),
        Some("composite".into())
    );
}

/// Postgres keeps `CREATE DOMAIN` apart from `CREATE TYPE`; the graph has no
/// reason to, so a domain is a `Type` carrying the type it constrains.
#[test]
fn a_domain_is_a_type_with_a_base() {
    let out = digest(&[
        ("m/0001.sql", "CREATE DOMAIN email AS text;"),
        ("m/0002.sql", "CREATE TABLE users (id int, addr email);"),
    ]);
    assert_eq!(keys(&out, "Type"), ["email"]);
    assert_eq!(
        prop(&out, "email", "kind").and_then(|v| v.as_str().map(str::to_string)),
        Some("domain".into())
    );
    assert_eq!(
        prop(&out, "email", "base").and_then(|v| v.as_str().map(str::to_string)),
        Some("TEXT".into())
    );
    assert_eq!(
        edges(&out, "IS_TYPE"),
        [("users.addr".into(), "email".into())]
    );
}

// ---- foreign keys --------------------------------------------------------

/// A foreign key naming both sides is a column-to-column edge — the precise
/// fact, not table-to-table.
#[test]
fn a_foreign_key_joins_the_two_columns_it_names() {
    let out = digest(&[
        ("m/0001.sql", USERS),
        (
            "m/0002.sql",
            "CREATE TABLE posts (
                 id     int PRIMARY KEY,
                 author bigint NOT NULL,
                 CONSTRAINT posts_author FOREIGN KEY (author)
                     REFERENCES users (id) ON DELETE CASCADE
             );",
        ),
    ]);
    assert_eq!(
        edges(&out, "REFERENCES"),
        [("posts.author".into(), "users.id".into())]
    );
    let props: Value = out
        .edges
        .iter()
        .find(|e| e.type_ == "REFERENCES")
        .expect("the edge")
        .properties
        .parse()
        .expect("JSON");
    assert_eq!(props["on_delete"]["$value"], "CASCADE");
    assert_eq!(props["_resolved_by"]["$value"], "foreign-key");
}

/// An inline `REFERENCES` names no local column: it is the one it is written
/// on. Both spellings mean the same thing, so both are read.
#[test]
fn an_inline_reference_is_read_like_a_constraint() {
    let out = digest(&[
        ("m/0001.sql", USERS),
        (
            "m/0002.sql",
            "CREATE TABLE posts (id int, author bigint REFERENCES users (id));",
        ),
    ]);
    assert_eq!(
        edges(&out, "REFERENCES"),
        [("posts.author".into(), "users.id".into())]
    );
}

/// A reference to a table outside this tree gets no edge. An edge onto a key
/// no node declares fails the entire write, not the one edge.
#[test]
fn a_reference_to_an_unknown_table_is_dropped() {
    let out = digest(&[(
        "m/0001.sql",
        "CREATE TABLE posts (id int, author bigint REFERENCES accounts (id));",
    )]);
    assert_eq!(keys(&out, "Table"), ["posts"]);
    assert_eq!(edges(&out, "REFERENCES"), [] as [(String, String); 0]);
}

// ---- views and indexes ---------------------------------------------------

/// What a view reads comes from its defining query, including through a CTE —
/// which is why the read set is taken from sqlparser's relation visitor
/// rather than a walk of the `FROM` clause written here.
#[test]
fn a_view_points_at_what_its_query_reads() {
    let out = digest(&[
        ("m/0001.sql", USERS),
        (
            "m/0002.sql",
            "CREATE TABLE posts (id int PRIMARY KEY, author bigint);",
        ),
        (
            "m/0003.sql",
            "CREATE VIEW active_authors AS
                 WITH recent AS (SELECT author FROM posts)
                 SELECT u.id, u.email FROM users u JOIN recent ON recent.author = u.id;",
        ),
    ]);
    assert_eq!(keys(&out, "View"), ["active_authors"]);
    assert_eq!(
        edges(&out, "SELECTS_FROM"),
        [
            ("active_authors".into(), "posts".into()),
            ("active_authors".into(), "users".into()),
        ],
        "both the CTE's table and the joined one"
    );
}

/// `CREATE OR REPLACE VIEW` redefines rather than duplicating, so the last
/// definition is the one the graph holds.
#[test]
fn a_replaced_view_keeps_its_latest_definition() {
    let out = digest(&[
        ("m/0001.sql", USERS),
        (
            "m/0002.sql",
            "CREATE VIEW emails AS SELECT email FROM users;",
        ),
        (
            "m/0003.sql",
            "CREATE OR REPLACE VIEW emails (addr) AS SELECT email FROM users;",
        ),
    ]);
    assert_eq!(keys(&out, "View"), ["emails"]);
    assert_eq!(
        keys(&out, "Column")
            .iter()
            .filter(|k| k.starts_with("emails."))
            .count(),
        1
    );
    assert!(keys(&out, "Column").contains(&"emails.addr".to_string()));
}

/// An index points at its table and at each column it indexes.
#[test]
fn an_index_points_at_its_table_and_columns() {
    let out = digest(&[
        ("m/0001.sql", USERS),
        (
            "m/0002.sql",
            "CREATE UNIQUE INDEX users_email_lower ON users (email, nickname);",
        ),
    ]);
    assert_eq!(keys(&out, "Index"), ["users_email_lower"]);
    assert_eq!(prop(&out, "users_email_lower", "unique"), Some(true.into()));
    assert_eq!(
        edges(&out, "INDEXES"),
        [
            ("users_email_lower".into(), "users".into()),
            ("users_email_lower".into(), "users.email".into()),
            ("users_email_lower".into(), "users.nickname".into()),
        ]
    );
}

/// An unnamed index has no key a migration wrote, and inventing one would key
/// a node on nothing. It is counted instead.
#[test]
fn an_unnamed_index_is_reported_rather_than_keyed() {
    let out = digest(&[
        ("m/0001.sql", USERS),
        ("m/0002.sql", "CREATE INDEX ON users (email);"),
    ]);
    assert_eq!(keys(&out, "Index"), [] as [String; 0]);
    assert!(
        out.report.notes.iter().any(|n| n.contains("unnamed index")),
        "{:?}",
        out.report.notes
    );
}

// ---- provenance and dialects --------------------------------------------

/// Which migration created a thing is a fact worth keeping: it is the answer
/// to "when did this column appear", and the family promises provenance.
#[test]
fn every_object_says_which_migration_defined_it() {
    let out = digest(&[
        ("m/0001_users.sql", USERS),
        ("m/0002_role.sql", "CREATE TYPE role AS ENUM ('admin');"),
    ]);
    assert_eq!(
        keys(&out, "Migration"),
        ["m/0001_users.sql", "m/0002_role.sql"]
    );
    assert_eq!(
        edges(&out, "DEFINES"),
        [
            ("m/0001_users.sql".into(), "users".into()),
            ("m/0002_role.sql".into(), "role".into()),
        ]
    );
}

/// MySQL quotes identifiers with backticks, which the generic dialect does
/// not accept — so the dialect is a setting, and the quoting is not part of
/// the key.
#[test]
fn mysql_backticks_need_the_dialect_and_leave_the_key_clean() {
    let sql = "CREATE TABLE `users` (`id` int PRIMARY KEY, `email` varchar(255) NOT NULL);";
    let mysql = digest_with(
        &[("m/0001.sql", sql)],
        &[("dialect".into(), "mysql".into())],
    );
    assert_eq!(keys(&mysql, "Table"), ["users"]);
    assert_eq!(keys(&mysql, "Column"), ["users.email", "users.id"]);
}

/// MySQL's `CHANGE` may rename and retype in one operation, so it lowers to
/// both — and the rename has to come first or the retype names a column that
/// no longer exists.
#[test]
fn mysql_change_column_renames_and_retypes() {
    let out = digest_with(
        &[
            (
                "m/0001.sql",
                "CREATE TABLE users (id int, nick varchar(20));",
            ),
            (
                "m/0002.sql",
                "ALTER TABLE users CHANGE nick nickname varchar(80);",
            ),
        ],
        &[("dialect".into(), "mysql".into())],
    );
    assert_eq!(keys(&out, "Column"), ["users.id", "users.nickname"]);
    assert_eq!(
        prop(&out, "users.nickname", "type").and_then(|v| v.as_str().map(str::to_string)),
        Some("VARCHAR(80)".into())
    );
}

/// SQLite's DDL is a subset, and the type-free column it allows must not
/// break the column's other facts.
#[test]
fn sqlite_parses_and_keeps_an_untyped_column() {
    let out = digest_with(
        &[(
            "m/0001.sql",
            "CREATE TABLE users (id INTEGER PRIMARY KEY, data);",
        )],
        &[("dialect".into(), "sqlite".into())],
    );
    assert_eq!(keys(&out, "Column"), ["users.data", "users.id"]);
}

/// `ALTER COLUMN` changes facts already on the node rather than adding one.
#[test]
fn altering_a_column_updates_its_facts() {
    let out = digest(&[
        ("m/0001.sql", USERS),
        (
            "m/0002.sql",
            "ALTER TABLE users ALTER COLUMN nickname SET NOT NULL;
             ALTER TABLE users ALTER COLUMN nickname TYPE text;",
        ),
    ]);
    assert_eq!(prop(&out, "users.nickname", "nullable"), Some(false.into()));
    assert_eq!(
        prop(&out, "users.nickname", "type").and_then(|v| v.as_str().map(str::to_string)),
        Some("TEXT".into())
    );
}

/// A file that does not parse is counted and named, with the hint that names
/// the one setting that would change the outcome. A thin graph is explained
/// by the report, never investigated by re-running the ingest.
#[test]
fn a_file_that_does_not_parse_is_named_not_fatal() {
    let mut chunk = Partial::default();
    let dialect = dialect_from(&[]);
    let file = read(
        "m/0001.sql",
        "CREATE TABLE users (",
        dialect.as_ref(),
        &mut chunk,
    );
    assert!(file.changes.is_empty());
    assert_eq!(chunk.skipped, 1);
    assert!(
        chunk.notes.iter().any(|n| n.contains("m/0001.sql")),
        "{:?}",
        chunk.notes
    );
}

/// A statement that changes no shape is counted rather than carried, so the
/// report can say how much of a file was data rather than schema.
#[test]
fn statements_that_change_no_shape_are_counted() {
    let mut chunk = Partial::default();
    let dialect = dialect_from(&[]);
    let file = read(
        "m/0001.sql",
        "CREATE TABLE users (id int);
         INSERT INTO users VALUES (1);
         GRANT SELECT ON users TO readonly;",
        dialect.as_ref(),
        &mut chunk,
    );
    assert_eq!(file.changes.len(), 1);
    assert_eq!(file.ignored, 2);
}

/// Two tables one unqualified name could mean. Guessing which would attach a
/// column to the wrong table, so neither is chosen and the statement is named.
#[test]
fn an_ambiguous_table_name_is_not_resolved() {
    let out = digest(&[
        (
            "m/0001.sql",
            "CREATE TABLE public.users (id int); CREATE TABLE audit.users (id int);",
        ),
        ("m/0002.sql", "ALTER TABLE users ADD COLUMN email text;"),
    ]);
    assert!(
        !keys(&out, "Column").iter().any(|k| k.ends_with(".email")),
        "{:?}",
        keys(&out, "Column")
    );
    assert!(
        out.report.notes.iter().any(|n| n.contains("not applied")),
        "{:?}",
        out.report.notes
    );
}

/// `REFERENCES workspaces` names only the table, and SQL defines that as its
/// primary key — which the replayed schema already holds. This is the common
/// spelling in the wild: every foreign key in one real SQLite tree is written
/// this way, and leaving them table-level would lose the column on all of them.
#[test]
fn a_reference_naming_only_a_table_resolves_to_its_primary_key() {
    let out = digest(&[
        ("m/0001.sql", USERS),
        (
            "m/0002.sql",
            "CREATE TABLE posts (id int PRIMARY KEY, author bigint REFERENCES users);",
        ),
    ]);
    assert_eq!(
        edges(&out, "REFERENCES"),
        [("posts.author".into(), "users.id".into())]
    );
    let props: Value = out
        .edges
        .iter()
        .find(|e| e.type_ == "REFERENCES")
        .expect("the edge")
        .properties
        .parse()
        .expect("JSON");
    assert_eq!(props["_resolved_by"]["$value"], "foreign-key-primary-key");
}

/// A composite primary key and a single-column foreign key do not line up.
/// Pairing them would invent a column, so the edge points at the table and
/// says that is what happened.
#[test]
fn a_reference_that_cannot_be_paired_points_at_the_table() {
    let out = digest(&[
        (
            "m/0001.sql",
            "CREATE TABLE memberships (
                 tenant int, user_id int, PRIMARY KEY (tenant, user_id));",
        ),
        (
            "m/0002.sql",
            "CREATE TABLE audit (id int, membership int REFERENCES memberships);",
        ),
    ]);
    assert_eq!(
        edges(&out, "REFERENCES"),
        [("audit.membership".into(), "memberships".into())]
    );
    let props: Value = out
        .edges
        .iter()
        .find(|e| e.type_ == "REFERENCES")
        .expect("the edge")
        .properties
        .parse()
        .expect("JSON");
    assert_eq!(props["_resolved_by"]["$value"], "foreign-key-table");
}

/// A file adding several columns to a table whose own `CREATE` was never seen
/// would repeat one sentence once per column. Said once per file and object.
#[test]
fn an_unknown_target_is_reported_once_per_file() {
    let out = digest(&[(
        "m/0002.sql",
        "ALTER TABLE users ADD COLUMN a text;
         ALTER TABLE users ADD COLUMN b text;
         ALTER TABLE users ADD COLUMN c text;",
    )]);
    let said: Vec<&String> = out
        .report
        .notes
        .iter()
        .filter(|n| n.contains("not applied"))
        .collect();
    assert_eq!(said.len(), 1, "{said:?}");
}

// ---- queries written in code ---------------------------------------------

/// Migrations and source files through the real pipeline: replay, scan, emit,
/// with only `host::list` and `host::read` left out.
fn digest_code(migrations: &[(&str, &str)], code: &[(&str, &str)]) -> Output {
    digest_code_with(migrations, code, &[])
}

fn digest_code_with(
    migrations: &[(&str, &str)],
    code: &[(&str, &str)],
    options: &[(String, String)],
) -> Output {
    let dialect = dialect_from(options);
    let mut chunk = Partial::default();
    for (path, sql) in migrations {
        let file = read(path, sql, dialect.as_ref(), &mut chunk);
        chunk.files.push(file);
    }
    let mut out = output();
    let schema = schema::replay(chunk.files);
    emit::emit(&schema, &mut out);

    let mut found = Scanned::default();
    if wants_inline(options) {
        for (path, text) in code {
            scan_text(
                path,
                text,
                dialect.as_ref(),
                broader_grammar(options).as_deref(),
                &mut found,
            );
        }
    }
    emit::statements(&schema, &found.stmts, &mut out);
    out.report.notes.extend(found.notes());
    out.finish()
}

/// An edge's property `$value`, for an edge named by both its ends.
fn edge_prop(o: &Output, src: &str, dst: &str, name: &str) -> Option<Value> {
    let e = o.edges.iter().find(|e| e.src == src && e.dst == dst)?;
    let props: Value = e.properties.parse().ok()?;
    props.get(name)?.get("$value").cloned()
}

/// Two tables and a join between them, so an unqualified column has more than
/// one table it could belong to.
const TWO: &str = "
CREATE TABLE users (id bigint PRIMARY KEY, email text, name text);
CREATE TABLE orgs  (id bigint PRIMARY KEY, name text);
CREATE TABLE sessions (id bigint PRIMARY KEY, user_id bigint);
";

/// The whole point: a query in Rust lands on the table the DDL declares.
#[test]
fn a_query_in_code_reads_the_table_it_names() {
    let out = digest_code(
        &[("m/0001.sql", TWO)],
        &[(
            "src/db.rs",
            "fn load(id: i64) {\n    \
             let sql = \"SELECT email FROM users WHERE id = $1\";\n}\n",
        )],
    );
    assert_eq!(keys(&out, "Statement"), ["src/db.rs:2"]);
    assert_eq!(
        edges(&out, "READS"),
        [("src/db.rs:2".to_string(), "users".to_string())]
    );
    assert_eq!(
        prop(&out, "src/db.rs:2", "kind"),
        Some(Value::from("select"))
    );
}

/// Where it was written is the only name a query in a literal has, and the
/// span is what joins it to the function around it.
#[test]
fn a_statement_says_where_it_was_written() {
    let out = digest_code(
        &[("m/0001.sql", TWO)],
        &[(
            "src/db.rs",
            "// a header\nfn load() {\n    let sql = \"\n        SELECT email\n        FROM users\";\n}\n",
        )],
    );
    assert_eq!(
        prop(&out, "src/db.rs:3", "file"),
        Some(Value::from("src/db.rs"))
    );
    assert_eq!(prop(&out, "src/db.rs:3", "line"), Some(Value::from(3i64)));
    assert_eq!(
        prop(&out, "src/db.rs:3", "end_line"),
        Some(Value::from(5i64)),
        "the literal spans to where it closes"
    );
}

/// A projected column is part of the shape the caller gets back; one in a
/// predicate is not, and the edge says which.
#[test]
fn a_column_edge_says_which_role_it_was_named_in() {
    let out = digest_code(
        &[("m/0001.sql", TWO)],
        &[(
            "src/db.rs",
            "let sql = \"SELECT email FROM users WHERE id = $1\";\n",
        )],
    );
    assert_eq!(
        edge_prop(&out, "src/db.rs:1", "users.email", "role"),
        Some(Value::from("projection"))
    );
    assert_eq!(
        edge_prop(&out, "src/db.rs:1", "users.id", "role"),
        Some(Value::from("reference"))
    );
}

/// `INSERT … SELECT` does both, and the two tables must not be confused.
#[test]
fn an_insert_from_a_select_writes_one_table_and_reads_another() {
    let out = digest_code(
        &[("m/0001.sql", TWO)],
        &[(
            "src/db.rs",
            "let sql = \"INSERT INTO sessions (user_id) SELECT id FROM users\";\n",
        )],
    );
    assert_eq!(
        edges(&out, "WRITES"),
        [("src/db.rs:1".to_string(), "sessions".to_string())]
    );
    assert_eq!(
        edges(&out, "READS"),
        [("src/db.rs:1".to_string(), "users".to_string())]
    );
    assert_eq!(
        edge_prop(&out, "src/db.rs:1", "sessions.user_id", "role"),
        Some(Value::from("assignment")),
        "the insert's column list belongs to its own target"
    );
}

/// An `UPDATE` assigns to its own target whatever else it reads.
#[test]
fn an_update_assigns_to_its_target() {
    let out = digest_code(
        &[("m/0001.sql", TWO)],
        &[(
            "src/db.rs",
            "let sql = \"UPDATE users SET email = $1 WHERE id = $2\";\n",
        )],
    );
    assert_eq!(
        edges(&out, "WRITES"),
        [("src/db.rs:1".to_string(), "users".to_string())]
    );
    assert_eq!(
        edge_prop(&out, "src/db.rs:1", "users.email", "role"),
        Some(Value::from("assignment"))
    );
}

/// An alias is how real SQL qualifies a column, so it has to resolve.
#[test]
fn an_alias_binds_a_column_to_its_table() {
    let out = digest_code(
        &[("m/0001.sql", TWO)],
        &[(
            "src/db.rs",
            "let sql = \"SELECT u.email, o.name FROM users u JOIN orgs o ON o.id = u.id\";\n",
        )],
    );
    let cols = edges(&out, "USES_COLUMN");
    assert!(
        cols.contains(&("src/db.rs:1".to_string(), "users.email".to_string())),
        "{cols:?}"
    );
    assert!(
        cols.contains(&("src/db.rs:1".to_string(), "orgs.name".to_string())),
        "{cols:?}"
    );
}

/// Two tables and no qualifier: `name` is on both, and guessing would be
/// wrong half the time.
#[test]
fn a_bare_column_with_two_relations_is_unbound_not_guessed() {
    let out = digest_code(
        &[("m/0001.sql", TWO)],
        &[(
            "src/db.rs",
            "let sql = \"SELECT name FROM users JOIN orgs ON orgs.id = users.id\";\n",
        )],
    );
    let unbound = prop(&out, "src/db.rs:1", "unbound_columns");
    assert_eq!(unbound, Some(Value::from("name")), "{unbound:?}");
    assert!(
        !edges(&out, "USES_COLUMN")
            .iter()
            .any(|(_, dst)| dst.ends_with(".name")),
        "no edge under a guess"
    );
}

/// A table no migration here declares gets no edge at all: a dangling
/// endpoint fails the entire write, not the one edge.
#[test]
fn a_query_against_an_unknown_table_writes_no_edge() {
    let out = digest_code(
        &[("m/0001.sql", TWO)],
        &[(
            "src/db.rs",
            "let sql = \"SELECT id FROM other_service.audit\";\n",
        )],
    );
    assert_eq!(keys(&out, "Statement"), ["src/db.rs:1"]);
    assert!(
        edges(&out, "READS").is_empty(),
        "{:?}",
        edges(&out, "READS")
    );
    let declared: Vec<String> = out.nodes.iter().map(|n| n.key.clone()).collect();
    for e in &out.edges {
        assert!(declared.contains(&e.dst), "dangling endpoint {}", e.dst);
    }
}

/// A hole where a value goes reads as the parameter the driver will send.
#[test]
fn a_template_hole_is_read_as_a_bind_parameter() {
    let out = digest_code(
        &[("m/0001.sql", TWO)],
        &[(
            "src/db.ts",
            "const sql = `SELECT email FROM users WHERE id = ${id}`;\n",
        )],
    );
    assert_eq!(
        edges(&out, "READS"),
        [("src/db.ts:1".to_string(), "users".to_string())]
    );
    assert_eq!(
        prop(&out, "src/db.ts:1", "parameterized"),
        Some(Value::from(true))
    );
}

/// A hole in a table position is a table named at runtime, which is not a
/// fact about this tree — so the statement is refused and said, not guessed.
#[test]
fn a_hole_in_a_table_position_is_refused_and_named() {
    let out = digest_code(
        &[("m/0001.sql", TWO)],
        &[("src/db.ts", "const sql = `SELECT id FROM ${table}`;\n")],
    );
    assert!(keys(&out, "Statement").is_empty());
    assert!(
        out.report
            .notes
            .iter()
            .any(|n| n.contains("would not parse") && n.contains("src/db.ts:1")),
        "{:?}",
        out.report.notes
    );
}

/// How Java and C# write a statement: several literals and a `+`.
#[test]
fn a_query_split_across_literals_is_one_statement() {
    let out = digest_code(
        &[("m/0001.sql", TWO)],
        &[(
            "src/Dao.java",
            "String sql = \"SELECT email \"\n    + \"FROM users \"\n    + \"WHERE id = ?\";\n",
        )],
    );
    assert_eq!(keys(&out, "Statement"), ["src/Dao.java:1"]);
    assert_eq!(
        edges(&out, "READS"),
        [("src/Dao.java:1".to_string(), "users".to_string())]
    );
}

/// C concatenates by adjacency alone; Rust does not, and reading `"a" "b"`
/// there as one string would invent a statement nobody wrote.
#[test]
fn adjacency_joins_only_where_the_language_says_so() {
    let c = digest_code(
        &[("m/0001.sql", TWO)],
        &[(
            "src/q.c",
            "const char *q = \"SELECT email \"\n                \"FROM users\";\n",
        )],
    );
    assert_eq!(
        edges(&c, "READS"),
        [("src/q.c:1".to_string(), "users".to_string())]
    );

    let rs = digest_code(
        &[("m/0001.sql", TWO)],
        &[("src/q.rs", "f(\"SELECT email \", \"FROM users\");\n")],
    );
    assert!(keys(&rs, "Statement").is_empty(), "two arguments, no query");
}

/// The reason comments are skipped rather than merely ignored. In a language
/// where `\'` quotes a string, the apostrophe in one comment opens a literal
/// that closes at the apostrophe in the next — and everything between, the
/// query included, is swallowed.
#[test]
fn an_apostrophe_in_a_comment_does_not_swallow_the_query_under_it() {
    let out = digest_code(
        &[("m/0001.sql", TWO)],
        &[(
            "src/db.ts",
            "// don't\nconst sql = \"SELECT email FROM users\";\n// can't\n",
        )],
    );
    assert_eq!(keys(&out, "Statement"), ["src/db.ts:2"]);
}

/// A commented-out query is not a query.
#[test]
fn a_commented_out_query_is_not_a_statement() {
    let out = digest_code(
        &[("m/0001.sql", TWO)],
        &[(
            "src/db.rs",
            "// let sql = \"SELECT email FROM users\";\n/* \"SELECT name FROM orgs\" */\n",
        )],
    );
    assert!(keys(&out, "Statement").is_empty());
}

/// A lifetime is not a quote, and treating `'a` as one would desynchronise
/// every Rust file that has one.
#[test]
fn a_rust_lifetime_is_not_a_string() {
    let out = digest_code(
        &[("m/0001.sql", TWO)],
        &[(
            "src/db.rs",
            "fn load<'a>(s: &'a str) {\n    let sql = \"SELECT email FROM users\";\n}\n",
        )],
    );
    assert_eq!(keys(&out, "Statement"), ["src/db.rs:2"]);
}

/// A raw string is how a Rust query carries a quoted identifier, and the
/// quotes have to survive the lexer to reach the parser.
#[test]
fn a_raw_string_keeps_the_quotes_inside_it() {
    let out = digest_code(
        &[("m/0001.sql", TWO)],
        &[(
            "src/db.rs",
            "let sql = r#\"SELECT \"email\" FROM users\"#;\n",
        )],
    );
    assert!(
        edges(&out, "USES_COLUMN")
            .contains(&("src/db.rs:1".to_string(), "users.email".to_string())),
        "{:?}",
        edges(&out, "USES_COLUMN")
    );
}

/// An escaped quote inside an ordinary literal is the same identifier.
#[test]
fn an_escaped_quote_reaches_the_parser_as_a_quote() {
    let out = digest_code(
        &[("m/0001.sql", TWO)],
        &[(
            "src/Dao.java",
            "String sql = \"SELECT \\\"email\\\" FROM users\";\n",
        )],
    );
    assert!(
        edges(&out, "USES_COLUMN")
            .contains(&("src/Dao.java:1".to_string(), "users.email".to_string())),
        "{:?}",
        edges(&out, "USES_COLUMN")
    );
}

/// Python writes a query in a triple-quoted block, Go in backquotes.
#[test]
fn every_language_s_own_literal_is_read() {
    let py = digest_code(
        &[("m/0001.sql", TWO)],
        &[(
            "app/db.py",
            "SQL = \"\"\"\nSELECT email\nFROM users\n\"\"\"\n",
        )],
    );
    assert_eq!(
        edges(&py, "READS"),
        [("app/db.py:1".to_string(), "users".to_string())]
    );

    let go = digest_code(
        &[("m/0001.sql", TWO)],
        &[("db/q.go", "const q = `SELECT email FROM users`\n")],
    );
    assert_eq!(
        edges(&go, "READS"),
        [("db/q.go:1".to_string(), "users".to_string())]
    );

    let cs = digest_code(
        &[("m/0001.sql", TWO)],
        &[("Db.cs", "var q = @\"SELECT \"\"email\"\" FROM users\";\n")],
    );
    assert!(
        edges(&cs, "USES_COLUMN").contains(&("Db.cs:1".to_string(), "users.email".to_string())),
        "a doubled quote in a verbatim string is one quote: {:?}",
        edges(&cs, "USES_COLUMN")
    );
}

/// Every other string in a repository must not become a node.
#[test]
fn a_string_that_does_not_open_a_query_is_not_one() {
    let out = digest_code(
        &[("m/0001.sql", TWO)],
        &[(
            "src/db.rs",
            "let a = \"hello world\";\nlet b = \"../migrations/0001.sql\";\nlet c = \"users\";\n",
        )],
    );
    assert!(keys(&out, "Statement").is_empty());
    assert!(
        out.report
            .notes
            .iter()
            .all(|n| !n.contains("would not parse"))
    );
}

/// One line can hold two, and a key is one statement's.
#[test]
fn two_statements_on_one_line_get_distinct_keys() {
    let out = digest_code(
        &[("m/0001.sql", TWO)],
        &[(
            "src/db.rs",
            "let (a, b) = (\"SELECT email FROM users\", \"SELECT name FROM orgs\");\n",
        )],
    );
    assert_eq!(keys(&out, "Statement"), ["src/db.rs:1", "src/db.rs:1#2"]);
}

/// A leading `--` is a header, not a reason to refuse the statement under it.
#[test]
fn a_leading_sql_comment_does_not_hide_the_statement() {
    let out = digest_code(
        &[("m/0001.sql", TWO)],
        &[(
            "src/db.rs",
            "let sql = \"-- the hot path\\nSELECT email FROM users\";\n",
        )],
    );
    assert_eq!(
        edges(&out, "READS"),
        [("src/db.rs:1".to_string(), "users".to_string())]
    );
}

/// The scan costs a pass over the tree's source, and an operator who does not
/// want it says so.
#[test]
fn the_scan_can_be_turned_off() {
    let code = [("src/db.rs", "let sql = \"SELECT email FROM users\";\n")];
    let off = digest_code_with(
        &[("m/0001.sql", TWO)],
        &code,
        &[("inline".into(), "false".into())],
    );
    assert!(keys(&off, "Statement").is_empty());
    assert!(
        !keys(&off, "Table").is_empty(),
        "the migrations are still read"
    );
}

/// The report accounts for what was read, so a thin result is explained
/// rather than investigated.
#[test]
fn the_report_says_what_the_scan_read() {
    let out = digest_code(
        &[("m/0001.sql", TWO)],
        &[
            ("src/db.rs", "let sql = \"SELECT email FROM users\";\n"),
            ("src/other.rs", "let x = 1;\n"),
        ],
    );
    assert!(
        out.report
            .notes
            .iter()
            .any(|n| n.contains("read 2 source file(s)") && n.contains("found 1 in 1")),
        "{:?}",
        out.report.notes
    );
}

/// A query whose `FROM` is a function reads rows from no table at all, so a
/// node for it would carry a literal and not one fact.
#[test]
fn a_query_about_no_table_is_not_a_node() {
    let out = digest_code(
        &[("m/0001.sql", TWO)],
        &[(
            "src/db.rs",
            "let a = \"SELECT n FROM generate_series(1, 10) n\";\n\
             let b = \"SELECT x FROM unnest($1) x\";\n",
        )],
    );
    assert!(keys(&out, "Statement").is_empty());
    assert!(
        out.report
            .notes
            .iter()
            .any(|n| n.contains("2 that name no table")),
        "{:?}",
        out.report.notes
    );
}

/// A tree with two databases in it writes placeholders both ways: `$1` for
/// the Postgres primary and `?` for the SQLite cache, in neighbouring
/// modules. One declared grammar cannot read both, so a refused query is
/// retried against the broadest one — and says so.
#[test]
fn a_query_the_declared_grammar_refuses_is_retried_and_marked() {
    let pg = [("dialect".to_string(), "postgres".to_string())];
    let out = digest_code_with(
        &[("m/0001.sql", TWO)],
        &[(
            "src/cache.rs",
            "let sql = \"SELECT email FROM users WHERE id = ?\";\n",
        )],
        &pg,
    );
    assert_eq!(
        edges(&out, "READS"),
        [("src/cache.rs:1".to_string(), "users".to_string())]
    );
    assert_eq!(
        prop(&out, "src/cache.rs:1", "_resolved_by"),
        Some(Value::from("generic-grammar")),
        "the node says which grammar read it"
    );

    // And one the declared grammar reads is not marked.
    let plain = digest_code_with(
        &[("m/0001.sql", TWO)],
        &[(
            "src/db.rs",
            "let sql = \"SELECT email FROM users WHERE id = $1\";\n",
        )],
        &pg,
    );
    assert_eq!(prop(&plain, "src/db.rs:1", "_resolved_by"), None);
}

/// A user interface is full of strings that open with one of SQL's own words.
/// Handing each to the parser costs nothing in edges and everything in a
/// report: on one real tree the opening word alone named 178 of them.
#[test]
fn a_ui_label_that_opens_with_a_sql_word_is_not_a_candidate() {
    let out = digest_code(
        &[("m/0001.sql", TWO)],
        &[(
            "app/ui.tsx",
            "const a = 'Delete';\nconst b = 'Select a workspace';\n\
             const c = 'Update available';\nconst d = 'Insert the card';\n",
        )],
    );
    assert!(keys(&out, "Statement").is_empty());
    assert!(
        out.report
            .notes
            .iter()
            .all(|n| !n.contains("would not parse")),
        "nothing reached the parser: {:?}",
        out.report.notes
    );
}
