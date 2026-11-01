use sealdb_errno::Result;
use sealdb_meta::{DBInfo, Mutator, TableInfo};
use std::collections::HashMap;

#[derive(Clone, Debug, Default)]
pub struct InfoSchema {
    dbs: HashMap<String, DBInfo>,
    tables: HashMap<(String, String), TableInfo>,
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

    pub fn schema_meta_version(&self) -> i64 {
        self.schema_version
    }

    pub fn schema_by_name(&self, name: &str) -> Option<&DBInfo> {
        self.dbs.get(&name.to_ascii_lowercase())
    }

    pub fn table_by_name(&self, schema: &str, table: &str) -> Option<&TableInfo> {
        self.tables
            .get(&(schema.to_ascii_lowercase(), table.to_ascii_lowercase()))
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
