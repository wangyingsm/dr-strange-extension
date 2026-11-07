//! The final schema as graph facts.
//!
//! Keys are SQL's own qualified names — `public.users`, `public.users.email`,
//! or `users` and `users.email` where the DDL wrote no schema — because that
//! is what a reader of the migrations calls these things, and the family's
//! promise is to invent no ids.
//!
//! Built-in types do not get a node. A `Type` node for `text` would appear in
//! every schema, carry nothing, and crowd out the types a tree actually
//! defines; the written type rides on the column as a property instead, and
//! `IS_TYPE` points only at a type some migration declares.

use dr_strange_ext::{Output, edge, node};

use crate::change::{Fk, Name, TypeBody};
use crate::schema::Schema;

/// Turn the replayed schema into nodes and edges on `out`.
///
/// Types first: a column's `IS_TYPE` edge is only written for a type this
/// tree declares, and that is decided by looking at the types already found.
pub fn emit(schema: &Schema, out: &mut Output) {
    for path in &schema.applied {
        out.nodes.push(
            node(path, "Migration")
                .described(
                    "path",
                    "the migration file these facts were read from",
                    &**path,
                )
                .build(),
        );
    }
    types(schema, out);
    tables(schema, out);
    views(schema, out);
    indexes(schema, out);
    out.report.notes.extend(schema.notes.iter().cloned());
}

/// The user-defined types, and a composite type's fields.
fn types(schema: &Schema, out: &mut Output) {
    for ty in &schema.types {
        let key = ty.name.key();
        let (kind, labels) = match &ty.body {
            TypeBody::Enum(labels) => ("enum", labels.clone()),
            TypeBody::Composite(_) => ("composite", Vec::new()),
            TypeBody::Domain(_) => ("domain", Vec::new()),
            TypeBody::Opaque => ("other", Vec::new()),
        };
        let mut n = node(&key, "Type")
            .described(
                "kind",
                "enum, composite, domain, or a shape not broken down",
                kind,
            )
            .described("name", "the type's own name, unqualified", &*ty.name.name);
        if let Some(schema_name) = &ty.name.schema {
            n = n.described(
                "schema",
                "the schema the DDL qualified it with",
                &**schema_name,
            );
        }
        if !labels.is_empty() {
            n = n.described("labels", "the permitted values, in declared order", labels);
        }
        if let TypeBody::Domain(base) = &ty.body {
            n = n.described("base", "the type this domain constrains", &**base);
        }
        out.nodes.push(n.build());
        out.edges
            .push(edge(&ty.created_in, "DEFINES", &key).build());

        // A composite type has fields, and they are the same kind of fact as
        // a table's columns, so they are the same label and the same edge.
        if let TypeBody::Composite(attrs) = &ty.body {
            for attr in attrs {
                let field = format!("{key}.{}", attr.name);
                out.nodes.push(
                    node(&field, "Column")
                        .described("type", "the type as written in the DDL", &*attr.ty)
                        .described("name", "the field's own name", &*attr.name)
                        .build(),
                );
                out.edges.push(edge(&key, "HAS_FIELD", &field).build());
                type_edge(schema, &attr.ty, &field, out);
            }
        }
    }
}

/// The tables, their columns, and the foreign keys between them.
fn tables(schema: &Schema, out: &mut Output) {
    for table in &schema.tables {
        let key = table.name.key();
        let mut n = node(&key, "Table")
            .described(
                "name",
                "the table's own name, unqualified",
                &*table.name.name,
            )
            .described(
                "columns",
                "how many columns the migrations left it with",
                table.columns.len() as i64,
            );
        if let Some(schema_name) = &table.name.schema {
            n = n.described(
                "schema",
                "the schema the DDL qualified it with",
                &**schema_name,
            );
        }
        if !table.primary_key.is_empty() {
            n = n.described(
                "primary_key",
                "the key's columns, in declared order",
                table.primary_key.clone(),
            );
        }
        out.nodes.push(n.build());
        out.edges
            .push(edge(&table.created_in, "DEFINES", &key).build());

        for col in &table.columns {
            let ckey = format!("{key}.{}", col.name);
            let mut c = node(&ckey, "Column")
                .described("name", "the column's own name", &*col.name)
                .described("type", "the type as written in the DDL", &*col.ty)
                .described(
                    "nullable",
                    "whether a row may leave it unset",
                    !col.not_null,
                );
            if col.primary_key {
                c = c.described("primary_key", "part of the table's primary key", true);
            }
            if col.unique {
                c = c.described("unique", "declared unique on its own", true);
            }
            if let Some(d) = &col.default {
                c = c.described("default", "the default expression as written", &**d);
            }
            out.nodes.push(c.build());
            out.edges.push(edge(&key, "HAS_FIELD", &ckey).build());
            type_edge(schema, &col.ty, &ckey, out);
        }

        for fk in &table.foreign_keys {
            foreign_key(schema, &key, fk, out);
        }
    }
}

/// One foreign key, column to column wherever the pair can be named.
///
/// `REFERENCES workspaces (id)` names both ends outright. `REFERENCES
/// workspaces` names only the table, and SQL defines that as the referenced
/// table's primary key — so the replayed schema already holds the answer, and
/// resolving it is reading the standard rather than guessing. Each case says
/// which it was in `_resolved_by`. Where the primary key is composite and the
/// key names a different number of columns the two do not line up, and the
/// edge points at the table instead.
fn foreign_key(schema: &Schema, key: &str, fk: &Fk, out: &mut Output) {
    // A target this tree never declares gets no edge at all: an edge onto a
    // key no node declares fails the entire write, not the one edge.
    let Some(target) = resolve(schema, &fk.table) else {
        return;
    };
    // Whichever the DDL named, or the primary key it meant.
    let (referred, why): (&[String], &str) =
        if fk.referred.len() == fk.columns.len() && !fk.referred.is_empty() {
            (&fk.referred, "foreign-key")
        } else if fk.referred.is_empty()
            && let Some(pk) = primary_key_of(schema, &fk.table)
            && pk.len() == fk.columns.len()
        {
            (pk, "foreign-key-primary-key")
        } else {
            (&[], "foreign-key-table")
        };
    let how = match why {
        "foreign-key" => "a FOREIGN KEY naming both columns",
        "foreign-key-primary-key" => {
            "a FOREIGN KEY naming only the table, which SQL defines as its primary key"
        }
        _ => "a FOREIGN KEY whose columns could not be paired, so it points at the table",
    };

    for (i, from) in fk.columns.iter().enumerate() {
        let src = format!("{key}.{from}");
        let mut e = match referred.get(i) {
            Some(to) if has_column(schema, &fk.table, to) => edge(
                src,
                "REFERENCES",
                format!("{target}.{to}"),
            )
            .described("_resolved_by", how, why),
            _ => edge(src, "REFERENCES", target.clone()).described(
                "_resolved_by",
                "a FOREIGN KEY whose columns could not be paired, so it points at the table",
                "foreign-key-table",
            ),
        };
        if let Some(action) = &fk.on_delete {
            e = e.described(
                "on_delete",
                "what happens to this row when the target goes",
                &**action,
            );
        }
        out.edges.push(e.build());
    }
}

/// The views, their named columns, and what their queries read.
fn views(schema: &Schema, out: &mut Output) {
    for view in &schema.views {
        let key = view.name.key();
        let mut n = node(&key, "View")
            .described("name", "the view's own name, unqualified", &*view.name.name)
            .described(
                "materialized",
                "whether the dialect stores its rows",
                view.materialized,
            );
        if let Some(schema_name) = &view.name.schema {
            n = n.described(
                "schema",
                "the schema the DDL qualified it with",
                &**schema_name,
            );
        }
        out.nodes.push(n.build());
        out.edges
            .push(edge(&view.created_in, "DEFINES", &key).build());

        for col in &view.columns {
            let ckey = format!("{key}.{col}");
            out.nodes.push(
                node(&ckey, "Column")
                    .described("name", "the column's own name", &**col)
                    .build(),
            );
            out.edges.push(edge(&key, "HAS_FIELD", &ckey).build());
        }

        // What the defining query reads. A name the tree never creates is
        // skipped rather than minted: an edge onto a key no node declares
        // fails the whole write, and a view over a table from another
        // database is not this tree's fact to assert.
        for read in &view.reads {
            if read.may_be(&view.name) {
                continue;
            }
            if let Some(target) = resolve(schema, read) {
                out.edges.push(
                    edge(&key, "SELECTS_FROM", &target)
                        .described(
                            "_resolved_by",
                            "a relation in the view's defining query",
                            "view-query",
                        )
                        .build(),
                );
            }
        }
    }
}

/// The indexes, each pointing at its table and at every column it covers.
fn indexes(schema: &Schema, out: &mut Output) {
    for index in &schema.indexes {
        let key = index.name.key();
        // The table may have been dropped by a later migration than the one
        // that made the index, and an edge onto a key nothing declares fails
        // the whole write.
        let Some(table) = resolve(schema, &index.table) else {
            continue;
        };
        out.nodes.push(
            node(&key, "Index")
                .described(
                    "name",
                    "the index's own name, unqualified",
                    &*index.name.name,
                )
                .described("unique", "whether it forbids duplicate keys", index.unique)
                .described(
                    "columns",
                    "the indexed columns or expressions, in order",
                    index.columns.clone(),
                )
                .build(),
        );
        out.edges
            .push(edge(&index.created_in, "DEFINES", &key).build());
        out.edges.push(edge(&key, "INDEXES", &table).build());
        for col in &index.columns {
            if has_column(schema, &index.table, col) {
                out.edges
                    .push(edge(&key, "INDEXES", format!("{table}.{col}")).build());
            }
        }
    }
}

/// An `IS_TYPE` edge, when the written type is one the tree declares.
///
/// The type is matched on its bare name as well as a qualified one: a column
/// written `status user_role` and a type created as `public.user_role` are the
/// same type, and only one of the two spellings carries a schema.
fn type_edge(schema: &Schema, written: &str, column_key: &str, out: &mut Output) {
    // The written type may carry a modifier — `varchar(64)`, `numeric(9,2)` —
    // and an array marker. Neither is part of the name.
    let bare = written
        .split('(')
        .next()
        .unwrap_or(written)
        .trim_end_matches("[]")
        .trim();
    let mut hits = schema.types.iter().filter(|t| {
        t.name.name.eq_ignore_ascii_case(bare) || t.name.key().eq_ignore_ascii_case(bare)
    });
    let Some(ty) = hits.next() else {
        return;
    };
    if hits.next().is_some() {
        // Two declared types the written name could mean. Candidates, not a
        // guess — so no edge, and the column keeps its `type` property.
        return;
    }
    out.edges.push(
        edge(column_key, "IS_TYPE", ty.name.key())
            .described(
                "_resolved_by",
                "the column's written type names a type this tree declares",
                "declared-type",
            )
            .build(),
    );
}

/// The primary key of the table this name denotes, if it declares one.
fn primary_key_of<'a>(schema: &'a Schema, table: &Name) -> Option<&'a [String]> {
    schema
        .tables
        .iter()
        .find(|t| t.name.may_be(table))
        .map(|t| t.primary_key.as_slice())
        .filter(|pk| !pk.is_empty())
}

/// Whether `table` has a column by this name in the replayed schema.
fn has_column(schema: &Schema, table: &Name, column: &str) -> bool {
    schema
        .tables
        .iter()
        .filter(|t| t.name.may_be(table))
        .any(|t| {
            t.columns
                .iter()
                .any(|c| c.name.eq_ignore_ascii_case(column))
        })
}

/// The key of the table or view this name denotes, if the tree declares one.
fn resolve(schema: &Schema, name: &Name) -> Option<String> {
    if let Some(t) = schema.tables.iter().find(|t| t.name.may_be(name)) {
        return Some(t.name.key());
    }
    schema
        .views
        .iter()
        .find(|v| v.name.may_be(name))
        .map(|v| v.name.key())
}
