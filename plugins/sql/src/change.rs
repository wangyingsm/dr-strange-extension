//! What one DDL statement does, in a form that crosses the phase boundary.
//!
//! `parse` runs per chunk and in parallel, `assemble` once — and the replay
//! that turns a sequence of migrations into one schema has to happen in
//! `assemble`, where the whole sequence is in hand. So the expensive half
//! (lexing and parsing SQL) stays in `parse`, and what crosses between them is
//! this: the *effect* of each statement, already extracted from the AST.
//!
//! sqlparser's AST is not serializable without its `serde` feature, and
//! shipping the raw text across instead would move every parse into the one
//! serial phase. This type is the reason neither is necessary.

use serde::{Deserialize, Serialize};

/// A name as the DDL wrote it: `users`, or `public.users`.
///
/// The schema is kept only when it was written. Supplying a default — `public`
/// for Postgres, the database for MySQL, `main` for SQLite — would be
/// inventing a key, and the three dialects would each invent a different one.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Name {
    pub schema: Option<String>,
    pub name: String,
}

impl Name {
    /// The node key: the qualified name, exactly as written.
    pub fn key(&self) -> String {
        match &self.schema {
            Some(s) => format!("{s}.{}", self.name),
            None => self.name.clone(),
        }
    }

    /// Whether two names could denote one object — equal, or equal but for a
    /// schema only one of them states. Case-insensitive, because unquoted
    /// identifiers fold in Postgres and compare case-insensitively in MySQL
    /// and SQLite.
    pub fn may_be(&self, other: &Self) -> bool {
        if !self.name.eq_ignore_ascii_case(&other.name) {
            return false;
        }
        match (&self.schema, &other.schema) {
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            _ => true,
        }
    }
}

/// One column, as declared.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Col {
    pub name: String,
    /// The type as written — `text`, `varchar(64)`, `user_role`.
    pub ty: String,
    pub not_null: bool,
    pub primary_key: bool,
    pub unique: bool,
    pub default: Option<String>,
}

/// A foreign key, from columns of the table that declares it.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Fk {
    pub columns: Vec<String>,
    pub table: Name,
    pub referred: Vec<String>,
    pub on_delete: Option<String>,
}

/// What a user-defined type is made of.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum TypeBody {
    /// `CREATE TYPE … AS ENUM (…)`.
    Enum(Vec<String>),
    /// `CREATE TYPE … AS (…)`.
    Composite(Vec<Col>),
    /// `CREATE DOMAIN … AS <type>`.
    Domain(String),
    /// A shape this plugin does not break down — range, or a base type.
    Opaque,
}

/// The effect of one DDL statement.
///
/// Only the statements that change the shape of a schema are here. A statement
/// that does not — `INSERT`, `GRANT`, `SET` — lowers to nothing and is counted
/// as ignored rather than carried.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum Change {
    CreateTable {
        name: Name,
        columns: Vec<Col>,
        foreign_keys: Vec<Fk>,
        primary_key: Vec<String>,
    },
    AddColumn {
        table: Name,
        column: Col,
    },
    DropColumn {
        table: Name,
        columns: Vec<String>,
    },
    RenameColumn {
        table: Name,
        from: String,
        to: String,
    },
    /// `ALTER COLUMN`: whichever of the two facts the statement set.
    AlterColumn {
        table: Name,
        column: String,
        ty: Option<String>,
        not_null: Option<bool>,
    },
    AddForeignKey {
        table: Name,
        fk: Fk,
    },
    RenameTable {
        from: Name,
        to: Name,
    },
    DropTable(Vec<Name>),
    CreateView {
        name: Name,
        columns: Vec<String>,
        /// Tables and views the defining query reads.
        reads: Vec<Name>,
        materialized: bool,
    },
    DropView(Vec<Name>),
    CreateType {
        name: Name,
        body: TypeBody,
    },
    DropType(Vec<Name>),
    CreateIndex {
        /// Absent for an index the dialect lets go unnamed.
        name: Option<Name>,
        table: Name,
        columns: Vec<String>,
        unique: bool,
    },
    DropIndex(Vec<Name>),
}

/// Every change one file applies, with the file that applied it.
///
/// The path travels with the changes because `assemble` has to put the files
/// back in migration order, and the contract says the result must not depend
/// on where the chunk boundaries fell.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct FileChanges {
    pub path: String,
    pub changes: Vec<Change>,
    /// Statements that parsed but change no shape.
    pub ignored: u32,
}
