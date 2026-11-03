/// AnalyzeExec (TiDB `executor/analyze.go`) — count/NDV/histogram/TopN.
/// Supports `ANALYZE TABLE t INDEX idx` for index-column subsets.
use crate::internal::exec::Executor;
use crate::table_reader::scan_table;
use sealdb_domain::Domain;
use sealdb_errno::{ERR_NO_DB, ERR_NO_SUCH_TABLE, Error, Result};
use sealdb_kv::{LockWait, RowLock, TxnHandle};
use sealdb_meta::TableInfo;
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
        let index_cols = resolve_index_columns(&info, &analyze.indexes)?;
        let columns = collect_column_stats(&info, &rows, index_cols.as_ref());
        let stats = if let Some(mut prev) = self.domain.stats().table(&schema, &name) {
            if index_cols.is_some() {
                prev.count = rows.len() as u64;
                prev.version += 1;
                for (cname, cstats) in columns {
                    if let Some((_, slot)) = prev
                        .columns
                        .iter_mut()
                        .find(|(n, _)| n.eq_ignore_ascii_case(&cname))
                    {
                        *slot = cstats;
                    } else {
                        prev.columns.push((cname, cstats));
                    }
                }
                prev
            } else {
                Table {
                    count: rows.len() as u64,
                    modify_count: 0,
                    columns,
                    version: prev.version + 1,
                }
            }
        } else {
            Table {
                count: rows.len() as u64,
                modify_count: 0,
                columns,
                version: 1,
            }
        };
        self.domain.stats().update(&schema, &name, stats);
        self.domain.persist_stats()?;
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

fn resolve_index_columns(info: &TableInfo, indexes: &[String]) -> Result<Option<HashSet<String>>> {
    if indexes.is_empty() {
        return Ok(None);
    }
    let mut cols = HashSet::new();
    for iname in indexes {
        let Some(index) = info
            .indices
            .iter()
            .find(|idx| idx.name.o().eq_ignore_ascii_case(iname))
        else {
            return Err(Error::new(
                ERR_NO_SUCH_TABLE,
                format!("Index '{iname}' doesn't exist"),
            ));
        };
        for col in &index.columns {
            cols.insert(col.name.o().to_ascii_lowercase());
        }
    }
    Ok(Some(cols))
}

fn collect_column_stats(
    info: &TableInfo,
    rows: &[crate::table_reader::ScannedRow],
    index_cols: Option<&HashSet<String>>,
) -> Vec<(String, ColumnStats)> {
    let mut columns = Vec::new();
    for (idx, col) in info.columns.iter().enumerate() {
        if let Some(wanted) = index_cols
            && !wanted.contains(&col.name.o().to_ascii_lowercase())
        {
            continue;
        }
        let mut distinct = HashSet::new();
        let mut null_count = 0;
        let mut values = Vec::new();
        let mut freq: HashMap<String, u64> = HashMap::new();
        for row in rows {
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
    columns
}
