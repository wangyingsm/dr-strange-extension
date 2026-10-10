//! The scanner's own tests. Every case is SQL a mapper actually writes.

use super::*;

fn tables(sql: &str) -> Vec<(String, Access)> {
    touched(sql).tables
}

fn reads(sql: &str) -> Vec<String> {
    tables(sql)
        .into_iter()
        .filter(|(_, a)| *a == Access::Read)
        .map(|(t, _)| t)
        .collect()
}

/// One table, so every bare column in the statement is its own.
#[test]
fn a_select_binds_its_columns_to_its_only_table() {
    let t = touched("SELECT id, user_id FROM orders WHERE paid = ?");
    assert_eq!(t.tables, vec![("orders".into(), Access::Read)]);
    assert_eq!(
        t.bound,
        vec![
            ("orders".to_string(), "id".to_string()),
            ("orders".to_string(), "paid".to_string()),
            ("orders".to_string(), "user_id".to_string()),
        ]
    );
    assert!(t.unbound.is_empty());
}

/// Two tables: a qualified column binds, a bare one is named and left alone.
/// A column under the wrong table would be worse than no column at all.
#[test]
fn a_join_binds_only_what_is_qualified() {
    let t = touched("SELECT u.id, o.total, created FROM users u JOIN orders o ON o.user_id = u.id");
    assert_eq!(
        reads("SELECT 1 FROM users u JOIN orders o ON 1 = 1"),
        vec!["users", "orders"]
    );
    assert!(t.bound.contains(&("users".to_string(), "id".to_string())));
    assert!(
        t.bound
            .contains(&("orders".to_string(), "total".to_string()))
    );
    assert!(
        t.bound
            .contains(&("orders".to_string(), "user_id".to_string()))
    );
    assert_eq!(t.unbound, vec!["created".to_string()]);
}

/// `AS` is optional, and an alias is not a column.
#[test]
fn an_alias_is_read_either_way() {
    let t = touched("SELECT u.id FROM users AS u WHERE u.id = ?");
    assert_eq!(t.tables, vec![("users".into(), Access::Read)]);
    assert_eq!(t.bound, vec![("users".to_string(), "id".to_string())]);
    assert!(t.unbound.is_empty());
}

/// A name followed by `(` is a function.
#[test]
fn a_function_call_is_not_a_column() {
    let t = touched("SELECT COUNT(*), NOW(), MAX(total) FROM orders");
    assert_eq!(t.bound, vec![("orders".to_string(), "total".to_string())]);
    assert!(t.unbound.is_empty());
}

/// The keyword that introduces a table says whether it is read or written,
/// which an `INSERT … SELECT` needs: one of each.
#[test]
fn the_introducing_keyword_says_read_or_write() {
    assert_eq!(
        tables("INSERT INTO archive (id) SELECT id FROM orders"),
        vec![
            ("archive".into(), Access::Write),
            ("orders".into(), Access::Read)
        ]
    );
    assert_eq!(
        tables("UPDATE orders SET paid = 1 WHERE id = ?"),
        vec![("orders".into(), Access::Write)]
    );
    // `DELETE FROM t` writes `t`, however the keyword reads.
    assert_eq!(
        tables("DELETE FROM orders WHERE id = ?"),
        vec![("orders".into(), Access::Write)]
    );
}

/// A subquery's parenthesis is not a table name; the subquery's own `FROM` is.
#[test]
fn a_subquery_is_not_a_table() {
    let t = touched("SELECT x.id FROM (SELECT id FROM orders) x");
    assert_eq!(t.tables, vec![("orders".into(), Access::Read)]);
}

/// A `${…}` in a table position means the list is incomplete, and says so
/// rather than binding the hole's text as a name.
#[test]
fn an_interpolated_table_is_admitted_not_guessed() {
    let t = touched("SELECT id FROM ${tableName} WHERE id = ?");
    assert!(t.tables.is_empty());
    assert!(t.interpolated_table);
    // With no table, a bare column has nothing to bind to.
    assert_eq!(t.unbound, vec!["id".to_string()]);
}

/// Comments and string literals name nothing.
#[test]
fn comments_and_literals_are_not_columns() {
    let t =
        touched("SELECT id -- the pk\n /* orders.total */ FROM orders WHERE name = 'from users'");
    assert_eq!(t.tables, vec![("orders".into(), Access::Read)]);
    assert_eq!(
        t.bound,
        vec![
            ("orders".to_string(), "id".to_string()),
            ("orders".to_string(), "name".to_string()),
        ]
    );
}

/// Backticks are MySQL's quoting, not part of the name.
#[test]
fn backticks_are_not_part_of_a_name() {
    let t = touched("SELECT `id` FROM `orders`");
    assert_eq!(t.tables, vec![("orders".into(), Access::Read)]);
    assert_eq!(t.bound, vec![("orders".to_string(), "id".to_string())]);
}

/// An insert names its columns in its target's parentheses.
#[test]
fn an_insert_binds_the_columns_it_names() {
    let t = touched("INSERT INTO orders (user_id, total) VALUES (?, ?)");
    assert_eq!(t.tables, vec![("orders".into(), Access::Write)]);
    assert_eq!(
        t.bound,
        vec![
            ("orders".to_string(), "total".to_string()),
            ("orders".to_string(), "user_id".to_string()),
        ]
    );
}

/// A comma-separated `FROM` is the old join spelling, and still two tables.
#[test]
fn a_comma_separated_from_is_two_tables() {
    assert_eq!(
        tables("SELECT u.id FROM users u, orders o WHERE o.user_id = u.id"),
        vec![
            ("users".into(), Access::Read),
            ("orders".into(), Access::Read)
        ]
    );
}

/// Nothing at all is a statement that touched nothing — never a panic.
#[test]
fn an_empty_statement_touches_nothing() {
    let t = touched("");
    assert!(t.tables.is_empty());
    assert!(t.bound.is_empty());
    assert!(t.unbound.is_empty());
}

/// A `#{…}` is a value the driver binds, never an identifier, so nothing
/// inside it is read as a column — and the scanner needs no rewritten SQL to
/// see that.
#[test]
fn a_bind_parameter_names_no_column() {
    let t = touched("SELECT id FROM orders WHERE id = #{id} AND name = #{user.name}");
    assert_eq!(t.tables, vec![("orders".into(), Access::Read)]);
    assert_eq!(
        t.bound,
        vec![
            ("orders".to_string(), "id".to_string()),
            ("orders".to_string(), "name".to_string()),
        ]
    );
    assert!(t.unbound.is_empty(), "{:?}", t.unbound);
}

/// A bind parameter in a table position is nonsense a driver could not send,
/// so it ends the table list rather than being admitted like a `${…}`.
#[test]
fn a_bind_parameter_is_not_a_table() {
    let t = touched("SELECT id FROM #{table}");
    assert!(t.tables.is_empty());
    assert!(!t.interpolated_table, "a bind is not an interpolation");
}

/// The keyword list is binary-searched, so it has to stay sorted. A new word
/// added in the wrong place would make the lookup miss it silently.
#[test]
fn the_keyword_list_is_sorted() {
    assert!(
        KEYWORDS.windows(2).all(|w| w[0] < w[1]),
        "out of order at {:?}",
        KEYWORDS.windows(2).find(|w| w[0] >= w[1])
    );
    assert_eq!(
        KEYWORDS.iter().map(|k| k.len()).max(),
        Some(LONGEST_KEYWORD),
        "LONGEST_KEYWORD must match the longest entry, or the length test rejects it"
    );
    for k in KEYWORDS {
        assert!(is_keyword(k), "{k} must be found");
        assert!(
            is_keyword(&k.to_uppercase()),
            "{k} must be found upper-cased"
        );
    }
    assert!(!is_keyword("orders"));
    assert!(!is_keyword("user_id"));
    assert!(!is_keyword("a_column_name_longer_than_any_keyword"));
}

/// A statement that names one table many times — a long `UNION ALL`, or a
/// generated `<foreach>` — records it once. The column pass searches the
/// alias list per token, so an unbounded list there is quadratic.
#[test]
fn a_repeated_table_is_recorded_once() {
    let mut sql = String::new();
    for i in 0..50 {
        if i > 0 {
            sql.push_str(" UNION ALL ");
        }
        sql.push_str("SELECT id FROM orders o WHERE o.id = #{x}");
    }
    let t = touched(&sql);
    assert_eq!(t.tables, vec![("orders".into(), Access::Read)]);
    assert_eq!(t.bound, vec![("orders".to_string(), "id".to_string())]);
}

/// PostgreSQL and standard SQL quote identifiers with `"`. A quoted name
/// where only a table can stand settles what this statement's quotes mean,
/// and its columns follow.
#[test]
fn double_quoted_identifiers_are_read_as_names() {
    let t = touched(r#"SELECT "id", "total" FROM "orders" WHERE "paid" = #{p}"#);
    assert_eq!(t.tables, vec![("orders".into(), Access::Read)]);
    assert_eq!(
        t.bound,
        vec![
            ("orders".to_string(), "id".to_string()),
            ("orders".to_string(), "paid".to_string()),
            ("orders".to_string(), "total".to_string()),
        ]
    );
    assert!(t.unbound.is_empty(), "{:?}", t.unbound);
}

/// In MySQL's default mode the same bytes are a string literal, so without
/// that evidence a `"…"` is named on the statement and bound to nothing — a
/// column invented from a string would be a worse answer than none.
#[test]
fn a_double_quoted_value_is_not_invented_as_a_column() {
    let t = touched(r#"SELECT id FROM orders WHERE name = "bob""#);
    assert_eq!(t.tables, vec![("orders".into(), Access::Read)]);
    assert_eq!(
        t.bound,
        vec![
            ("orders".to_string(), "id".to_string()),
            ("orders".to_string(), "name".to_string()),
        ]
    );
    assert_eq!(t.unbound, vec!["bob".to_string()]);
}

/// A single-quoted string is a literal in every dialect there is, and names
/// nothing at all.
#[test]
fn a_single_quoted_literal_names_nothing() {
    let t = touched("SELECT id FROM orders WHERE name = 'from users'");
    assert_eq!(t.tables, vec![("orders".into(), Access::Read)]);
    assert!(
        !t.unbound.iter().any(|u| u.contains("from")),
        "{:?}",
        t.unbound
    );
}

/// A doubled quote is an escaped one, in both dialects' spelling, and must not
/// end the name — or the rest of the statement reads as quoted text.
#[test]
fn a_doubled_quote_stays_inside_the_name() {
    let t = touched(r#"SELECT id FROM "odd""name" WHERE id = #{x}"#);
    assert_eq!(t.tables, vec![(r#"odd"name"#.into(), Access::Read)]);
    assert_eq!(t.bound, vec![(r#"odd"name"#.to_string(), "id".to_string())]);
}

/// A quoted alias stands for its table like any other.
#[test]
fn a_quoted_alias_is_an_alias() {
    let t = touched(r#"SELECT "o"."id" FROM "orders" "o""#);
    assert_eq!(t.bound, vec![("orders".to_string(), "id".to_string())]);
}

// ---- dialects other than the one the issue came from ----------------------

/// SQL Server and Sybase bracket their identifiers, and no dialect opens a
/// string that way — so, like a backtick, it needs no evidence. A dotted
/// chain of quoted parts is one name, not one name per part.
#[test]
fn bracket_quoted_identifiers_are_names() {
    let t = touched("SELECT [id], [total] FROM [dbo].[Orders] WHERE [paid] = #{p}");
    assert_eq!(t.tables, vec![("dbo.Orders".into(), Access::Read)]);
    assert_eq!(
        t.bound,
        vec![
            ("dbo.Orders".to_string(), "id".to_string()),
            ("dbo.Orders".to_string(), "paid".to_string()),
            ("dbo.Orders".to_string(), "total".to_string()),
        ]
    );
}

/// Every dialect's quoting, in a chain, reading as the one name it is.
#[test]
fn a_quoted_chain_is_one_name() {
    for sql in [
        r#"SELECT id FROM "myschema"."orders""#,
        "SELECT id FROM `mydb`.`orders`",
        "SELECT id FROM [mydb].[orders]",
        r#"SELECT id FROM myschema."orders""#,
    ] {
        let t = touched(sql);
        assert_eq!(t.tables.len(), 1, "{sql}: {:?}", t.tables);
        assert!(
            t.tables[0].0.contains('.'),
            "{sql}: {:?} keeps its qualifier",
            t.tables
        );
    }
}

/// Oracle and T-SQL let the verb stand without its preposition.
#[test]
fn a_verb_without_its_preposition_still_names_its_table() {
    assert_eq!(
        touched("DELETE orders WHERE id = #{id}").tables,
        vec![("orders".into(), Access::Write)]
    );
    assert_eq!(
        touched("INSERT orders (id) VALUES (#{id})").tables,
        vec![("orders".into(), Access::Write)]
    );
}

/// `MERGE INTO target USING source` touches both, and `JOIN … USING (col)`
/// is not a table at all.
#[test]
fn merge_names_both_sides() {
    assert_eq!(
        touched("MERGE INTO orders o USING staging s ON s.id = o.id WHEN MATCHED THEN UPDATE SET o.total = s.total").tables,
        vec![
            ("orders".into(), Access::Write),
            ("staging".into(), Access::Read)
        ]
    );
    assert_eq!(
        touched("SELECT id FROM orders JOIN users USING (user_id)").tables,
        vec![
            ("orders".into(), Access::Read),
            ("users".into(), Access::Read)
        ]
    );
}

/// An output alias is not a column of the table, and neither is a cast's type
/// — in either spelling.
#[test]
fn an_alias_or_a_type_is_not_a_column() {
    let t = touched("SELECT total AS amount, id::text, CAST(created AS date) FROM orders");
    assert_eq!(
        t.bound,
        vec![
            ("orders".to_string(), "created".to_string()),
            ("orders".to_string(), "id".to_string()),
            ("orders".to_string(), "total".to_string()),
        ]
    );
    assert!(t.unbound.is_empty(), "{:?}", t.unbound);
}

/// A schema-qualified table keeps its qualifier, and a column written against
/// it binds through the whole name rather than its first segment.
#[test]
fn a_schema_qualified_table_binds_its_columns() {
    let t = touched("SELECT myschema.orders.id FROM myschema.orders");
    assert_eq!(t.tables, vec![("myschema.orders".into(), Access::Read)]);
    assert_eq!(
        t.bound,
        vec![("myschema.orders".to_string(), "id".to_string())]
    );
}

/// The pseudo-columns and niladic functions each dialect spells bare are not
/// columns of anything — nor are the boolean literals.
#[test]
fn pseudo_columns_are_not_columns() {
    for sql in [
        "SELECT SYSDATE FROM dual",
        "SELECT id FROM orders WHERE ROWNUM <= 10",
        "SELECT id FROM orders WHERE created < CURRENT_TIMESTAMP",
        "SELECT id FROM orders WHERE active = false",
        "SELECT id FROM orders WHERE active = TRUE",
        "SELECT TOP 10 id FROM orders",
    ] {
        let t = touched(sql);
        let names: Vec<&str> = t.bound.iter().map(|(_, c)| c.as_str()).collect();
        for bad in [
            "sysdate",
            "rownum",
            "current_timestamp",
            "false",
            "true",
            "top",
            "dual",
        ] {
            assert!(
                !names.iter().any(|c| c.eq_ignore_ascii_case(bad)),
                "{sql}: {bad} is not a column — got {names:?}"
            );
        }
        assert!(t.unbound.is_empty(), "{sql}: {:?}", t.unbound);
    }
}

/// The shape that found two bugs the single-feature tests did not: a mapper
/// written against a PostgreSQL schema, mixing quoted and bare parts, an
/// alias that is itself quoted, a cast, and an output alias.
#[test]
fn a_mixed_dialect_statement_binds_everything_it_can() {
    let t = touched(
        r#"SELECT "r"."name" AS label, s.total::numeric FROM "sales"."regions" "r" \
           JOIN sales.summary s ON s."region_id" = "r"."id" \
           WHERE "r"."name" = #{region} AND s.created < CURRENT_TIMESTAMP"#,
    );
    assert_eq!(
        t.tables,
        vec![
            ("sales.regions".into(), Access::Read),
            ("sales.summary".into(), Access::Read)
        ]
    );
    assert_eq!(
        t.bound,
        vec![
            ("sales.regions".to_string(), "id".to_string()),
            ("sales.regions".to_string(), "name".to_string()),
            ("sales.summary".to_string(), "created".to_string()),
            ("sales.summary".to_string(), "region_id".to_string()),
            ("sales.summary".to_string(), "total".to_string()),
        ]
    );
    // Nothing left over: not the quoted alias, not the cast's type, not the
    // output alias, not the niladic function.
    assert!(t.unbound.is_empty(), "{:?}", t.unbound);
}

/// A quoted alias is an alias, not a column — settled before the quoting rule
/// gets to decide anything.
#[test]
fn a_quoted_alias_is_never_a_column() {
    let t = touched(r#"SELECT "r"."id" FROM regions "r""#);
    assert_eq!(t.bound, vec![("regions".to_string(), "id".to_string())]);
    assert!(t.unbound.is_empty(), "{:?}", t.unbound);
}

/// A table hint is not a column: T-SQL writes them bare inside `WITH (…)`,
/// and MySQL after `USE`/`FORCE INDEX`.
#[test]
fn a_table_hint_is_not_a_column() {
    let t = touched("SELECT id FROM orders WITH (NOLOCK) WHERE id = #{id}");
    assert_eq!(t.tables, vec![("orders".into(), Access::Read)]);
    assert_eq!(t.bound, vec![("orders".to_string(), "id".to_string())]);
    assert!(t.unbound.is_empty(), "{:?}", t.unbound);
}
