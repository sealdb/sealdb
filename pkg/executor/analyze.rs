/// AnalyzeExec (TiDB `executor/analyze.go`) — count/NDV/histogram/TopN.
/// Supports `ANALYZE TABLE t INDEX idx` for index-column subsets.
use crate::internal::exec::Executor;
use crate::table_reader::scan_table;
use sealdb_domain::Domain;
use sealdb_errno::{ERR_NO_DB, ERR_NO_SUCH_TABLE, Error, Result};
use sealdb_kv::{LockWait, RowLock, TxnHandle};
use sealdb_meta::TableInfo;
use sealdb_parser::ast::Statement;
use sealdb_statistics::{
    ColumnStats, Histogram, IndexHistogram, IndexStats, Table, TopN, datum_key,
};
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
        let partitions = analyze.partitions.clone();
        let columns = analyze.columns.clone();
        let buckets = analyze.bucket_count;
        let topn = analyze.topn_count;
        let tables: Vec<_> = analyze.all_tables().into_iter().cloned().collect();
        for table in &tables {
            self.analyze_one(table, &indexes, &partitions, &columns, buckets, topn)?;
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
    #[allow(clippy::too_many_arguments)]
    fn analyze_one(
        &self,
        table: &sealdb_parser::ast::TableName,
        indexes: &[String],
        partitions: &[String],
        requested_columns: &[String],
        buckets: Option<u64>,
        topn_size: Option<u64>,
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
        let all_rows = scan_table(
            &self.txn,
            &info,
            RowLock::None,
            LockWait::Block,
            &info.physical_ids(),
        )?;
        let total_count = all_rows.len() as u64;
        let rows = filter_partition_rows(&info, all_rows, partitions)?;
        let count = if partitions.is_empty() {
            rows.len() as u64
        } else {
            total_count
        };
        // TiDB `ANALYZE` statistics version: an index-only analyze cannot
        // migrate a table whose stored version differs from the session's, so
        // it warns and keeps the existing version (only a full analyze can
        // switch it).
        let session_version = self.domain.analyze_version();
        let stored_version = self.domain.table_analyze_version(&schema, &name);
        let incompatible = !indexes.is_empty()
            && stored_version.is_some_and(|version| version != session_version);
        let effective_version = if incompatible {
            stored_version.unwrap_or(session_version)
        } else {
            session_version
        };
        // TiDB's sampling pushdown appends the adjusted sample rate as a Note
        // for whole-table tasks; a partition-scoped analyze task is built
        // without `V2Options`, so it stays silent.
        if effective_version == 2 && partitions.is_empty() {
            // An empty table takes the "assume empty" reason for an index-only
            // analyze; a table that was never analyzed still reads the pseudo
            // row count (10000) and reports the plain ratio.
            let reason = if count == 0 && !indexes.is_empty() {
                "TiDB assumes that the table is empty, use sample-rate=1".to_string()
            } else if count == 0 {
                "use min(1, 110000/10000) as the sample-rate=1".to_string()
            } else {
                let sample = (110_000.0 / count as f64).min(1.0);
                format!("use min(1, 110000/{count}) as the sample-rate={sample:.0}")
            };
            sealdb_expression::note_message(
                1105,
                format!(
                    "Analyze use auto adjusted sample rate 1.000000 for table {schema}.{name}, reason to use this rate is \"{reason}\""
                ),
            );
        }
        if incompatible {
            sealdb_expression::note_warning(
                1105,
                "The analyze version from the session is not compatible with the existing statistics of the table. Use the existing version instead",
            );
        }
        if effective_version == 2 && !indexes.is_empty() {
            sealdb_expression::note_warning(
                1105,
                "The version 2 would collect all statistics not only the selected indexes",
            );
        }
        if effective_version == 1 {
            for index in &info.indices {
                if is_multi_valued_index(&info, index) {
                    sealdb_expression::note_warning(
                        1105,
                        format!(
                            "analyzing multi-valued indexes is not supported, skip {}",
                            index.name.o()
                        ),
                    );
                }
            }
            sealdb_expression::note_warning(
                1681,
                "ANALYZE with tidb_analyze_version=1 is deprecated and will be removed in a future release.",
            );
        }
        let index_cols = resolve_index_columns(&info, indexes)?;
        let skip_types = self.domain.analyze_skip_column_types();
        let analyzed_columns = collect_column_stats(
            &info,
            &rows,
            index_cols.as_ref(),
            requested_columns,
            &skip_types,
            buckets,
            topn_size,
        );
        let columns = analyzed_columns.clone();
        let index_stats = collect_index_stats(
            &info,
            &rows,
            buckets,
            topn_size,
            effective_version == 1,
        );
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
                prev.count = count;
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
                    count,
                    modify_count: 0,
                    columns,
                    indices,
                    part_counts,
                    version: prev.version + 1,
                }
            }
        } else {
            Table {
                count,
                modify_count: 0,
                columns,
                indices: index_stats,
                part_counts,
                version: 1,
            }
        };
        self.domain.stats().update(&schema, &name, stats);
        self.domain.persist_stats()?;
        match stored_version {
            // A full ANALYZE can migrate a table to version 2; an existing
            // version-2 table is never downgraded by a deprecated version-1 run.
            Some(2) => {}
            _ => self
                .domain
                .set_table_analyze_version(&schema, &name, effective_version),
        }
        let job_info = analyze_job_info(&info, &analyzed_columns, buckets, topn_size);
        let partition_name = match partitions {
            [one] => one.as_str(),
            _ => "",
        };
        self.record_analyze_job(&schema, &name, partition_name, &job_info, rows.len() as u64)?;
        Ok(())
    }

    /// Insert the finished task into `mysql.analyze_jobs` (TiDB records one
    /// row per analyzer task; tests read `job_info` back).
    fn record_analyze_job(
        &self,
        schema: &str,
        table: &str,
        partition: &str,
        job_info: &str,
        processed_rows: u64,
    ) -> Result<()> {
        let Some(info) = self
            .domain
            .infoschema()
            .table_by_name("mysql", "analyze_jobs")
            .cloned()
        else {
            return Ok(());
        };
        let handle = self.domain.next_handle(info.id)?;
        let mut values = vec![Datum::Null; info.columns.len()];
        for (pos, column) in info.columns.iter().enumerate() {
            values[pos] = match column.name.l() {
                "id" => Datum::I64(handle),
                "table_schema" => Datum::Bytes(schema.as_bytes().to_vec()),
                "table_name" => Datum::Bytes(table.as_bytes().to_vec()),
                "partition_name" => Datum::Bytes(partition.as_bytes().to_vec()),
                "job_info" => Datum::Bytes(job_info.as_bytes().to_vec()),
                "processed_rows" => Datum::I64(processed_rows as i64),
                "state" => Datum::Bytes(b"finished".to_vec()),
                "fail_reason" => Datum::Null,
                "instance" => Datum::Bytes(Vec::new()),
                "remarks" => Datum::Bytes(Vec::new()),
                _ => Datum::Null,
            };
        }
        let col_ids: Vec<i64> = info.columns.iter().map(|column| column.id).collect();
        let key = sealdb_tablecodec::encode_row_key_with_handle(info.id, handle);
        let row = sealdb_tablecodec::encode_row(&col_ids, &values);
        self.txn.with_txn(|txn| txn.set(key.as_bytes(), &row))?;
        Ok(())
    }
}

/// TiDB `analyze table` task text stored in `mysql.analyze_jobs.job_info`.
fn analyze_job_info(
    info: &TableInfo,
    analyzed_columns: &[(String, ColumnStats)],
    buckets: Option<u64>,
    topn_size: Option<u64>,
) -> String {
    let names: Vec<&str> = analyzed_columns
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    let buckets = buckets.unwrap_or(256);
    let topn = topn_size.unwrap_or(100);
    let mut out = String::from("analyze table ");
    if !info.indices.is_empty() {
        out.push_str("all indexes, ");
    }
    out.push_str("columns ");
    out.push_str(&names.join(", "));
    out.push_str(&format!(
        " with {buckets} buckets, {topn} topn, 1 samplerate"
    ));
    out
}

/// Restrict scanned rows to the named partitions (`ANALYZE TABLE t PARTITION`).
fn filter_partition_rows(
    info: &TableInfo,
    rows: Vec<crate::table_reader::ScannedRow>,
    partitions: &[String],
) -> Result<Vec<crate::table_reader::ScannedRow>> {
    if partitions.is_empty() {
        return Ok(rows);
    }
    let part = info
        .partition
        .as_ref()
        .filter(|part| part.enable)
        .ok_or_else(|| {
            Error::new(
                sealdb_errno::ERR_UNKNOWN_PARTITION,
                format!("Table '{}' is not partitioned", info.name.o()),
            )
        })?;
    let mut ids = HashSet::new();
    for name in partitions {
        let Some(def) = part
            .definitions
            .iter()
            .find(|def| def.name.o().eq_ignore_ascii_case(name))
        else {
            return Err(Error::new(
                sealdb_errno::ERR_UNKNOWN_PARTITION,
                format!("Unknown partition '{name}' in table '{}'", info.name.o()),
            ));
        };
        ids.insert(def.id);
    }
    Ok(rows
        .into_iter()
        .filter(|row| ids.contains(&row.physical_id))
        .collect())
}

fn collect_index_stats(
    info: &TableInfo,
    rows: &[crate::table_reader::ScannedRow],
    buckets: Option<u64>,
    topn_size: Option<u64>,
    skip_multi_valued: bool,
) -> Vec<(String, IndexStats)> {
    let max_buckets = buckets
        .map(|n| n as usize)
        .unwrap_or(Table::DEFAULT_HIST_BUCKETS);
    let mut out = Vec::new();
    for index in &info.indices {
        // Statistics version 1 cannot analyze multi-valued indexes; TiDB skips
        // them with a warning (the caller emits it).
        if skip_multi_valued && is_multi_valued_index(info, index) {
            continue;
        }
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
            // TiDB's version 2 analyze collects statistics for every index, so
            // an index that existed at ANALYZE time is never missing, even when
            // the table (or an expression key) holds no row. SealDB keeps plain
            // index estimates on the key column's histogram; the placeholder
            // entry only records the index is not missing (see
            // `IndexStats::placeholder`).
            if !skip_multi_valued && (rows.is_empty() || parts.len() == index.columns.len()) {
                out.push((
                    index.name.o().to_string(),
                    IndexStats {
                        ndv: 0,
                        null_count: 0,
                        histogram: None,
                        topn: Vec::new(),
                        cm_sketch: None,
                        stats_ver: 2,
                        placeholder: true,
                    },
                ));
            }
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
        let topn_max = topn_size.map(|n| n as usize).unwrap_or(Table::DEFAULT_TOPN);
        let (topn, histogram) = if counts.len() <= topn_max {
            (
                counts,
                IndexHistogram {
                    ndv,
                    buckets: Vec::new(),
                },
            )
        } else {
            let rest = counts.split_off(topn_max);
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
                placeholder: false,
            },
        ));
    }
    out
}

/// A multi-valued index keys on `CAST(<json> AS <type> ARRAY)`. TiDB models a
/// row as one entry per array element, so only IndexMerge can read it.
pub fn is_multi_valued_index(info: &TableInfo, index: &sealdb_meta::IndexInfo) -> bool {
    index.columns.iter().any(|part| {
        info.columns
            .iter()
            .find(|col| col.hidden && col.name.o().eq_ignore_ascii_case(part.name.o()))
            .and_then(|col| col.generated_expr.as_deref())
            .is_some_and(|text| {
                let lower = text.trim().to_ascii_lowercase();
                lower.contains("cast(") && lower.ends_with("array)")
            })
    })
}

fn resolve_index_columns(info: &TableInfo, indexes: &[String]) -> Result<Option<HashSet<String>>> {    if indexes.is_empty() {
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
    requested_columns: &[String],
    skip_types: &[String],
    buckets: Option<u64>,
    topn_size: Option<u64>,
) -> Vec<(String, ColumnStats)> {
    // Columns backing any index are collected even when their type is on the
    // skip list (TiDB keeps prefix-index statistics for TEXT columns).
    let indexed: HashSet<String> = info
        .indices
        .iter()
        .flat_map(|index| index.columns.iter())
        .map(|part| part.name.o().to_ascii_lowercase())
        .collect();
    let requested: HashSet<String> = requested_columns
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect();
    let mut columns = Vec::new();
    for (idx, col) in info.columns.iter().enumerate() {
        let name = col.name.o().to_ascii_lowercase();
        // `ANALYZE … COLUMNS`: TiDB still collects index columns.
        if !requested.is_empty() && !requested.contains(&name) && !indexed.contains(&name) {
            continue;
        }
        if let Some(wanted) = index_cols
            && !wanted.contains(&name)
        {
            continue;
        }
        // Virtual generated columns have no stored values to sample.
        if col.generated_expr.is_some() && !col.generated_stored {
            continue;
        }
        let type_name = col.field_type.type_name();
        let base_type = type_name
            .split('(')
            .next()
            .unwrap_or(type_name.as_str())
            .to_ascii_lowercase();
        if !indexed.contains(&name) && skip_types.iter().any(|tp| *tp == base_type) {
            continue;
        }
        // TiDB collects string statistics under the column collation: a
        // `*_ci` column folds `'a'`/`'A'` into one NDV bucket.
        let ci = col.field_type.eval_type() == sealdb_types::field_type::EvalType::String
            && sealdb_util::collate::CollateMode::from_name(&col.field_type.collate).is_ci();
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
                    let raw = datum_key(other);
                    let key = if ci && matches!(other, Datum::Bytes(_)) {
                        sealdb_types::ci_fold_str(&raw)
                    } else {
                        raw
                    };
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
        let topn_max = topn_size.map(|n| n as usize).unwrap_or(Table::DEFAULT_TOPN);
        let histogram = Some(if distinct.len() <= topn_max {
            Histogram {
                ndv: distinct.len() as u64,
                buckets: Vec::new(),
            }
        } else {
            Histogram::from_sorted(&values, max_buckets)
        });
        let topn = Some(TopN::from_counts(&counts, topn_max));
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
                ci,
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
