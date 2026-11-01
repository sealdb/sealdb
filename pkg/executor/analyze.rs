/// F21 core: AnalyzeExec (TiDB `executor/analyze.go`).
/// Completeness wave: sampling, histograms, indexes, partitions, persist.
use crate::internal::exec::Executor;
use crate::table_reader::scan_table;
use sealdb_domain::Domain;
use sealdb_errno::{ERR_NO_DB, ERR_NO_SUCH_TABLE, Error, Result};
use sealdb_kv::TxnHandle;
use sealdb_parser::ast::Statement;
use sealdb_statistics::{ColumnStats, Table};
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;
use std::collections::HashSet;
use std::sync::Arc;

pub struct AnalyzeExec {
    domain: Arc<Domain>,
    stmt: Statement,
    current_db: String,
    txn: Arc<TxnHandle>,
}

impl AnalyzeExec {
    pub fn new(
        domain: Arc<Domain>,
        stmt: Statement,
        current_db: impl Into<String>,
        txn: Arc<TxnHandle>,
    ) -> Self {
        Self {
            domain,
            stmt,
            current_db: current_db.into(),
            txn,
        }
    }
}

impl Executor for AnalyzeExec {
    fn open(&mut self) -> Result<()> {
        let Statement::AnalyzeTable(analyze) = &self.stmt else {
            return Ok(());
        };
        let (schema, name) = analyze.table.qualified(&self.current_db);
        if schema.is_empty() {
            return Err(Error::new(ERR_NO_DB, "No database selected"));
        }
        let is = self.domain.infoschema();
        let info = is.table_by_name(&schema, &name).cloned().ok_or_else(|| {
            Error::new(
                ERR_NO_SUCH_TABLE,
                format!("Table '{schema}.{name}' doesn't exist"),
            )
        })?;
        let rows = scan_table(&self.txn, &info, false, &info.physical_ids())?;
        let mut columns = Vec::new();
        for (idx, col) in info.columns.iter().enumerate() {
            let mut distinct = HashSet::new();
            let mut null_count = 0;
            for row in &rows {
                match row.values.get(idx).unwrap_or(&Datum::Null) {
                    Datum::Null => null_count += 1,
                    other => {
                        distinct.insert(format!("{other:?}"));
                    }
                }
            }
            columns.push((
                col.name.o().to_string(),
                ColumnStats {
                    ndv: distinct.len() as u64,
                    null_count,
                },
            ));
        }
        self.domain.stats().update(
            schema,
            name,
            Table {
                count: rows.len() as u64,
                columns,
                version: 1,
            },
        );
        Ok(())
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        Ok(None)
    }

    fn close(&mut self) -> Result<()> {
        Ok(())
    }

    fn schema(&self) -> &[String] {
        &[]
    }
}
