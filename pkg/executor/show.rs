use crate::internal::exec::Executor;
use sealdb_domain::Domain;
use sealdb_errno::{ERR_BAD_DB, ERR_NO_DB, ERR_NO_SUCH_TABLE, Error, Result};
use sealdb_parser::ast::{ShowStmt, ShowStmtKind, TableName};
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;
use std::sync::Arc;

pub struct ShowExec {
    domain: Arc<Domain>,
    stmt: ShowStmt,
    current_db: String,
    schema: Vec<String>,
    rows: Vec<Vec<Datum>>,
    emitted: bool,
}

impl ShowExec {
    pub fn new(domain: Arc<Domain>, stmt: ShowStmt, current_db: impl Into<String>) -> Self {
        Self {
            domain,
            stmt,
            current_db: current_db.into(),
            schema: Vec::new(),
            rows: Vec::new(),
            emitted: false,
        }
    }
}

impl Executor for ShowExec {
    fn open(&mut self) -> Result<()> {
        let is = self.domain.infoschema();
        match self.stmt.kind {
            ShowStmtKind::Databases => {
                self.schema = vec!["Database".to_string()];
                self.rows = is
                    .all_schemas()
                    .into_iter()
                    .map(|db| vec![Datum::Bytes(db.name.o().as_bytes().to_vec())])
                    .collect();
            }
            ShowStmtKind::Tables => {
                let db = self
                    .stmt
                    .db_name
                    .clone()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| self.current_db.clone());
                if db.is_empty() {
                    return Err(Error::new(ERR_NO_DB, "No database selected"));
                }
                if is.schema_by_name(&db).is_none() {
                    return Err(Error::new(ERR_BAD_DB, format!("Unknown database '{db}'")));
                }
                self.schema = vec![format!("Tables_in_{db}")];
                self.rows = is
                    .schema_tables(&db)
                    .into_iter()
                    .map(|t| vec![Datum::Bytes(t.name.o().as_bytes().to_vec())])
                    .collect();
            }
            ShowStmtKind::Columns => {
                let table = self
                    .stmt
                    .table
                    .as_ref()
                    .ok_or_else(|| Error::new(ERR_NO_SUCH_TABLE, "missing table name"))?;
                let (schema, name) = resolve_table(table, &self.current_db)?;
                let info = is.table_by_name(&schema, &name).ok_or_else(|| {
                    Error::new(
                        ERR_NO_SUCH_TABLE,
                        format!("Table '{schema}.{name}' doesn't exist"),
                    )
                })?;
                self.schema = vec![
                    "Field".to_string(),
                    "Type".to_string(),
                    "Null".to_string(),
                    "Key".to_string(),
                    "Default".to_string(),
                    "Extra".to_string(),
                ];
                self.rows = info
                    .columns
                    .iter()
                    .map(|col| {
                        vec![
                            Datum::Bytes(col.name.o().as_bytes().to_vec()),
                            Datum::Bytes(col.field_type.type_name().into_bytes()),
                            Datum::Bytes(b"YES".to_vec()),
                            Datum::Bytes(Vec::new()),
                            Datum::Null,
                            Datum::Bytes(Vec::new()),
                        ]
                    })
                    .collect();
            }
        }
        Ok(())
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if self.emitted {
            return Ok(None);
        }
        self.emitted = true;
        let mut chunk = Chunk::new(self.schema.len(), self.rows.len());
        for row in self.rows.drain(..) {
            chunk.append_row(row);
        }
        Ok(Some(chunk))
    }

    fn close(&mut self) -> Result<()> {
        Ok(())
    }

    fn schema(&self) -> &[String] {
        &self.schema
    }
}

fn resolve_table(name: &TableName, current_db: &str) -> Result<(String, String)> {
    let (schema, table) = name.qualified(current_db);
    if schema.is_empty() {
        return Err(Error::new(ERR_NO_DB, "No database selected"));
    }
    Ok((schema, table))
}
