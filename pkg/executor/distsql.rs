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
    /// Point range over the pinned key prefix (`idx = 1`).
    Equal(Vec<Datum>),
    /// Composite range: `low` / `high` are the pinned key parts (empty means
    /// unbounded), so `(c2 = 1, c1 > 1)` seeks `low [1 1]` .. `high [1]`.
    Range {
        low: Vec<Datum>,
        low_inclusive: bool,
        high: Vec<Datum>,
        high_inclusive: bool,
    },
}

pub struct IndexLookUpExecutor {
    domain: Arc<Domain>,
    table: TableName,
    index: String,
    access: Vec<IndexAccess>,
    current_db: String,
    txn: Arc<TxnHandle>,
    lock: RowLock,
    wait: LockWait,
    partitions: Vec<i64>,
    pushed: Option<ExprNode>,
    covering: bool,
    /// The read must return rows in index-key order (`keep order:true`), so the
    /// probe keeps the Build side's handle order instead of batching.
    keep_order: bool,
    /// The read walks one ordered index whose keys live in *different*
    /// partition subtrees (a GLOBAL index read across partitions), so the rows
    /// are merged back into index-key order instead of partition order.
    merge_order: bool,
    /// Project TiDB's `_tidb_rowid` (the heap table handle) as an extra column.
    with_rowid: bool,
    /// Cap how many index keys the Build side collects before the table probe
    /// (`LIMIT` absorbed into IndexLookUp push-down: fetch `offset+count`).
    /// The root `Limit` still applies the original offset/count on the result.
    index_fetch_limit: Option<u64>,
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
        access: Vec<IndexAccess>,
        current_db: impl Into<String>,
        txn: Arc<TxnHandle>,
        lock: RowLock,
        wait: LockWait,
        partitions: Vec<i64>,
        pushed: Option<ExprNode>,
        covering: bool,
        keep_order: bool,
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
            keep_order,
            merge_order: false,
            with_rowid,
            index_fetch_limit: None,
            schema: Vec::new(),
            rows: Vec::new(),
            emitted: false,
        }
    }

    /// Cap the index-side scan to `fetch` keys (TiDB LocalIndexLookUp Limit).
    pub fn with_index_fetch_limit(mut self, fetch: Option<u64>) -> Self {
        self.index_fetch_limit = fetch;
        self
    }

    /// True when the current transaction has buffered writes under any of the
    /// table's physical ids (TiDB's dirty table / UnionScan path).
    fn txn_has_writes(&self, physical_ids: &[i64]) -> bool {
        let dirty = self
            .txn
            .with_txn(|txn| Ok(txn.dirty_keys()))
            .unwrap_or_default();
        dirty.iter().any(|key| {
            physical_ids.iter().any(|id| {
                let prefix = sealdb_tablecodec::encode_table_prefix(*id);
                key.starts_with(prefix.as_bytes())
            })
        })
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
        sealdb_expression::note_binary_columns_from_table(&info);
        let index = info
            .indices
            .iter()
            .find(|idx| idx.name.o().eq_ignore_ascii_case(&self.index))
            .cloned()
            .ok_or_else(|| Error::unsupported(format!("index {}", self.index)))?;
        // An expression key part stores the expression's value, not the plain
        // columns it reads, so a covering read cannot rebuild the row. Fetch
        // the table rows to evaluate the expression (the EXPLAIN shape remains
        // a covering `IndexReader`).
        let expression_index = sealdb_planner::core::expr_index::has_expression_part(&info, &index);
        // `IndexReader` (index-only plan) keeps the index key order even when
        // an expression part forces a table fetch to evaluate the expression;
        // `IndexLookUp` batches its probe and answers in handle order.
        let index_only = self.covering;
        if expression_index {
            self.covering = false;
        }
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
        // TiDB's `TableRowIDScan` probe batches the row ids the Build side
        // produced into one `BatchGet`, which TiKV answers in key order. An
        // unordered lookup therefore returns rows in handle order even when
        // the index arms (`a = 1 or a = 2`) yielded their handles out of order.
        // A dirty table (UnionScan over this txn's own writes) merges the dirty
        // index entries instead and keeps the index key order, so only batch a
        // read with no pending writes under the table.
        // An expression index also batches the lookup: TiDB's `TableRowIDScan`
        // probe answers the fetched rows in handle order even when the Build
        // side scanned an expression index. Index-only reads keep the
        // expression order (`SELECT a+1` over `KEY ((a+1))` yields 0,1,2…)
        // because they never probe the table.
        // Absorbed Limit still batches the Probe (TiDB TableRowIDScan →
        // handle order). Residual Selection is applied while collecting
        // handles so the fetch cap counts only matching rows.
        let probe_batched = !index_only && !self.keep_order && !self.txn_has_writes(&physical);
        // A GLOBAL expression index read spans the partitions, so its keys are
        // stored in one subtree per partition: the scan yields them partition
        // by partition and must be merged back into key order.
        self.merge_order =
            self.keep_order && physical.len() > 1 && (index.global || expression_index);
        let merge_order = self.merge_order;
        // A clustered integer PRIMARY KEY has no separate index entries (the
        // row handle is the key), so `IndexFullScan` on it reads the table
        // rows, just like TiDB's clustered-index coprocessor task.
        let clustered_primary_full = index.primary
            && info.pk_is_handle
            && self.access.iter().all(|access| {
                matches!(
                    access,
                    IndexAccess::Range { low, high, .. } if low.is_empty() && high.is_empty()
                )
            });
        self.rows = if clustered_primary_full {
            crate::table_reader::scan_table(&self.txn, &info, self.lock, self.wait, &physical)?
                .into_iter()
                .map(|row| {
                    if !self.covering {
                        return row.values;
                    }
                    let mut out = vec![Datum::Null; info.columns.len()];
                    for col in &index.columns {
                        if let Some(value) = row.values.get(col.offset as usize) {
                            out[col.offset as usize] = value.clone();
                        }
                    }
                    if expose_rowid {
                        out.push(Datum::I64(row.handle));
                    }
                    out
                })
                .collect()
        } else {
            self.txn.with_txn(|txn| {
                let mut rows = Vec::new();
                let mut keyed: Vec<(Vec<u8>, Vec<Datum>)> = Vec::new();
                let mut pending: Vec<(i64, i64)> = Vec::new();
                let mut fetched = 0u64;
                let fetch_cap = self.index_fetch_limit;
                // Unordered multi-partition LocalIndexLookUp: TiDB runs a
                // per-region cop task (handle order within each partition),
                // then concatenates partitions in id order before the root
                // Limit. Cap per partition so early index-order stop across
                // partitions cannot reshuffle the first N handles.
                let per_partition_cap = probe_batched && physical.len() > 1;
                'scan: for physical_id in physical {
                    let mut part_fetched = 0u64;
                    for access in &self.access {
                        let (start, end) = scan_bounds(physical_id, index.id, access);
                        if start.as_bytes() >= end.as_slice() {
                            continue;
                        }
                        for range in sealdb_store::split_key_ranges_for_scan(start.as_bytes(), &end)
                        {
                            let pairs =
                                txn.scan_locked(&range.start, &range.end, self.lock, self.wait)?;
                            for (key, value) in pairs {
                                if per_partition_cap {
                                    if fetch_cap.is_some_and(|cap| part_fetched >= cap) {
                                        continue 'scan;
                                    }
                                } else if fetch_cap.is_some_and(|cap| fetched >= cap) {
                                    break 'scan;
                                }
                                let handle = index_handle(&index, &key, &value)?;
                                if self.covering {
                                    let mut row = covering_row(&info, &index, &key, handle)?;
                                    if info.common_handle
                                        && common_handle_columns_missing(&info, &row)
                                    {
                                        // TiDB stores a clustered PRIMARY KEY as the
                                        // common handle inside every secondary index key,
                                        // so `KEY idx(b)` alone answers `SELECT a` when `a`
                                        // is part of that key. SealDB's index keys carry the
                                        // heap row id instead, so the covering read fetches
                                        // the row for the handle columns it cannot decode.
                                        let row_key =
                                            encode_row_key_with_handle(physical_id, handle);
                                        if let Some(raw) = txn.get_locked(
                                            row_key.as_bytes(),
                                            self.lock,
                                            self.wait,
                                        )? {
                                            backfill_row(&info, &mut row, &raw);
                                        }
                                    }
                                    if expose_rowid {
                                        row.push(Datum::I64(handle));
                                    }
                                    if merge_order {
                                        keyed.push((index_sort_key(&key), row));
                                    } else {
                                        rows.push(row);
                                    }
                                    fetched = fetched.saturating_add(1);
                                    part_fetched = part_fetched.saturating_add(1);
                                    continue;
                                }
                                // Absorbed Limit + residual Selection: probe to
                                // test the filter before counting toward the cap.
                                if fetch_cap.is_some()
                                    && let Some(filter) = self.pushed.as_ref()
                                {
                                    let row_key = encode_row_key_with_handle(physical_id, handle);
                                    let Some(raw) =
                                        txn.get_locked(row_key.as_bytes(), self.lock, self.wait)?
                                    else {
                                        continue;
                                    };
                                    let row = decode_lookup_row(&info, &raw, expose_rowid, handle);
                                    let schema: Vec<String> = info
                                        .columns
                                        .iter()
                                        .map(|c| c.name.o().to_string())
                                        .collect();
                                    let filtered =
                                        filter_rows(vec![row], &schema, Some(filter), Some(&info))?;
                                    if filtered.is_empty() {
                                        continue;
                                    }
                                    if probe_batched {
                                        pending.push((physical_id, handle));
                                    } else if merge_order {
                                        keyed.push((
                                            index_sort_key(&key),
                                            filtered.into_iter().next().unwrap(),
                                        ));
                                    } else {
                                        rows.push(filtered.into_iter().next().unwrap());
                                    }
                                    fetched = fetched.saturating_add(1);
                                    part_fetched = part_fetched.saturating_add(1);
                                    continue;
                                }
                                if probe_batched {
                                    pending.push((physical_id, handle));
                                    fetched = fetched.saturating_add(1);
                                    part_fetched = part_fetched.saturating_add(1);
                                    continue;
                                }
                                let row_key = encode_row_key_with_handle(physical_id, handle);
                                let raw =
                                    txn.get_locked(row_key.as_bytes(), self.lock, self.wait)?;
                                if let Some(raw) = raw {
                                    let row = decode_lookup_row(&info, &raw, expose_rowid, handle);
                                    if merge_order {
                                        keyed.push((index_sort_key(&key), row));
                                    } else {
                                        rows.push(row);
                                    }
                                    fetched = fetched.saturating_add(1);
                                    part_fetched = part_fetched.saturating_add(1);
                                }
                            }
                        }
                    }
                }
                if merge_order {
                    // Index key order: the key's table/index prefix is the same for
                    // every row, so the encoded key values decide.
                    keyed.sort_by(|left, right| left.0.cmp(&right.0));
                    rows.extend(keyed.into_iter().map(|(_, row)| row));
                }
                if probe_batched {
                    // Key order is the storage order: table id, then the row handle.
                    pending.sort_unstable();
                    if per_partition_cap {
                        if let Some(cap) = fetch_cap {
                            pending.truncate(cap as usize);
                        }
                    }
                    for (physical_id, handle) in pending {
                        let row_key = encode_row_key_with_handle(physical_id, handle);
                        let raw = txn.get_locked(row_key.as_bytes(), self.lock, self.wait)?;
                        if let Some(raw) = raw {
                            rows.push(decode_lookup_row(&info, &raw, expose_rowid, handle));
                        }
                    }
                }
                Ok(rows)
            })?
        };
        // A covering read only has the index's key values: an expression key
        // part answers the expression it stores, so evaluate the pushed filter
        // on the part (`like(md5(s), …)` → `like(_V$_idx_0, …)`).
        let pushed = if self.covering {
            self.pushed.as_ref().map(|filter| {
                sealdb_planner::core::expr_index::substitute_parts_expr(&info, &index, filter)
            })
        } else {
            self.pushed.clone()
        };
        self.rows = sealdb_expression::with_index_dst_double(|| {
            filter_rows(
                std::mem::take(&mut self.rows),
                &self.schema,
                pushed.as_ref(),
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
        let mut rows: Vec<Vec<Datum>> = if self.paths.iter().any(|path| path.table_range) {
            crate::table_reader::scan_table(&self.txn, &info, self.lock, self.wait, &physical)?
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
                // An arm may carry a cop `Selection(Build)` over its index
                // columns (`b NOT LIKE '%test%'` on the third part of
                // `mvi1(c, mvi(j), b)`). TiDB filters the arm's row IDs before
                // merging them, so it has to be applied per arm, not to the
                // merged result.
                let mut arm_rows: Vec<Vec<Datum>> = Vec::new();
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
                    let bounds: Vec<IndexAccess> = match &path.seek {
                        // A composite point key (`[c = 1, element = 4]`): the
                        // element cannot be expressed as one `ranges` datum.
                        Some(seek) => vec![IndexAccess::Range {
                            low: seek.clone(),
                            low_inclusive: true,
                            high: seek.clone(),
                            high_inclusive: true,
                        }],
                        None => path
                            .ranges
                            .iter()
                            .map(|(low, high)| IndexAccess::Range {
                                low: low
                                    .as_ref()
                                    .map(|(value, _)| vec![value.clone()])
                                    .unwrap_or_default(),
                                low_inclusive: low.as_ref().is_none_or(|(_, inclusive)| *inclusive),
                                high: high
                                    .as_ref()
                                    .map(|(value, _)| vec![value.clone()])
                                    .unwrap_or_default(),
                                high_inclusive: high
                                    .as_ref()
                                    .is_none_or(|(_, inclusive)| *inclusive),
                            })
                            .collect(),
                    };
                    let mut arm_handles: Vec<(i64, i64)> = Vec::new();
                    let mut arm_seen = HashSet::new();
                    collect_bounds(
                        txn,
                        index.clone(),
                        &bounds,
                        &physical,
                        self.lock,
                        self.wait,
                        &mut arm_seen,
                        &mut arm_handles,
                    )?;
                    for (physical_id, handle) in arm_handles {
                        let row_key = encode_row_key_with_handle(physical_id, handle);
                        let Some(raw) = txn.get_locked(row_key.as_bytes(), self.lock, self.wait)?
                        else {
                            continue;
                        };
                        let decoded = decode_row(&raw, &info.col_field_types());
                        let row: Vec<Datum> = info
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
                        if let Some(filter) = path.residual_selection.as_ref()
                            && filter_rows(
                                vec![row.clone()],
                                &self.schema,
                                Some(filter),
                                Some(&info),
                            )?
                            .is_empty()
                        {
                            continue;
                        }
                        if seen.insert((physical_id, handle)) {
                            arm_rows.push(row);
                        }
                    }
                }
                let mut rows = Vec::with_capacity(handles.len() + arm_rows.len());
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
                rows.append(&mut arm_rows);
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
            && let Some(pos) = self
                .schema
                .iter()
                .position(|name| name.eq_ignore_ascii_case(column))
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

/// Encoded index key values of a stored index key (`t<id>_i<index><values>`),
/// used to merge a scan that visits several partition subtrees back into index
/// order.
fn index_sort_key(key: &[u8]) -> Vec<u8> {
    key.get(sealdb_tablecodec::PREFIX_LEN + 8..)
        .unwrap_or(key)
        .to_vec()
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
        arms.push((index, IndexAccess::Equal(vec![value])));
    }
    Ok(arms)
}

fn scan_bounds(physical_id: i64, index_id: i64, access: &IndexAccess) -> (Key, Vec<u8>) {
    match access {
        IndexAccess::Equal(values) => {
            let start = encode_index_seek_prefix(physical_id, index_id, values);
            let end = next_prefix(start.as_bytes());
            (start, end)
        }
        IndexAccess::Range {
            low,
            low_inclusive,
            high,
            high_inclusive,
        } => {
            let start = if low.is_empty() {
                encode_index_seek_key(physical_id, index_id, &[])
            } else {
                let prefix = encode_index_seek_prefix(physical_id, index_id, low);
                if *low_inclusive {
                    prefix
                } else {
                    Key::from_raw(next_prefix(prefix.as_bytes()))
                }
            };
            let end = if high.is_empty() {
                next_prefix(encode_index_seek_key(physical_id, index_id, &[]).as_bytes())
            } else {
                let prefix = encode_index_seek_prefix(physical_id, index_id, high);
                if *high_inclusive {
                    next_prefix(prefix.as_bytes())
                } else {
                    prefix.as_bytes().to_vec()
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

/// Decode one probe-side row from its stored value.
fn decode_lookup_row(info: &TableInfo, raw: &[u8], expose_rowid: bool, handle: i64) -> Vec<Datum> {
    let decoded = decode_row(raw, &info.col_field_types());
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
    row
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
        // A table with an implicit `_tidb_rowid` has no user column holding the
        // handle: `handle_column` only borrows the first INT column for its
        // type, so filling it would overwrite real data with the row id.
        && (info.pk_is_handle || info.common_handle)
        && let Some(slot) = row.get_mut(handle_col.offset as usize)
        // Index already decoded this column (e.g. KEY(a,b,c) on non-PK table);
        // do not clobber with the artificial/common handle.
        && matches!(slot, Datum::Null)
    {
        *slot = Datum::I64(handle);
    }
    Ok(row)
}

/// Whether a common-handle table's PRIMARY KEY columns are missing from the
/// row a covering index read rebuilt (they are not part of the index key here).
fn common_handle_columns_missing(info: &TableInfo, row: &[Datum]) -> bool {
    let Some(pk) = info.indices.iter().find(|idx| idx.primary) else {
        return false;
    };
    pk.columns.iter().any(|col| {
        row.get(col.offset as usize)
            .is_some_and(|value| value.is_null())
    })
}

/// Recover the columns a covering index read left as `NULL` from the stored row.
fn backfill_row(info: &TableInfo, row: &mut [Datum], raw: &[u8]) {
    let decoded = decode_row(raw, &info.col_field_types());
    for col in &info.columns {
        if !matches!(row.get(col.offset as usize), Some(Datum::Null)) {
            continue;
        }
        if let Some((_, value)) = decoded.iter().find(|(id, _)| *id == col.id)
            && let Some(slot) = row.get_mut(col.offset as usize)
        {
            *slot = value.clone();
        }
    }
}

fn rehydrate_index_datum(info: &TableInfo, offset: usize, value: Datum) -> Result<Datum> {
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
