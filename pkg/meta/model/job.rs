use sealdb_parser::ast::{Statement, StmtNode};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SchemaState(pub u8);

pub const STATE_NONE: SchemaState = SchemaState(0);
pub const STATE_DELETE_ONLY: SchemaState = SchemaState(1);
pub const STATE_WRITE_ONLY: SchemaState = SchemaState(2);
pub const STATE_WRITE_REORGANIZATION: SchemaState = SchemaState(3);
pub const STATE_DELETE_REORGANIZATION: SchemaState = SchemaState(4);
pub const STATE_PUBLIC: SchemaState = SchemaState(5);

impl SchemaState {
    pub fn as_str(self) -> &'static str {
        match self.0 {
            1 => "delete only",
            2 => "write only",
            3 => "write reorganization",
            4 => "delete reorganization",
            5 => "public",
            _ => "none",
        }
    }
}

/// TiDB `model.JobState` for `ADMIN SHOW DDL JOBS`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum JobState {
    #[default]
    None,
    Running,
    Rollingback,
    RollbackDone,
    Done,
    Cancelled,
    Synced,
    Cancelling,
    Queueing,
    Paused,
    Pausing,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Running => "running",
            Self::Rollingback => "rollingback",
            Self::RollbackDone => "rollback done",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
            Self::Synced => "synced",
            Self::Cancelling => "cancelling",
            Self::Queueing => "queueing",
            Self::Paused => "paused",
            Self::Pausing => "pausing",
        }
    }
}

/// TiDB `model.Job` (id, type, schema/table, state). History is in-process;
/// the async job queue / reorg stay later leftovers.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub id: i64,
    pub tp: String,
    pub schema_name: String,
    pub table_name: String,
    pub state: JobState,
    pub schema_state: SchemaState,
}

impl Job {
    /// Build a running job from a DDL statement; callers flip `state` after apply.
    pub fn from_stmt(id: i64, stmt: &Statement, current_db: &str) -> Self {
        let (tp, schema_name, table_name) = match stmt {
            Statement::CreateDatabase(s) => ("create schema", s.name.clone(), String::new()),
            Statement::DropDatabase(s) => ("drop schema", s.name.clone(), String::new()),
            Statement::CreateTable(s) => {
                let (db, table) = s.table.qualified(current_db);
                ("create table", db, table)
            }
            Statement::CreateView(s) => {
                let (db, table) = s.view_name.qualified(current_db);
                ("create view", db, table)
            }
            Statement::DropTable(s) => {
                let (db, table) = s
                    .tables
                    .first()
                    .map(|t| t.qualified(current_db))
                    .unwrap_or_else(|| (current_db.to_string(), String::new()));
                if s.is_view {
                    ("drop view", db, table)
                } else {
                    ("drop table", db, table)
                }
            }
            Statement::TruncateTable(s) => {
                let (db, table) = s.table.qualified(current_db);
                ("truncate table", db, table)
            }
            Statement::CreateIndex(s) => {
                let (db, table) = s.table.qualified(current_db);
                ("add index", db, table)
            }
            Statement::DropIndex(s) => {
                let (db, table) = s.table.qualified(current_db);
                ("drop index", db, table)
            }
            Statement::AlterTable(s) => {
                let (db, table) = s.table.qualified(current_db);
                ("alter table", db, table)
            }
            other => ("ddl", current_db.to_string(), other.stmt_type().to_string()),
        };
        Self {
            id,
            tp: tp.to_string(),
            schema_name,
            table_name,
            state: JobState::Running,
            schema_state: STATE_PUBLIC,
        }
    }
}
