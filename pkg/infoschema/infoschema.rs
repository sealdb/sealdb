use sealdb_errno::Result;
use sealdb_meta::{DBInfo, Mutator, TableInfo};
use std::cell::RefCell;
use std::collections::HashMap;

thread_local! {
    /// Session-local temporary tables of the statement currently executing on
    /// this thread. Sessions are pinned to a thread for the duration of a
    /// statement, so executors that rebuild an `InfoSchema` from the domain
    /// still resolve references rewritten to the reserved schema.
    static THREAD_TEMP_TABLES: RefCell<HashMap<(String, String), TableInfo>> =
        RefCell::new(HashMap::new());
}

/// Publish the executing session's temporary tables for `Domain::infoschema`.
pub fn set_thread_temp_tables(tables: HashMap<(String, String), TableInfo>) {
    THREAD_TEMP_TABLES.with(|slot| *slot.borrow_mut() = tables);
}

pub fn clear_thread_temp_tables() {
    THREAD_TEMP_TABLES.with(|slot| slot.borrow_mut().clear());
}

/// One row of `information_schema.processlist`: the session currently running a
/// statement on this thread. Sessions are pinned to a thread per statement, so a
/// thread-local snapshot mirrors TiDB's `ShowProcessList` for the single
/// connection the test harness uses.
#[derive(Clone, Debug, Default)]
pub struct ProcessInfo {
    pub id: u64,
    pub user: String,
    pub host: String,
    pub db: String,
    pub command: String,
    pub time: i64,
    pub state: String,
    pub info: String,
    pub resource_group: String,
}

thread_local! {
    static THREAD_PROCESS: RefCell<Option<ProcessInfo>> = const { RefCell::new(None) };
}

/// Publish the executing session's processlist row for `virtual_rows`.
pub fn set_thread_process(info: ProcessInfo) {
    THREAD_PROCESS.with(|slot| *slot.borrow_mut() = Some(info));
}

pub fn clear_thread_process() {
    THREAD_PROCESS.with(|slot| *slot.borrow_mut() = None);
}

pub fn thread_process() -> Option<ProcessInfo> {
    THREAD_PROCESS.with(|slot| slot.borrow().clone())
}

/// One row of `information_schema.ddl_jobs` (`ADMIN SHOW DDL JOBS` shape).
#[derive(Clone, Debug)]
pub struct DdlJobInfo {
    pub id: i64,
    pub db_name: String,
    pub table_name: String,
    pub job_type: String,
    pub schema_state: String,
    pub state: String,
}

thread_local! {
    /// DDL jobs visible to the statement currently executing on this thread.
    static THREAD_DDL_JOBS: RefCell<Vec<DdlJobInfo>> = const { RefCell::new(Vec::new()) };
}

/// Publish the domain's DDL jobs for `information_schema.ddl_jobs`.
pub fn set_thread_ddl_jobs(jobs: Vec<DdlJobInfo>) {
    THREAD_DDL_JOBS.with(|slot| *slot.borrow_mut() = jobs);
}

pub fn clear_thread_ddl_jobs() {
    THREAD_DDL_JOBS.with(|slot| slot.borrow_mut().clear());
}

pub fn thread_ddl_jobs() -> Vec<DdlJobInfo> {
    THREAD_DDL_JOBS.with(|slot| slot.borrow().clone())
}

/// Merge this thread's session-local temporary tables into `is`.
pub fn apply_thread_temp_tables(is: &mut InfoSchema) {
    THREAD_TEMP_TABLES.with(|slot| {
        let slot = slot.borrow();
        if slot.is_empty() {
            return;
        }
        for (key, info) in slot.iter() {
            is.temp_tables.insert(key.clone(), info.clone());
        }
    });
}

#[derive(Clone, Debug, Default)]
pub struct InfoSchema {
    dbs: HashMap<String, DBInfo>,
    tables: HashMap<(String, String), TableInfo>,
    /// Session-local temporary tables, keyed by their synthetic schema and
    /// table name. They never appear in `dbs` / `tables`, so catalog listings
    /// (`SHOW DATABASES` / `information_schema.schemata`) stay unchanged.
    temp_tables: HashMap<(String, String), TableInfo>,
    /// `mysql.stats_meta` / `mysql.stats_histograms` equivalent, attached by
    /// `Domain::infoschema()` so `information_schema.tables` / `PARTITIONS` can
    /// report `TABLE_ROWS` / `DATA_LENGTH` / `INDEX_LENGTH`.
    stats: Option<std::sync::Arc<sealdb_statistics::handle::Handle>>,
    schema_version: i64,
}

impl InfoSchema {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn from_meta(meta: &Mutator) -> Result<Self> {
        let mut is = Self {
            dbs: HashMap::new(),
            tables: HashMap::new(),
            temp_tables: HashMap::new(),
            stats: None,
            schema_version: meta.get_schema_version()?,
        };
        for db in meta.list_databases()? {
            let db_l = db.name.l().to_string();
            for table in meta.list_tables(db.id)? {
                is.tables
                    .insert((db_l.clone(), table.name.l().to_string()), table);
            }
            is.dbs.insert(db_l, db);
        }
        Ok(is)
    }

    /// Attach the domain statistics handle (TiDB infoschema holds
    /// `TableStats`); `information_schema.tables` reads row/data/index lengths
    /// from it.
    pub fn with_stats(mut self, stats: std::sync::Arc<sealdb_statistics::handle::Handle>) -> Self {
        self.stats = Some(stats);
        self
    }

    pub fn stats(&self) -> Option<&sealdb_statistics::handle::Handle> {
        self.stats.as_deref()
    }

    pub fn schema_meta_version(&self) -> i64 {
        self.schema_version
    }

    pub fn schema_by_name(&self, name: &str) -> Option<&DBInfo> {
        self.dbs.get(&name.to_ascii_lowercase())
    }

    pub fn table_by_name(&self, schema: &str, table: &str) -> Option<&TableInfo> {
        let key = (schema.to_ascii_lowercase(), table.to_ascii_lowercase());
        self.tables.get(&key).or_else(|| self.temp_tables.get(&key))
    }

    /// Look up a session-local temporary table by bare name. Statements that
    /// deliberately ignore temporary tables (`SHOW CREATE VIEW|SEQUENCE`) still
    /// need to know whether one shadows the name: TiDB derives their result
    /// column headers from the session catalog, which does include `TEMPORARY`
    /// tables (e.g. a shadowed `SHOW CREATE SEQUENCE` reports
    /// `Table` / `Create Table` while rendering the permanent sequence DDL).
    pub fn temp_table_by_name(&self, table: &str) -> Option<&TableInfo> {
        let table_l = table.to_ascii_lowercase();
        self.temp_tables
            .iter()
            .find_map(|((_, name), info)| (name == &table_l).then_some(info))
    }

    /// Register a session-local temporary table under `schema` (a reserved
    /// schema name the session rewrites unqualified references to).
    pub fn insert_temp_table(&mut self, schema: &str, info: TableInfo) {
        self.temp_tables.insert(
            (schema.to_ascii_lowercase(), info.name.l().to_string()),
            info,
        );
    }

    pub fn all_schemas(&self) -> Vec<&DBInfo> {
        let mut dbs: Vec<_> = self.dbs.values().collect();
        dbs.sort_by(|a, b| a.name.l().cmp(b.name.l()));
        dbs
    }

    pub fn schema_tables(&self, schema: &str) -> Vec<&TableInfo> {
        let schema_l = schema.to_ascii_lowercase();
        let mut tables: Vec<_> = self
            .tables
            .iter()
            .filter_map(|((db, _), table)| (db == &schema_l).then_some(table))
            .collect();
        tables.sort_by(|a, b| a.name.l().cmp(b.name.l()));
        tables
    }
}
