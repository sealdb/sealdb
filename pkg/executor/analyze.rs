/// F21: AnalyzeExec (TiDB `executor/analyze.go`) — count/NDV/histogram/TopN.
/// Completeness wave: sampling ratios, indexes, partitions.
use crate::internal::exec::Executor;
use crate::table_reader::scan_table;
use sealdb_domain::Domain;
use sealdb_errno::{ERR_NO_DB, ERR_NO_SUCH_TABLE, Error, Result};
use sealdb_kv::{LockWait, RowLock, TxnHandle};
use sealdb_parser::ast::Statement;
use sealdb_statistics::{ColumnStats, Histogram, Table, TopN, datum_key};
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;
use std::collections::{HashMap, HashSet};
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
        let rows = scan_table(
            &self.txn,
            &info,
            RowLock::None,
            LockWait::Block,
            &info.physical_ids(),
        )?;
        let mut columns = Vec::new();
        for (idx, col) in info.columns.iter().enumerate() {
            let mut distinct = HashSet::new();
            let mut null_count = 0;
            let mut values = Vec::new();
            let mut freq: HashMap<String, u64> = HashMap::new();
            for row in &rows {
                match row.values.get(idx).unwrap_or(&Datum::Null) {
                    Datum::Null => null_count += 1,
                    other => {
                        let key = datum_key(other);
                        distinct.insert(key.clone());
                        values.push(key.clone());
                        *freq.entry(key).or_default() += 1;
                    }
                }
            }
            values.sort();
            let histogram = Some(Histogram::from_sorted(&values, Table::DEFAULT_HIST_BUCKETS));
            let counts: Vec<(String, u64)> = freq.into_iter().collect();
            let topn = Some(TopN::from_counts(&counts, Table::DEFAULT_TOPN));
            columns.push((
                col.name.o().to_string(),
                ColumnStats {
                    ndv: distinct.len() as u64,
                    null_count,
                    histogram,
                    topn,
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
