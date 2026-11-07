//! Replaying migrations into the schema they leave behind.
//!
//! This is the whole reason the plugin implements the two-phase contract
//! rather than the `Simple` facade. A migration tree is a sequence: the union
//! of every file describes a schema that never existed at any point in time,
//! with every column that was ever added and none of the drops applied. What
//! the graph should hold is the schema the *last* migration leaves — so the
//! changes are replayed in order, and only the final state is emitted.
//!
//! Replaying also makes the drops self-limiting. A `DROP TABLE` of something
//! this tree never created refers to an object outside it and is counted
//! rather than applied, so a stray teardown script cannot empty the schema.

use crate::change::{Change, Col, Fk, Name, TypeBody};
use crate::migrate;

/// A table, as the migrations left it.
#[derive(Clone, Debug)]
pub struct Table {
    pub name: Name,
    pub columns: Vec<Col>,
    pub foreign_keys: Vec<Fk>,
    pub primary_key: Vec<String>,
    /// The migration that created it.
    pub created_in: String,
}

/// A view, as the migrations left it.
#[derive(Clone, Debug)]
pub struct View {
    pub name: Name,
    pub columns: Vec<String>,
    pub reads: Vec<Name>,
    pub materialized: bool,
    pub created_in: String,
}

/// A user-defined type.
#[derive(Clone, Debug)]
pub struct Type {
    pub name: Name,
    pub body: TypeBody,
    pub created_in: String,
}

/// An index.
#[derive(Clone, Debug)]
pub struct Index {
    pub name: Name,
    pub table: Name,
    pub columns: Vec<String>,
    pub unique: bool,
    pub created_in: String,
}

/// The schema a migration sequence leaves behind, and what could not be
/// applied on the way there.
#[derive(Debug, Default)]
pub struct Schema {
    pub tables: Vec<Table>,
    pub views: Vec<View>,
    pub types: Vec<Type>,
    pub indexes: Vec<Index>,
    /// Every migration that contributed, in the order replayed.
    pub applied: Vec<String>,
    /// What a reader needs to know to explain the shape above.
    pub notes: Vec<String>,
}

impl Schema {
    /// Replay one file's changes. `path` is the migration they came from.
    pub fn apply(&mut self, path: &str, changes: &[Change]) {
        for change in changes {
            self.one(path, change);
        }
    }

    /// One change. Split by what it acts on: the table operations all have to
    /// find their table first and report it when they cannot, where the
    /// object-level ones redefine or remove outright.
    fn one(&mut self, path: &str, change: &Change) {
        match change {
            Change::CreateTable { .. }
            | Change::AddColumn { .. }
            | Change::DropColumn { .. }
            | Change::RenameColumn { .. }
            | Change::AlterColumn { .. }
            | Change::AddForeignKey { .. }
            | Change::RenameTable { .. }
            | Change::DropTable(_) => self.table_change(path, change),
            _ => self.object_change(path, change),
        }
    }

    /// A change to a table or one of its columns.
    fn table_change(&mut self, path: &str, change: &Change) {
        match change {
            Change::CreateTable {
                name,
                columns,
                foreign_keys,
                primary_key,
            } => {
                if self.table_at(name).is_some() {
                    // `CREATE TABLE IF NOT EXISTS` run twice, or a fixture
                    // restating the schema. The first definition stands.
                    return;
                }
                self.tables.push(Table {
                    name: name.clone(),
                    columns: columns.clone(),
                    foreign_keys: foreign_keys.clone(),
                    primary_key: primary_key.clone(),
                    created_in: path.to_string(),
                });
            }
            Change::AddColumn { table, column } => {
                if let Some(t) = self.table_at(table) {
                    let t = &mut self.tables[t];
                    if !t.columns.iter().any(|c| eq(&c.name, &column.name)) {
                        t.columns.push(column.clone());
                    }
                } else {
                    self.unknown(path, "ALTER TABLE … ADD COLUMN", table);
                }
            }
            Change::DropColumn { table, columns } => match self.table_at(table) {
                Some(t) => self.tables[t]
                    .columns
                    .retain(|c| !columns.iter().any(|d| eq(&c.name, d))),
                None => self.unknown(path, "ALTER TABLE … DROP COLUMN", table),
            },
            Change::RenameColumn { table, from, to } => match self.table_at(table) {
                Some(t) => {
                    let renamed = self.tables[t]
                        .columns
                        .iter_mut()
                        .find(|c| eq(&c.name, from))
                        .map(|c| c.name = to.clone())
                        .is_some();
                    if renamed {
                        rename_in_keys(&mut self.tables, table, from, to);
                    }
                }
                None => self.unknown(path, "ALTER TABLE … RENAME COLUMN", table),
            },
            Change::AlterColumn {
                table,
                column,
                ty,
                not_null,
            } => match self.table_at(table) {
                Some(t) => {
                    if let Some(c) = self.tables[t]
                        .columns
                        .iter_mut()
                        .find(|c| eq(&c.name, column))
                    {
                        if let Some(ty) = ty {
                            c.ty = ty.clone();
                        }
                        if let Some(nn) = not_null {
                            c.not_null = *nn;
                        }
                    }
                }
                None => self.unknown(path, "ALTER TABLE … ALTER COLUMN", table),
            },
            Change::AddForeignKey { table, fk } => match self.table_at(table) {
                Some(t) => self.tables[t].foreign_keys.push(fk.clone()),
                None => self.unknown(path, "ALTER TABLE … ADD FOREIGN KEY", table),
            },
            Change::RenameTable { from, to } => match self.table_at(from) {
                Some(t) => {
                    self.tables[t].name = to.clone();
                    // Every foreign key that pointed at the old name now
                    // points at nothing, so they move with it.
                    for table in &mut self.tables {
                        for fk in &mut table.foreign_keys {
                            if fk.table.may_be(from) {
                                fk.table = to.clone();
                            }
                        }
                    }
                }
                None => self.unknown(path, "ALTER TABLE … RENAME TO", from),
            },
            Change::DropTable(names) => {
                for name in names {
                    match self.table_at(name) {
                        Some(t) => {
                            self.tables.remove(t);
                            // An index on a dropped table goes with it,
                            // whether or not the migration says so.
                            self.indexes.retain(|i| !i.table.may_be(name));
                        }
                        None => self.unknown(path, "DROP TABLE", name),
                    }
                }
            }
            _ => {}
        }
    }

    /// A change to a view, a type or an index — things that are redefined or
    /// removed whole rather than altered in place.
    fn object_change(&mut self, path: &str, change: &Change) {
        match change {
            Change::CreateView {
                name,
                columns,
                reads,
                materialized,
            } => {
                // A view is routinely dropped and recreated to change it, and
                // `CREATE OR REPLACE` does it in one statement, so the last
                // definition wins rather than the first.
                self.views.retain(|v| !v.name.may_be(name));
                self.views.push(View {
                    name: name.clone(),
                    columns: columns.clone(),
                    reads: reads.clone(),
                    materialized: *materialized,
                    created_in: path.to_string(),
                });
            }
            Change::DropView(names) => {
                self.views
                    .retain(|v| !names.iter().any(|n| v.name.may_be(n)));
            }
            Change::CreateType { name, body } => {
                self.types.retain(|t| !t.name.may_be(name));
                self.types.push(Type {
                    name: name.clone(),
                    body: body.clone(),
                    created_in: path.to_string(),
                });
            }
            Change::DropType(names) => {
                self.types
                    .retain(|t| !names.iter().any(|n| t.name.may_be(n)));
            }
            Change::CreateIndex {
                name,
                table,
                columns,
                unique,
            } => {
                // An unnamed index is the dialect's to name, and inventing one
                // would key a node on something no migration wrote. Its
                // columns are already on the table, so it is counted instead.
                let Some(name) = name else {
                    self.notes.push(format!(
                        "{path}: an unnamed index on {} — no key to give it, so its \
                         columns are recorded on the table alone",
                        table.key()
                    ));
                    return;
                };
                self.indexes.retain(|i| !i.name.may_be(name));
                self.indexes.push(Index {
                    name: name.clone(),
                    table: table.clone(),
                    columns: columns.clone(),
                    unique: *unique,
                    created_in: path.to_string(),
                });
            }
            Change::DropIndex(names) => {
                self.indexes
                    .retain(|i| !names.iter().any(|n| i.name.may_be(n)));
            }
            _ => {}
        }
    }

    /// Index of the table this name denotes, if exactly one does.
    ///
    /// Two candidates means a tree where `users` and `audit.users` both exist
    /// and a migration named one of them without a schema. Guessing which
    /// would attach a column to the wrong table, so neither is chosen.
    fn table_at(&self, name: &Name) -> Option<usize> {
        let mut hits = self
            .tables
            .iter()
            .enumerate()
            .filter(|(_, t)| t.name.may_be(name));
        let first = hits.next()?;
        match hits.next() {
            None => Some(first.0),
            Some(_) => None,
        }
    }

    /// Record a statement that named something this tree never created.
    ///
    /// Said once per file and object: a migration that adds six columns to a
    /// table whose own `CREATE` failed to parse would otherwise repeat one
    /// sentence six times, and a report nobody reads explains nothing.
    fn unknown(&mut self, path: &str, what: &str, name: &Name) {
        let note = format!(
            "{path}: {what} on `{}`, which no migration in this tree creates — \
             not applied",
            name.key()
        );
        if !self.notes.contains(&note) {
            self.notes.push(note);
        }
    }
}

/// Replay every file, in the order the migration tool would run them.
///
/// Sorted here rather than trusted from the chunking: the contract says a
/// plugin's result must not depend on where the chunk boundaries fell, and
/// migration order is the one thing this plugin cannot get wrong.
pub fn replay(mut files: Vec<crate::change::FileChanges>) -> Schema {
    files.sort_by(|a, b| migrate::migration_order(&a.path, &b.path));
    let mut schema = Schema::default();
    for file in files {
        if file.changes.is_empty() {
            continue;
        }
        schema.applied.push(file.path.clone());
        schema.apply(&file.path, &file.changes);
    }
    schema
}

/// Unquoted SQL identifiers fold in Postgres and compare case-insensitively
/// in MySQL and SQLite, so nothing here is case-sensitive.
fn eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Follow a column rename through the keys that name it.
fn rename_in_keys(tables: &mut [Table], table: &Name, from: &str, to: &str) {
    for t in tables.iter_mut() {
        if t.name.may_be(table) {
            for col in &mut t.primary_key {
                if eq(col, from) {
                    *col = to.to_string();
                }
            }
            for fk in &mut t.foreign_keys {
                for col in &mut fk.columns {
                    if eq(col, from) {
                        *col = to.to_string();
                    }
                }
            }
        }
        // A foreign key in another table referring to the renamed column.
        for fk in &mut t.foreign_keys {
            if fk.table.may_be(table) {
                for col in &mut fk.referred {
                    if eq(col, from) {
                        *col = to.to_string();
                    }
                }
            }
        }
    }
}
