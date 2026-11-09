// core path: IndexLookUpExecutor / covering IndexReader
// (TiDB pkg/executor/distsql.go).
// IndexMerge MVB: OR of indexed equalities → union handles + table lookup.
// Completeness wave / not this feature: lookup push-down, parallel workers,
// composite prefix ranges, IndexMerge intersection.
use crate::coprocessor::filter_rows;
use crate::internal::exec::Executor;
use sealdb_domain::Domain;
use sealdb_errno::{ERR_NO_DB, ERR_NO_SUCH_TABLE, Error, Result};
use sealdb_kv::{Key, LockWait, RowLock, TxnHandle};
use sealdb_meta::{IndexInfo, TableInfo};
use sealdb_parser::ast::{ExprNode, TableName};
use sealdb_tablecodec::{
    decode_handle_from_index_key, decode_handle_from_row_key, decode_index_key_values, decode_row,
    encode_index_seek_key, encode_index_seek_prefix, encode_row_key_with_handle, next_prefix,
};
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;
use std::collections::HashSet;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub enum IndexAccess {
    Equal(Datum),
    Range {
        low: Option<Datum>,
        low_inclusive: bool,
        high: Option<Datum>,
        high_inclusive: bool,
    },
}

pub struct IndexLookUpExecutor {
    domain: Arc<Domain>,
    table: TableName,
    index: String,
    access: IndexAccess,
    current_db: String,
    txn: Arc<TxnHandle>,
    lock: RowLock,
    wait: LockWait,
    partitions: Vec<i64>,
    pushed: Option<ExprNode>,
    covering: bool,
    /// Project TiDB's `_tidb_rowid` (the heap table handle) as an extra column.
    with_rowid: bool,
    schema: Vec<String>,
    rows: Vec<Vec<Datum>>,
    emitted: bool,
}

impl IndexLookUpExecutor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        domain: Arc<Domain>,
        table: TableName,
        index: String,
        access: IndexAccess,
        current_db: impl Into<String>,
        txn: Arc<TxnHandle>,
        lock: RowLock,
        wait: LockWait,
        partitions: Vec<i64>,
        pushed: Option<ExprNode>,
        covering: bool,
        with_rowid: bool,
    ) -> Self {
        Self {
            domain,
            table,
            index,
            access,
            current_db: current_db.into(),
            txn,
            lock,
            wait,
            partitions,
            pushed,
            covering,
            with_rowid,
            schema: Vec::new(),
            rows: Vec::new(),
            emitted: false,
        }
    }
}

impl Executor for IndexLookUpExecutor {
    fn open(&mut self) -> Result<()> {
        let (schema, name) = self.table.qualified(&self.current_db);
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
        let index = info
            .indices
            .iter()
            .find(|idx| idx.name.o().eq_ignore_ascii_case(&self.index))
            .cloned()
            .ok_or_else(|| Error::unsupported(format!("index {}", self.index)))?;
        self.schema = info
            .columns
            .iter()
            .map(|c| c.name.o().to_string())
            .collect();
        let physical = if !self.partitions.is_empty() {
            self.partitions.clone()
        } else if let Some(filter) = self.pushed.as_ref() {
            sealdb_planner::core::prune_partitions(&info, Some(filter))
        } else {
            info.physical_ids()
        };
        let expose_rowid = self.with_rowid && !info.pk_is_handle;
        self.rows = self.txn.with_txn(|txn| {
            let mut rows = Vec::new();
            for physical_id in physical {
                let (start, end) = scan_bounds(physical_id, index.id, &self.access);
                if start.as_bytes() >= end.as_slice() {
                    continue;
                }
                for range in sealdb_store::split_key_ranges_for_scan(start.as_bytes(), &end) {
                    let pairs = txn.scan_locked(&range.start, &range.end, self.lock, self.wait)?;
                    for (key, value) in pairs {
                        let handle = index_handle(&index, &key, &value)?;
                        if self.covering {
                            let mut row = covering_row(&info, &index, &key, handle)?;
                            if expose_rowid {
                                row.push(Datum::I64(handle));
                            }
                            rows.push(row);
                            continue;
                        }
                        let row_key = encode_row_key_with_handle(physical_id, handle);
                        let raw = txn.get_locked(row_key.as_bytes(), self.lock, self.wait)?;
                        if let Some(raw) = raw {
                            let decoded = decode_row(&raw, &info.col_field_types());
                            let mut row: Vec<Datum> = info
                                .columns
                                .iter()
                                .map(|col| {
                                    decoded
                                        .iter()
                                        .find(|(id, _)| *id == col.id)
                                        .map(|(_, v)| v.clone())
                                        .unwrap_or(Datum::Null)
                                })
                                .collect();
                            if expose_rowid {
                                row.push(Datum::I64(handle));
                            }
                            rows.push(row);
                        }
                    }
                }
            }
            Ok(rows)
        })?;
        self.rows = sealdb_expression::with_index_dst_double(|| {
            filter_rows(
                std::mem::take(&mut self.rows),
                &self.schema,
                self.pushed.as_ref(),
                Some(&info),
            )
        })?;
        if expose_rowid {
            self.schema.push("\u{1}_tidb_rowid".to_string());
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

/// IndexMerge: scan each partial path's index (or the table handle range for a
/// clustered-PK arm), merge the row IDs, then look the rows up and filter them.
pub struct IndexMergeExecutor {
    domain: Arc<Domain>,
    table: TableName,
    current_db: String,
    txn: Arc<TxnHandle>,
    lock: RowLock,
    wait: LockWait,
    partitions: Vec<i64>,
    pushed: Option<ExprNode>,
    selection: Option<ExprNode>,
    /// Planner-built partial paths; empty for the legacy union shape that
    /// derives arms from `selection` at execution time.
    paths: Vec<sealdb_planner::core::physical_plans::IndexMergePath>,
    /// Rows read in the query's ORDER BY order (arms provide key order).
    keep_order: bool,
    order_column: Option<String>,
    order_desc: bool,
    /// Limit carried by the merge root (`limit embedded(offset, count)`).
    limit: Option<(u64, u64)>,
    schema: Vec<String>,
    rows: Vec<Vec<Datum>>,
    emitted: bool,
}

impl IndexMergeExecutor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        domain: Arc<Domain>,
        table: TableName,
        current_db: impl Into<String>,
        txn: Arc<TxnHandle>,
        lock: RowLock,
        wait: LockWait,
        partitions: Vec<i64>,
        pushed: Option<ExprNode>,
        selection: Option<ExprNode>,
        paths: Vec<sealdb_planner::core::physical_plans::IndexMergePath>,
        keep_order: bool,
        order_column: Option<String>,
        order_desc: bool,
        limit: Option<(u64, u64)>,
    ) -> Self {
        Self {
            domain,
            table,
            current_db: current_db.into(),
            txn,
            lock,
            wait,
            partitions,
            pushed,
            selection,
            paths,
            keep_order,
            order_column,
            order_desc,
            limit,
            schema: Vec::new(),
            rows: Vec::new(),
            emitted: false,
        }
    }
}

impl Executor for IndexMergeExecutor {
    fn open(&mut self) -> Result<()> {
        let (schema, name) = self.table.qualified(&self.current_db);
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
        self.schema = info
            .columns
            .iter()
            .map(|c| c.name.o().to_string())
            .collect();
        let physical = if !self.partitions.is_empty() {
            self.partitions.clone()
        } else if let Some(filter) = self.pushed.as_ref() {
            sealdb_planner::core::prune_partitions(&info, Some(filter))
        } else {
            info.physical_ids()
        };
        // A clustered-PK arm scans the table rows themselves, so the whole
        // table is the row-ID superset; the WHERE filter below prunes it to the
        // merge's exact result.
        let mut rows: Vec<Vec<Datum>> =
            if self.paths.iter().any(|path| path.table_range) {
                crate::table_reader::scan_table(
                    &self.txn,
                    &info,
                    self.lock,
                    self.wait,
                    &physical,
                )?
                .into_iter()
                .map(|row| row.values)
                .collect()
            } else {
                let arms = if self.paths.is_empty() {
                    merge_access_arms(self.selection.as_ref().or(self.pushed.as_ref()), &info)?
                } else {
                    Vec::new()
                };
                if arms.is_empty() && self.paths.is_empty() {
                    return Err(Error::unsupported("index merge without access path"));
                }
                self.txn.with_txn(|txn| {
                    let mut seen = HashSet::new();
                    let mut handles: Vec<(i64, i64)> = Vec::new();
                    for (index, access) in &arms {
                        collect_bounds(
                            txn,
                            (*index).clone(),
                            std::slice::from_ref(access),
                            &physical,
                            self.lock,
                            self.wait,
                            &mut seen,
                            &mut handles,
                        )?;
                    }
                    for path in &self.paths {
                        if path.table_range {
                            continue;
                        }
                        let Some(index) = info
                            .indices
                            .iter()
                            .find(|index| index.name.o().eq_ignore_ascii_case(&path.index))
                        else {
                            continue;
                        };
                        let bounds: Vec<IndexAccess> = path
                            .ranges
                            .iter()
                            .map(|(low, high)| IndexAccess::Range {
                                low: low.as_ref().map(|(value, _)| value.clone()),
                                low_inclusive: low.as_ref().is_none_or(|(_, inclusive)| *inclusive),
                                high: high.as_ref().map(|(value, _)| value.clone()),
                                high_inclusive: high
                                    .as_ref()
                                    .is_none_or(|(_, inclusive)| *inclusive),
                            })
                            .collect();
                        collect_bounds(
                            txn,
                            index.clone(),
                            &bounds,
                            &physical,
                            self.lock,
                            self.wait,
                            &mut seen,
                            &mut handles,
                        )?;
                    }
                    let mut rows = Vec::with_capacity(handles.len());
                    for (physical_id, handle) in handles {
                        let row_key = encode_row_key_with_handle(physical_id, handle);
                        let raw = txn.get_locked(row_key.as_bytes(), self.lock, self.wait)?;
                        if let Some(raw) = raw {
                            let decoded = decode_row(&raw, &info.col_field_types());
                            rows.push(
                                info.columns
                                    .iter()
                                    .map(|col| {
                                        decoded
                                            .iter()
                                            .find(|(id, _)| *id == col.id)
                                            .map(|(_, v)| v.clone())
                                            .unwrap_or(Datum::Null)
                                    })
                                    .collect(),
                            );
                        }
                    }
                    Ok(rows)
                })?
            };
        self.rows = filter_rows(
            std::mem::take(&mut rows),
            &self.schema,
            self.pushed.as_ref(),
            Some(&info),
        )?;
        // An arm-ordered merge (`keep order:true`) yields the ORDER BY order;
        // a DESC order sorts the merged result backwards.
        if self.keep_order
            && let Some(column) = self.order_column.as_deref()
            && let Some(pos) = self.schema.iter().position(|name| name.eq_ignore_ascii_case(column))
        {
            self.rows.sort_by(|left, right| {
                let ordering = left[pos].compare(&right[pos]);
                if self.order_desc {
                    ordering.reverse()
                } else {
                    ordering
                }
            });
        }
        if let Some((offset, count)) = self.limit {
            self.rows = std::mem::take(&mut self.rows)
                .into_iter()
                .skip(offset as usize)
                .take(count as usize)
                .collect();
        }
        // TiDB charges the partial-scan / table-lookup chunks of the index merge
        // reader against `tidb_mem_quota_query` (error 8175 once it is crossed).
        if std::env::var_os("SEALDB_IM_DEBUG").is_some() {
            eprintln!(
                "[im] table={} rows={} mem={} arms={}",
                self.table.name,
                self.rows.len(),
                sealdb_expression::rows_mem_size(&self.rows),
                self.paths.len()
            );
        }
        sealdb_expression::consume_query_mem(sealdb_expression::rows_mem_size(&self.rows))?;
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

fn merge_access_arms(
    selection: Option<&ExprNode>,
    info: &TableInfo,
) -> Result<Vec<(IndexInfo, IndexAccess)>> {
    let Some(expr) = selection else {
        return Ok(Vec::new());
    };
    let cnf = expr.split_cnf();
    let dnf = if cnf.len() == 1 {
        cnf[0].split_dnf()
    } else {
        // Residual AND wrappers: only treat a single OR conjunct as merge arms.
        return Ok(Vec::new());
    };
    let mut arms = Vec::with_capacity(dnf.len());
    for arm in dnf {
        let Some((column, value)) = arm.eq_column_const() else {
            return Err(Error::unsupported("index merge arm is not column = const"));
        };
        let index = info
            .find_index_covering(&column)
            .cloned()
            .ok_or_else(|| Error::unsupported(format!("index merge missing index on {column}")))?;
        arms.push((index, IndexAccess::Equal(value)));
    }
    Ok(arms)
}

fn scan_bounds(physical_id: i64, index_id: i64, access: &IndexAccess) -> (Key, Vec<u8>) {
    match access {
        IndexAccess::Equal(value) => {
            let start =
                encode_index_seek_prefix(physical_id, index_id, std::slice::from_ref(value));
            let end = next_prefix(start.as_bytes());
            (start, end)
        }
        IndexAccess::Range {
            low,
            low_inclusive,
            high,
            high_inclusive,
        } => {
            let start = match low {
                None => encode_index_seek_key(physical_id, index_id, &[]),
                Some(value) => {
                    let prefix = encode_index_seek_prefix(
                        physical_id,
                        index_id,
                        std::slice::from_ref(value),
                    );
                    if *low_inclusive {
                        prefix
                    } else {
                        Key::from_raw(next_prefix(prefix.as_bytes()))
                    }
                }
            };
            let end = match high {
                None => next_prefix(encode_index_seek_key(physical_id, index_id, &[]).as_bytes()),
                Some(value) => {
                    let prefix = encode_index_seek_prefix(
                        physical_id,
                        index_id,
                        std::slice::from_ref(value),
                    );
                    if *high_inclusive {
                        next_prefix(prefix.as_bytes())
                    } else {
                        prefix.as_bytes().to_vec()
                    }
                }
            };
            (start, end)
        }
    }
}

/// Scan one index over every range bound and collect the row IDs it yields.
#[allow(clippy::too_many_arguments)]
fn collect_bounds(
    txn: &mut dyn sealdb_kv::Transaction,
    index: IndexInfo,
    bounds: &[IndexAccess],
    physical: &[i64],
    lock: RowLock,
    wait: LockWait,
    seen: &mut HashSet<(i64, i64)>,
    handles: &mut Vec<(i64, i64)>,
) -> Result<()> {
    for access in bounds {
        for physical_id in physical {
            let (start, end) = scan_bounds(*physical_id, index.id, access);
            if start.as_bytes() >= end.as_slice() {
                continue;
            }
            for range in sealdb_store::split_key_ranges_for_scan(start.as_bytes(), &end) {
                let pairs = txn.scan_locked(&range.start, &range.end, lock, wait)?;
                for (key, value) in pairs {
                    let handle = index_handle(&index, &key, &value)?;
                    if seen.insert((*physical_id, handle)) {
                        handles.push((*physical_id, handle));
                    }
                }
            }
        }
    }
    Ok(())
}

fn index_handle(index: &IndexInfo, key: &[u8], value: &[u8]) -> Result<i64> {
    if index.unique {
        decode_handle_from_row_key(value)
    } else {
        decode_handle_from_index_key(key)
    }
}

fn covering_row(
    info: &TableInfo,
    index: &IndexInfo,
    key: &[u8],
    handle: i64,
) -> Result<Vec<Datum>> {
    let (values, _) = decode_index_key_values(key, index.columns.len(), index.unique)?;
    let mut row = vec![Datum::Null; info.columns.len()];
    for (col, value) in index.columns.iter().zip(values) {
        if let Some(slot) = row.get_mut(col.offset as usize) {
            // Index keys store Time as display bytes; rehydrate FieldType so
            // TIMESTAMP keeps UTC semantics (do not session_local_to_utc again).
            *slot = rehydrate_index_datum(info, col.offset as usize, value)?;
        }
    }
    if let Some(handle_col) = info.handle_column()
        && let Some(slot) = row.get_mut(handle_col.offset as usize)
        // Index already decoded this column (e.g. KEY(a,b,c) on non-PK table);
        // do not clobber with the artificial/common handle.
        && matches!(slot, Datum::Null)
    {
        *slot = Datum::I64(handle);
    }
    Ok(row)
}

fn rehydrate_index_datum(
    info: &TableInfo,
    offset: usize,
    value: Datum,
) -> Result<Datum> {
    let Some(col) = info.columns.get(offset) else {
        return Ok(value);
    };
    match value {
        Datum::Bytes(b) if col.field_type.tp == sealdb_types::TYPE_TIMESTAMP => {
            let s = String::from_utf8_lossy(&b);
            let mut t = sealdb_types::parse_timestamp_literal(&s)?;
            // Bytes already hold UTC wall clock from storage — do not local→UTC.
            t.tp = sealdb_types::TimeType::Timestamp;
            Ok(Datum::Time(t))
        }
        Datum::Bytes(b)
            if matches!(
                col.field_type.tp,
                sealdb_types::TYPE_DATETIME | sealdb_types::TYPE_DATE
            ) =>
        {
            let s = String::from_utf8_lossy(&b);
            let mut t = if col.field_type.tp == sealdb_types::TYPE_DATE {
                sealdb_types::parse_date_literal(&s)?
            } else {
                sealdb_types::parse_timestamp_literal(&s)?
            };
            t.tp = if col.field_type.tp == sealdb_types::TYPE_DATE {
                sealdb_types::TimeType::Date
            } else {
                sealdb_types::TimeType::Datetime
            };
            Ok(Datum::Time(t))
        }
        Datum::Bytes(b)
            if matches!(
                col.field_type.tp,
                sealdb_types::TYPE_NEW_DECIMAL | sealdb_types::TYPE_DECIMAL
            ) =>
        {
            // Index keys store DECIMAL as its canonical text.
            Ok(Datum::Decimal(sealdb_types::MyDecimal::parse(
                &String::from_utf8_lossy(&b),
            )?))
        }
        Datum::Bytes(b)
            if matches!(
                col.field_type.tp,
                sealdb_types::field_type::TYPE_ENUM | sealdb_types::field_type::TYPE_SET
            ) =>
        {
            let number = col.field_type.enum_set_number(&b);
            Ok(Datum::Enum(sealdb_types::EnumValue { label: b, number }))
        }
        other => Ok(other),
    }
}
