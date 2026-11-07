//! One parsed statement into the [`Change`] it applies.
//!
//! Everything here is a translation, never a judgement: a statement whose
//! effect on the schema's shape this plugin does not model lowers to `None`
//! and is counted, so a thin schema is explained by the report rather than
//! guessed at.

use std::ops::ControlFlow;

use sqlparser::ast::{
    AlterColumnOperation, AlterTableOperation, ColumnDef, ColumnOption, ColumnOptionDef,
    CreateDomain, CreateIndex, CreateTable, CreateView, ObjectName, ObjectType,
    RenameTableNameKind, Statement, TableConstraint, UserDefinedTypeRepresentation,
    visit_relations,
};

use crate::change::{Change, Col, Fk, Name, TypeBody};

/// Every change one statement applies, in order.
///
/// A list rather than one change because `ALTER TABLE` takes several
/// operations in a single statement, and each is a change of its own.
pub fn lower(statement: &Statement) -> Vec<Change> {
    let one = |c: Option<Change>| c.into_iter().collect();
    match statement {
        Statement::CreateTable(ct) => vec![create_table(ct)],
        Statement::AlterTable(at) => {
            let table = name_of(&at.name);
            at.operations
                .iter()
                .flat_map(|op| alter_one(&table, op))
                .collect()
        }
        Statement::CreateView(cv) => vec![create_view(cv)],
        Statement::CreateIndex(ci) => vec![create_index(ci)],
        Statement::CreateType {
            name,
            representation,
        } => vec![Change::CreateType {
            name: name_of(name),
            body: representation.as_ref().map_or(TypeBody::Opaque, type_body),
        }],
        Statement::CreateDomain(cd) => vec![create_domain(cd)],
        Statement::Drop {
            object_type, names, ..
        } => one(drop_objects(*object_type, names)),
        _ => Vec::new(),
    }
}

/// `CREATE TABLE`: its columns, and the keys declared either inline or as
/// table constraints. Both spellings mean the same thing, so both are read.
fn create_table(ct: &CreateTable) -> Change {
    let columns: Vec<Col> = ct.columns.iter().map(column).collect();
    let mut foreign_keys: Vec<Fk> = Vec::new();
    let mut primary_key: Vec<String> = Vec::new();

    for def in &ct.columns {
        for opt in &def.options {
            match &opt.option {
                ColumnOption::ForeignKey(fk) => foreign_keys.push(Fk {
                    // An inline `REFERENCES` names no local column: it is this
                    // one.
                    columns: vec![def.name.value.clone()],
                    table: name_of(&fk.foreign_table),
                    referred: fk
                        .referred_columns
                        .iter()
                        .map(|i| i.value.clone())
                        .collect(),
                    on_delete: fk.on_delete.map(|a| a.to_string()),
                }),
                ColumnOption::PrimaryKey(_) => primary_key.push(def.name.value.clone()),
                _ => {}
            }
        }
    }

    for constraint in &ct.constraints {
        match constraint {
            TableConstraint::ForeignKey(fk) => foreign_keys.push(Fk {
                columns: fk.columns.iter().map(|i| i.value.clone()).collect(),
                table: name_of(&fk.foreign_table),
                referred: fk
                    .referred_columns
                    .iter()
                    .map(|i| i.value.clone())
                    .collect(),
                on_delete: fk.on_delete.map(|a| a.to_string()),
            }),
            TableConstraint::PrimaryKey(pk) => {
                primary_key.extend(pk.columns.iter().map(|c| c.column.expr.to_string()));
            }
            _ => {}
        }
    }

    Change::CreateTable {
        name: name_of(&ct.name),
        columns,
        foreign_keys,
        primary_key,
    }
}

/// One operation of an `ALTER TABLE`, as the changes it makes.
fn alter_one(table: &Name, op: &AlterTableOperation) -> Vec<Change> {
    let table = table.clone();
    match op {
        AlterTableOperation::AddColumn { column_def, .. } => vec![Change::AddColumn {
            table,
            column: column(column_def),
        }],
        AlterTableOperation::DropColumn { column_names, .. } => vec![Change::DropColumn {
            table,
            columns: column_names.iter().map(|i| i.value.clone()).collect(),
        }],
        AlterTableOperation::RenameColumn {
            old_column_name,
            new_column_name,
        } => vec![Change::RenameColumn {
            table,
            from: old_column_name.value.clone(),
            to: new_column_name.value.clone(),
        }],
        // `RENAME TO` and `RENAME AS` are the same rename spelled two ways.
        AlterTableOperation::RenameTable { table_name } => {
            let to = match table_name {
                RenameTableNameKind::As(name) | RenameTableNameKind::To(name) => name,
            };
            vec![Change::RenameTable {
                from: table,
                to: name_of(to),
            }]
        }
        AlterTableOperation::AddConstraint {
            constraint: TableConstraint::ForeignKey(fk),
            ..
        } => vec![Change::AddForeignKey {
            table,
            fk: Fk {
                columns: fk.columns.iter().map(|i| i.value.clone()).collect(),
                table: name_of(&fk.foreign_table),
                referred: fk
                    .referred_columns
                    .iter()
                    .map(|i| i.value.clone())
                    .collect(),
                on_delete: fk.on_delete.map(|a| a.to_string()),
            },
        }],
        AlterTableOperation::AlterColumn { column_name, op } => {
            let (ty, not_null) = match op {
                AlterColumnOperation::SetNotNull => (None, Some(true)),
                AlterColumnOperation::DropNotNull => (None, Some(false)),
                AlterColumnOperation::SetDataType { data_type, .. } => {
                    (Some(data_type.to_string()), None)
                }
                _ => return Vec::new(),
            };
            vec![Change::AlterColumn {
                table,
                column: column_name.value.clone(),
                ty,
                not_null,
            }]
        }
        // MySQL's `MODIFY` restates the whole column definition, so the new
        // type is the one fact worth taking from it.
        AlterTableOperation::ModifyColumn {
            col_name,
            data_type,
            ..
        } => vec![Change::AlterColumn {
            table,
            column: col_name.value.clone(),
            ty: Some(data_type.to_string()),
            not_null: None,
        }],
        // MySQL's `CHANGE` may rename *and* retype in one operation, so it
        // lowers to both — the rename first, or the retype would name a
        // column that no longer exists.
        AlterTableOperation::ChangeColumn {
            old_name,
            new_name,
            data_type,
            ..
        } => {
            let mut out = Vec::new();
            if old_name.value != new_name.value {
                out.push(Change::RenameColumn {
                    table: table.clone(),
                    from: old_name.value.clone(),
                    to: new_name.value.clone(),
                });
            }
            out.push(Change::AlterColumn {
                table,
                column: new_name.value.clone(),
                ty: Some(data_type.to_string()),
                not_null: None,
            });
            out
        }
        _ => Vec::new(),
    }
}

/// `CREATE VIEW`: its name, the columns it names, and what it reads.
///
/// The read set comes from sqlparser's relation visitor rather than a walk of
/// the query tree written here, so a view defined over a CTE, a subquery or a
/// union is read as completely as one over a plain `FROM`.
fn create_view(cv: &CreateView) -> Change {
    let mut reads: Vec<Name> = Vec::new();
    let _: ControlFlow<()> = visit_relations(&*cv.query, |name| {
        let name = name_of(name);
        if !reads.iter().any(|seen| seen == &name) {
            reads.push(name);
        }
        ControlFlow::Continue(())
    });
    Change::CreateView {
        name: name_of(&cv.name),
        columns: cv.columns.iter().map(|c| c.name.value.clone()).collect(),
        reads,
        materialized: cv.materialized,
    }
}

/// `CREATE INDEX`. The indexed columns are expressions in general — a
/// functional index over `lower(email)` is one — and are kept as written.
fn create_index(ci: &CreateIndex) -> Change {
    Change::CreateIndex {
        name: ci.name.as_ref().map(name_of),
        table: name_of(&ci.table_name),
        columns: ci
            .columns
            .iter()
            .map(|c| c.column.expr.to_string())
            .collect(),
        unique: ci.unique,
    }
}

/// `CREATE DOMAIN` — a named type with a base type, which Postgres keeps
/// apart from `CREATE TYPE` while the graph has no reason to.
fn create_domain(cd: &CreateDomain) -> Change {
    Change::CreateType {
        name: name_of(&cd.name),
        body: TypeBody::Domain(cd.data_type.to_string()),
    }
}

/// `DROP`, for the four object kinds this plugin holds.
fn drop_objects(kind: ObjectType, names: &[ObjectName]) -> Option<Change> {
    let names: Vec<Name> = names.iter().map(name_of).collect();
    match kind {
        ObjectType::Table => Some(Change::DropTable(names)),
        ObjectType::View | ObjectType::MaterializedView => Some(Change::DropView(names)),
        ObjectType::Type => Some(Change::DropType(names)),
        ObjectType::Index => Some(Change::DropIndex(names)),
        _ => None,
    }
}

/// What a `CREATE TYPE` representation is made of.
fn type_body(repr: &UserDefinedTypeRepresentation) -> TypeBody {
    match repr {
        UserDefinedTypeRepresentation::Enum { labels } => {
            TypeBody::Enum(labels.iter().map(|l| l.value.clone()).collect())
        }
        UserDefinedTypeRepresentation::Composite { attributes } => TypeBody::Composite(
            attributes
                .iter()
                .map(|a| Col {
                    name: a.name.value.clone(),
                    ty: a.data_type.to_string(),
                    not_null: false,
                    primary_key: false,
                    unique: false,
                    default: None,
                })
                .collect(),
        ),
        _ => TypeBody::Opaque,
    }
}

/// One column definition, with the facts a reader of the graph would ask for.
fn column(def: &ColumnDef) -> Col {
    let has = |want: fn(&ColumnOptionDef) -> bool| def.options.iter().any(want);
    Col {
        name: def.name.value.clone(),
        ty: def.data_type.to_string(),
        not_null: has(|o| matches!(o.option, ColumnOption::NotNull))
            || has(|o| matches!(o.option, ColumnOption::PrimaryKey(_))),
        primary_key: has(|o| matches!(o.option, ColumnOption::PrimaryKey(_))),
        unique: has(|o| matches!(o.option, ColumnOption::Unique(_))),
        default: def.options.iter().find_map(|o| match &o.option {
            ColumnOption::Default(e) => Some(e.to_string()),
            _ => None,
        }),
    }
}

/// An `ObjectName` as a schema and a name.
///
/// More than two parts means a database or catalog qualifier in front; the
/// last two are the pair that identifies the object, and the rest would make
/// the key differ between two migrations that qualify differently.
pub fn name_of(name: &ObjectName) -> Name {
    let parts: Vec<String> = name.0.iter().map(|p| p.to_string()).collect();
    let unquote = |s: &String| s.trim_matches(['"', '`', '[', ']']).to_string();
    match parts.len() {
        0 => Name::default(),
        1 => Name {
            schema: None,
            name: unquote(&parts[0]),
        },
        n => Name {
            schema: Some(unquote(&parts[n - 2])),
            name: unquote(&parts[n - 1]),
        },
    }
}
