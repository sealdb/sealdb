/// AnalyzeExec (TiDB `executor/analyze.go`) — count/NDV/histogram/TopN.
/// Supports `ANALYZE TABLE t INDEX idx` for index-column subsets.
use crate::internal::exec::Executor;
use crate::table_reader::scan_table;
use sealdb_domain::Domain;
use sealdb_errno::{ERR_NO_DB, ERR_NO_SUCH_TABLE, Error, Result};
use sealdb_kv::{LockWait, RowLock, TxnHandle};
use sealdb_meta::TableInfo;
use sealdb_parser::ast::Statement;
use sealdb_statistics::{ColumnStats, Histogram, IndexHistogram, IndexStats, Table, TopN, datum_key};
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
        let indexes = analyze.indexes.clone();
        let buckets = analyze.bucket_count;
        let tables: Vec<_> = analyze.all_tables().into_iter().cloned().collect();
        for table in &tables {
            self.analyze_one(table, &indexes, buckets)?;
        }
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        Ok(())
    }

    fn schema(&self) -> &[String] {
        &[]
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        Ok(None)
    }
}

impl AnalyzeExec {
    fn analyze_one(
        &self,
        table: &sealdb_parser::ast::TableName,
        indexes: &[String],
        buckets: Option<u64>,
    ) -> Result<()> {
        let (schema, name) = table.qualified(&self.current_db);
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
        let index_cols = resolve_index_columns(&info, indexes)?;
        let columns = collect_column_stats(&info, &rows, index_cols.as_ref(), buckets);
        let index_stats = collect_index_stats(&info, &rows, buckets);
        // TiDB keeps a row count per physical table id in `mysql.stats_meta`; a
        // partition's `DATA_LENGTH` reads its own count, not the table total.
        let part_counts: Vec<(i64, u64)> = if info.partition.is_some() {
            let mut counts: Vec<(i64, u64)> = Vec::new();
            for row in &rows {
                match counts.iter_mut().find(|(id, _)| *id == row.physical_id) {
                    Some((_, count)) => *count += 1,
                    None => counts.push((row.physical_id, 1)),
                }
            }
            counts
        } else {
            Vec::new()
        };
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
                // A plain index keys off its column's statistics, but an
                // expression index has no column of its own: keep the encoded
                // key histogram collected above.
                for (iname, istats) in index_stats {
                    if let Some((_, slot)) = prev
                        .indices
                        .iter_mut()
                        .find(|(n, _)| n.eq_ignore_ascii_case(&iname))
                    {
                        *slot = istats;
                    } else {
                        prev.indices.push((iname, istats));
                    }
                }
                prev
            } else {
                let mut indices = prev.indices;
                for (iname, istats) in index_stats {
                    if let Some((_, slot)) = indices
                        .iter_mut()
                        .find(|(n, _)| n.eq_ignore_ascii_case(&iname))
                    {
                        *slot = istats;
                    } else {
                        indices.push((iname, istats));
                    }
                }
                Table {
                    count: rows.len() as u64,
                    modify_count: 0,
                    columns,
                    indices,
                    part_counts,
                    version: prev.version + 1,
                }
            }
        } else {
            Table {
                count: rows.len() as u64,
                modify_count: 0,
                columns,
                indices: index_stats,
                part_counts,
                version: 1,
            }
        };
        self.domain.stats().update(&schema, &name, stats);
        self.domain.persist_stats()?;
        Ok(())
    }
}

fn collect_index_stats(
    info: &TableInfo,
    rows: &[crate::table_reader::ScannedRow],
    buckets: Option<u64>,
) -> Vec<(String, IndexStats)> {
    let max_buckets = buckets
        .map(|n| n as usize)
        .unwrap_or(Table::DEFAULT_HIST_BUCKETS);
    let mut out = Vec::new();
    for index in &info.indices {
        // A plain index is estimated from its key column's statistics; only an
        // expression key part (a hidden generated column) needs a histogram of
        // its own, which TiDB keeps as the index's encoded-key statistics.
        let mut parts = Vec::with_capacity(index.columns.len());
        let mut expression = false;
        for part in &index.columns {
            let Some(pos) = info
                .columns
                .iter()
                .position(|c| c.name.o().eq_ignore_ascii_case(part.name.o()))
            else {
                break;
            };
            expression |= info.columns[pos].hidden;
            parts.push(pos);
        }
        if !expression || parts.len() != index.columns.len() || rows.is_empty() {
            continue;
        }
        let mut freq: HashMap<Vec<u8>, u64> = HashMap::new();
        for row in rows {
            let mut key = Vec::new();
            for &pos in &parts {
                let field_type = info.columns[pos].field_type.clone();
                let value = row.values.get(pos).unwrap_or(&Datum::Null);
                match sealdb_statistics::range_est::encode_index_key(&field_type, value) {
                    Some(encoded) => key.extend_from_slice(&encoded),
                    None => {
                        key.clear();
                        break;
                    }
                }
            }
            if key.is_empty() {
                continue;
            }
            *freq.entry(key).or_default() += 1;
        }
        if freq.is_empty() {
            continue;
        }
        // TiDB `BuildHistAndTopN`: the TopN keeps the most frequent keys, the
        // histogram holds the rest. When the TopN covers every distinct key
        // (`haveAllNDV`) no bucket is built and every estimate comes from TopN.
        let mut counts: Vec<(Vec<u8>, u64)> = freq.into_iter().collect();
        counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let ndv = counts.len() as u64;
        let (topn, histogram) = if counts.len() <= Table::DEFAULT_TOPN {
            (
                counts,
                IndexHistogram {
                    ndv,
                    buckets: Vec::new(),
                },
            )
        } else {
            let rest = counts.split_off(Table::DEFAULT_TOPN);
            let mut keys: Vec<Vec<u8>> = Vec::new();
            for (key, count) in &rest {
                for _ in 0..*count {
                    keys.push(key.clone());
                }
            }
            keys.sort();
            (
                counts,
                IndexHistogram::from_sorted_keys(&keys, ndv, max_buckets),
            )
        };
        out.push((
            index.name.o().to_string(),
            IndexStats {
                ndv,
                null_count: 0,
                histogram: Some(histogram),
                topn,
                cm_sketch: None,
                // Version 2 keeps the TopN rows out of the histogram, matching
                // `IndexStats::total_row_count`.
                stats_ver: 2,
            },
        ));
    }
    out
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
    buckets: Option<u64>,
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
        // TiDB `stats_histograms.tot_col_size`: bytes one value takes in a row,
        // a varint length byte plus the content, summed per physical table id.
        // Only variable-length columns consume it (`StorageLength` wins for
        // fixed types), but the collector records it for every column.
        let var_len = col.field_type.storage_length().is_none();
        let mut tot_col_size = 0u64;
        let mut part_tot_col_size: Vec<(i64, u64)> = Vec::new();
        for row in rows {
            let value = row.values.get(idx).unwrap_or(&Datum::Null);
            if var_len {
                let size = stored_value_size(value);
                tot_col_size += size;
                match part_tot_col_size
                    .iter_mut()
                    .find(|(id, _)| *id == row.physical_id)
                {
                    Some((_, total)) => *total += size,
                    None => part_tot_col_size.push((row.physical_id, size)),
                }
            }
            match value {
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
        let max_buckets = buckets
            .map(|n| n as usize)
            .unwrap_or(Table::DEFAULT_HIST_BUCKETS);
        let counts: Vec<(String, u64)> = freq.into_iter().collect();
        // TiDB `BuildHistAndTopN`: when the TopN already holds every distinct
        // value (`haveAllNDV`) no bucket is built, so the histogram keeps only
        // the NDV/null metadata and every estimate comes from the TopN rows
        // (`analyze table t all columns with 1 buckets` over 4 distinct values).
        let histogram = Some(if distinct.len() <= Table::DEFAULT_TOPN {
            Histogram {
                ndv: distinct.len() as u64,
                buckets: Vec::new(),
            }
        } else {
            Histogram::from_sorted(&values, max_buckets)
        });
        let topn = Some(TopN::from_counts(&counts, Table::DEFAULT_TOPN));
        // Planner selectivity for partition pruning needs exact counts; keep
        // them only for narrow columns.
        let value_counts = (distinct.len() <= Table::MAX_VALUE_COUNTS).then(|| {
            let mut exact = counts.clone();
            exact.sort();
            exact
        });
        columns.push((
            col.name.o().to_string(),
            ColumnStats {
                ndv: distinct.len() as u64,
                null_count,
                histogram,
                topn,
                value_counts,
                tot_col_size,
                part_tot_col_size,
            },
        ));
    }
    columns
}

/// TiDB's per-value row size for `tot_col_size`: a varint length byte plus the
/// value content (TiDB `update_test` documents `2 + 1` for `'ss'`).
fn stored_value_size(value: &Datum) -> u64 {
    match value {
        Datum::Null => 0,
        Datum::Bytes(bytes) => bytes.len() as u64 + 1,
        Datum::I64(_) | Datum::U64(_) | Datum::F64(_) => 9,
        Datum::Time(_) => 1 + 8,
        other => datum_key(other).len() as u64 + 1,
    }
}
