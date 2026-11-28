use crate::index::{index_applies, write_index_keys};
use crate::internal::exec::Executor;
use crate::join::apply::SubqueryEvalCtx;
use crate::table_reader::scan_table;
use sealdb_domain::Domain;
use sealdb_errno::{
    ERR_AMBIGUOUS_COLUMN, ERR_AUTOINC_READ_FAILED, ERR_BAD_FIELD, ERR_BAD_NULL,
    ERR_DATA_OUT_OF_RANGE, ERR_DATA_TOO_LONG, ERR_DUP_ENTRY, ERR_FIELD_NOT_DEFAULT,
    ERR_FIELD_SPECIFIED_TWICE, ERR_INVALID_AUTO_RANDOM, ERR_NO_DB,
    ERR_NO_PARTITION_FOR_GIVEN_VALUE, ERR_NO_SUCH_TABLE,
    ERR_NON_DEFAULT_VALUE_FOR_GENERATED_COLUMN, ERR_RECORD_FILE_FULL,
    ERR_ROW_DOES_NOT_MATCH_GIVEN_PARTITION_SET, ERR_TRUNCATED_WRONG_VALUE,
    ERR_TRUNCATED_WRONG_VALUE_FOR_FIELD, ERR_UNKNOWN_ERROR, ERR_UNKNOWN_PARTITION,
    ERR_WARN_DATA_OUT_OF_RANGE, ERR_WARN_DATA_TRUNCATED, ERR_WRONG_VALUE_COUNT_ON_ROW, Error,
    Result,
};
use sealdb_expression::{eval_expr_with_row, locate_partition_for_row};
use sealdb_kv::{LockWait, TxnHandle};
use sealdb_parser::ast::InsertStmt;
use sealdb_parser::ast::expressions::{ExprNode, ValueExpr};
use sealdb_tablecodec::{encode_row, encode_row_key_with_handle};
use sealdb_types::Datum;
use sealdb_types::EnumValue;
use sealdb_types::field_type::{
    FieldType, TYPE_BIT, TYPE_BLOB, TYPE_DATE, TYPE_DATETIME, TYPE_DECIMAL, TYPE_DOUBLE, TYPE_ENUM,
    TYPE_FLOAT, TYPE_INT24, TYPE_LONG, TYPE_LONG_BLOB, TYPE_LONG_LONG, TYPE_MEDIUM_BLOB,
    TYPE_NEW_DECIMAL, TYPE_SET, TYPE_SHORT, TYPE_STRING, TYPE_TIMESTAMP, TYPE_TINY, TYPE_TINY_BLOB,
    TYPE_VAR_STRING, TYPE_VARCHAR, TYPE_YEAR,
};
use sealdb_util::chunk::Chunk;
use std::sync::Arc;

thread_local! {
    /// One-based row number of the INSERT row currently being cast, used to
    /// render `… at row N` in column-conversion warnings.
    static CAST_ROW_NO: std::cell::Cell<usize> = const { std::cell::Cell::new(1) };
    /// The current row came from `INSERT … SELECT`: an out-of-range negative
    /// value reports TiDB's constant-overflow warning (1690), not 1264.
    static INSERT_SELECT_SOURCE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Whether this statement already recorded the auto-increment id TiDB
    /// exposes through `LAST_INSERT_ID()` (the first generated id wins).
    static LAST_ID_RECORDED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The session's `tidb_tmp_table_max_size` while the statement runs
    /// (`None` outside a session). Only temporary-table writes consult it.
    static TMP_TABLE_MAX_SIZE: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// Publish `tidb_tmp_table_max_size` for the duration of `f`. The previous
/// value is restored even if `f` unwinds.
pub fn with_tmp_table_max_size<R>(limit: Option<u64>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<u64>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TMP_TABLE_MAX_SIZE.with(|cell| cell.set(self.0));
        }
    }
    let previous = TMP_TABLE_MAX_SIZE.with(std::cell::Cell::get);
    TMP_TABLE_MAX_SIZE.with(|cell| cell.set(limit));
    let _restore = Restore(previous);
    f()
}

fn tmp_table_max_size() -> Option<u64> {
    TMP_TABLE_MAX_SIZE.with(std::cell::Cell::get)
}

/// Bytes a row contributes to a temporary table (`TemporaryTableData`
/// accounts the encoded KV value, which is dominated by the row payload).
fn row_data_size(col_ids: &[i64], values: &[Datum]) -> u64 {
    encode_row(col_ids, values).len() as u64
}

/// Record one generated AUTO_INCREMENT id for `LAST_INSERT_ID()`.
fn record_last_insert_id(id: i64) {
    if !LAST_ID_RECORDED.with(std::cell::Cell::get) {
        LAST_ID_RECORDED.with(|cell| cell.set(true));
        sealdb_expression::set_last_insert_id(id);
    }
}

/// Current INSERT row number for conversion diagnostics.
fn cast_row_no() -> usize {
    CAST_ROW_NO.with(std::cell::Cell::get)
}

pub struct InsertExec {
    domain: Arc<Domain>,
    stmt: InsertStmt,
    current_db: String,
    txn: Arc<TxnHandle>,
    select: Option<Box<dyn Executor>>,
    /// `alias.column` pairs the source query exposes (for `ON DUPLICATE KEY
    /// UPDATE` resolution and ambiguity checks).
    source_scope: Vec<(String, String)>,
    /// Source columns appended to the source projection, in output order.
    source_extra: Vec<(String, String)>,
    done: bool,
}

impl InsertExec {
    pub fn new(
        domain: Arc<Domain>,
        stmt: InsertStmt,
        current_db: impl Into<String>,
        txn: Arc<TxnHandle>,
        select: Option<Box<dyn Executor>>,
        source_scope: Vec<(String, String)>,
        source_extra: Vec<(String, String)>,
    ) -> Self {
        Self {
            domain,
            stmt,
            current_db: current_db.into(),
            txn,
            select,
            source_scope,
            source_extra,
            done: false,
        }
    }
    /// `REPLACE`: every row conflicting on any unique key is deleted, then the
    /// new row is written. TiDB counts a rewritten-verbatim row as one affected
    /// row and a changed replacement as delete + insert (two).
    fn replace_rows(
        &mut self,
        info: &sealdb_meta::TableInfo,
        pending: Vec<PendingRow>,
        batched: bool,
    ) -> Result<()> {
        let lock = crate::update::dml_row_lock(&self.txn)?;
        let physical = sealdb_planner::core::prune_partitions(info, None);
        let mut existing = scan_table(&self.txn, info, lock, LockWait::Block, &physical)?;
        let source = pending.len() as u64;
        let mut affected = 0u64;
        let mut duplicates = 0u64;
        self.txn.with_txn(|txn| {
            for PendingRow {
                values,
                rowid,
                generated_id,
            } in pending
            {
                let conflicts: Vec<usize> = existing
                    .iter()
                    .enumerate()
                    .filter(|(_, row)| index_conflict(info, &row.values, &values).is_some())
                    .map(|(i, _)| i)
                    .collect();
                if conflicts.is_empty() {
                    let (handle, physical_id) = write_row(txn, &self.domain, info, &values, rowid)?;
                    if let Some(id) = generated_id {
                        record_last_insert_id(id);
                    }
                    existing.push(crate::table_reader::ScannedRow {
                        handle,
                        physical_id,
                        values,
                    });
                    affected += 1;
                    continue;
                }
                duplicates += 1;
                let unchanged = conflicts.len() == 1 && existing[conflicts[0]].values == values;
                if unchanged {
                    affected += 1;
                    continue;
                }
                for idx in conflicts.into_iter().rev() {
                    let row = existing.remove(idx);
                    crate::delete::apply_row_delete(txn, info, &row)?;
                    affected += 1;
                }
                let (handle, physical_id) = write_row(txn, &self.domain, info, &values, rowid)?;
                if let Some(id) = generated_id {
                    record_last_insert_id(id);
                }
                existing.push(crate::table_reader::ScannedRow {
                    handle,
                    physical_id,
                    values,
                });
                affected += 1;
            }
            Ok(())
        })?;
        crate::internal::note_dml_affected(affected);
        if batched {
            crate::internal::set_dml_info(records_info(source, duplicates));
        }
        Ok(())
    }
}

impl Executor for InsertExec {
    fn open(&mut self) -> Result<()> {
        LAST_ID_RECORDED.with(|cell| cell.set(false));
        let _ignore_mode = IgnoreModeGuard::new(self.stmt.ignore);
        let (schema, table) = self.stmt.table.qualified(&self.current_db);
        if schema.is_empty() {
            return Err(Error::new(ERR_NO_DB, "No database selected"));
        }
        let is = self.domain.infoschema();
        let info = is.table_by_name(&schema, &table).ok_or_else(|| {
            Error::new(
                ERR_NO_SUCH_TABLE,
                format!("Table '{schema}.{table}' doesn't exist"),
            )
        })?;
        // TiDB refuses DML on views and sequences with a per-kind 1105.
        if info.is_view() {
            let verb = if self.stmt.replace {
                "replace"
            } else {
                "insert"
            };
            return Err(Error::new(
                ERR_UNKNOWN_ERROR,
                format!(
                    "{verb} into view {} is not supported now",
                    self.stmt.table.name
                ),
            ));
        }
        if info.is_sequence() {
            let verb = if self.stmt.replace {
                "replace"
            } else {
                "insert"
            };
            return Err(Error::new(
                ERR_UNKNOWN_ERROR,
                format!(
                    "{verb} into sequence {} is not supported now",
                    self.stmt.table.name
                ),
            ));
        }
        validate_insert_columns(info, &self.stmt.columns)?;
        validate_on_duplicate(info, &self.stmt, &self.source_scope)?;
        let mut extra_start = usize::MAX;
        let source_rows = if let Some(select) = &mut self.select {
            select.open()?;
            extra_start = select
                .schema()
                .len()
                .saturating_sub(self.source_extra.len());
            let mut rows = Vec::new();
            while let Some(chunk) = select.next()? {
                for i in 0..chunk.num_rows() {
                    rows.push(chunk.get_row(i));
                }
            }
            select.close()?;
            rows
        } else {
            Vec::new()
        };
        let mut pending = Vec::with_capacity(if self.select.is_some() {
            source_rows.len()
        } else {
            self.stmt.rows.len()
        });
        // Extra source columns appended by `widen_insert_source`, per row.
        let mut extra_rows: Vec<Vec<Datum>> = Vec::new();
        if self.select.is_some() {
            for (i, row) in source_rows.iter().enumerate() {
                let base = if extra_start == usize::MAX {
                    row.as_slice()
                } else {
                    &row[..extra_start.min(row.len())]
                };
                let rowid = rowid_override(&self.stmt.columns, base)?;
                let previous = INSERT_SELECT_SOURCE.with(|flag| flag.replace(true));
                let mapped = map_insert_row(info, &self.stmt.columns, base, &[], i + 1);
                INSERT_SELECT_SOURCE.with(|flag| flag.set(previous));
                pending.push(PendingRow {
                    values: mapped?,
                    rowid,
                    generated_id: None,
                });
                extra_rows.push(if extra_start == usize::MAX {
                    Vec::new()
                } else {
                    row[extra_start.min(row.len())..].to_vec()
                });
            }
        } else {
            // VALUES may hold scalar/IN/EXISTS subqueries (`INSERT ... VALUES
            // ((SELECT ...))`); evaluate them against the session txn.
            let mut subquery = SubqueryEvalCtx::new(
                Arc::clone(&self.domain),
                self.current_db.clone(),
                Arc::clone(&self.txn),
                Some(table.clone()),
            );
            for (i, row) in self.stmt.rows.iter().enumerate() {
                let (values, rowid) = sealdb_expression::with_insert_value_eval(|| {
                    eval_insert_row(info, &self.stmt.columns, row, &mut subquery, i + 1)
                })?;
                pending.push(PendingRow {
                    values,
                    rowid,
                    generated_id: None,
                });
            }
        }
        for PendingRow {
            values,
            generated_id,
            ..
        } in &mut pending
        {
            *generated_id = fill_auto_increment(
                &self.domain,
                info,
                values,
                !self.stmt.on_duplicate.is_empty(),
            )?;
            fill_auto_random(&self.domain, info, values)?;
            enforce_not_null_columns(
                info,
                values,
                self.stmt.ignore,
                self.stmt.rows.len() > 1 || self.select.is_some(),
            )?;
        }
        if !self.stmt.partitions.is_empty() {
            pending =
                filter_target_partitions(info, &self.stmt.partitions, pending, self.stmt.ignore)?;
        }
        let batched = self.stmt.rows.len() > 1
            || self.stmt.select.is_some()
            || self.stmt.select_set.is_some();
        if self.stmt.replace {
            return self.replace_rows(info, pending, batched);
        }
        if self.stmt.on_duplicate.is_empty() {
            let ignore = self.stmt.ignore;
            // Unique secondary indexes need the same pre-write check TiDB does
            // for every insert row (the KV layer only guards the PK handle).
            let lock = crate::update::dml_row_lock(&self.txn)?;
            let physical = sealdb_planner::core::prune_partitions(info, None);
            let mut existing = scan_table(&self.txn, info, lock, LockWait::Block, &physical)?;
            let mut affected = 0u64;
            let mut duplicates = 0u64;
            // Temporary tables are capped by `tidb_tmp_table_max_size`; the
            // scanned rows give the current footprint so a rolled-back insert
            // is not counted again (the limit lives outside the txn).
            let temp_limit = if info.temporary {
                tmp_table_max_size()
            } else {
                None
            };
            let col_ids: Vec<i64> = info.columns.iter().map(|c| c.id).collect();
            let table_name = self.stmt.table.name.clone();
            let mut temp_size = temp_limit
                .map(|_| {
                    existing
                        .iter()
                        .map(|row| row_data_size(&col_ids, &row.values))
                        .sum::<u64>()
                })
                .unwrap_or(0);
            self.txn.with_txn(|txn| {
                for PendingRow {
                    values,
                    rowid,
                    generated_id,
                } in &pending
                {
                    if let Some((row_idx, idx)) =
                        existing.iter().enumerate().find_map(|(row_idx, row)| {
                            index_conflict(info, &row.values, values).map(|idx| (row_idx, idx))
                        })
                    {
                        let e = duplicate_error_from_row(
                            info,
                            idx,
                            values,
                            Some(&existing[row_idx].values),
                        );
                        if ignore {
                            duplicates += 1;
                            sealdb_expression::note_warning(e.code, e.message);
                            continue;
                        }
                        return Err(e);
                    }
                    if let Some(limit) = temp_limit
                        && temp_size > limit
                    {
                        return Err(Error::new(
                            ERR_RECORD_FILE_FULL,
                            format!("The table '{table_name}' is full"),
                        ));
                    }
                    // TiDB `Foreign_Key_Check` on INSERT: a non-NULL key with
                    // no parent row is 1452 (or a warning under IGNORE).
                    let violation = crate::update::fk_insert_violation(
                        &self.domain,
                        txn,
                        &schema,
                        info,
                        values,
                    )?;
                    if crate::update::apply_fk_outcome(violation, ignore)? {
                        continue;
                    }
                    match write_row(txn, &self.domain, info, values, *rowid) {
                        Ok((handle, physical_id)) => {
                            if let Some(id) = generated_id {
                                record_last_insert_id(*id);
                            }
                            if temp_limit.is_some() {
                                temp_size += row_data_size(&col_ids, values);
                            }
                            existing.push(crate::table_reader::ScannedRow {
                                handle,
                                physical_id,
                                values: values.clone(),
                            });
                            affected += 1;
                        }
                        Err(e) if ignore && e.code == ERR_DUP_ENTRY => {
                            duplicates += 1;
                            sealdb_expression::note_warning(e.code, e.message.clone());
                        }
                        // `INSERT IGNORE` past the last partition warns `1526`.
                        Err(e) if ignore && e.code == ERR_NO_PARTITION_FOR_GIVEN_VALUE => {
                            sealdb_expression::note_warning(e.code, e.message);
                        }
                        Err(e) => return Err(e),
                    }
                }
                Ok(())
            })?;
            crate::internal::note_dml_affected(affected);
            if batched {
                crate::internal::set_dml_info(records_info(pending.len() as u64, duplicates));
            }
            return Ok(());
        }
        // `ON DUPLICATE KEY UPDATE`: locate conflicts against a snapshot of the
        // table, then update the conflicting row instead of inserting.
        //
        // Planning runs outside the txn lock: FK checks scan other tables and
        // `scan_table` takes the same non-reentrant lock. The planned writes
        // are then applied in one transaction, preserving statement atomicity.
        let lock = crate::update::dml_row_lock(&self.txn)?;
        let physical = sealdb_planner::core::prune_partitions(info, None);
        let existing = scan_table(&self.txn, info, lock, LockWait::Block, &physical)?;
        let names = crate::update::col_names(info);
        let assigned: std::collections::HashSet<String> = self
            .stmt
            .on_duplicate
            .iter()
            .map(|item| item.column.to_ascii_lowercase())
            .collect();
        enum OdkuAction {
            Update {
                idx: usize,
                merged: Vec<Datum>,
            },
            Insert {
                values: Vec<Datum>,
                rowid: Option<i64>,
                generated_id: Option<i64>,
            },
        }
        let mut planned = existing.clone();
        // Action index for each planned row: existing rows start with none and
        // get an `Update` action on their first conflict; rows inserted by this
        // statement point at their `Insert` action.
        let mut action_of: Vec<Option<usize>> = vec![None; planned.len()];
        let mut actions: Vec<OdkuAction> = Vec::new();
        let source = pending.len() as u64;
        let mut duplicates = 0u64;
        let mut affected = 0u64;
        for (
            row_no,
            PendingRow {
                values,
                rowid,
                generated_id,
            },
        ) in pending.into_iter().enumerate()
        {
            match find_conflict(info, &planned, None, &values) {
                Some((idx, _)) => {
                    let extra = extra_rows.get(row_no).map_or(&[][..], Vec::as_slice);
                    let old = planned[idx].values.clone();
                    let mut merged = old.clone();
                    apply_on_duplicate_assignments(
                        info,
                        &self.stmt.on_duplicate,
                        &names,
                        &self.source_extra,
                        &old,
                        &values,
                        extra,
                        &mut merged,
                    )?;
                    fill_generated_columns(info, &mut merged)?;
                    // `ON DUPLICATE KEY UPDATE col = NULL` on a NOT NULL
                    // column is 1048 even when sql_mode is non-strict;
                    // `INSERT IGNORE` only warns and stores the zero value.
                    enforce_not_null_columns(
                        info,
                        &mut merged,
                        self.stmt.ignore,
                        self.stmt.rows.len() > 1 || self.select.is_some(),
                    )?;
                    // Foreign keys see the new row exactly like an UPDATE;
                    // `INSERT IGNORE` warns (1452) and keeps the old row.
                    let child = crate::update::fk_child_violation(
                        &self.domain,
                        &self.txn,
                        &schema,
                        info,
                        &merged,
                        &assigned,
                    )?;
                    if crate::update::apply_fk_outcome(child, self.stmt.ignore)? {
                        continue;
                    }
                    let parent = crate::update::fk_parent_violation(
                        &self.domain,
                        &self.txn,
                        info,
                        &old,
                        &merged,
                        &assigned,
                    )?;
                    if crate::update::apply_fk_outcome(parent, self.stmt.ignore)? {
                        continue;
                    }
                    if let Some((_, dup_idx)) = find_conflict(info, &planned, Some(idx), &merged) {
                        let err = duplicate_error(info, dup_idx, &merged);
                        if self.stmt.ignore {
                            // `INSERT IGNORE ... ON DUPLICATE KEY UPDATE`: an
                            // assignment that collides with another row is a
                            // warning and the existing row stays unchanged.
                            sealdb_expression::note_warning(err.code, err.message);
                            continue;
                        }
                        return Err(err);
                    }
                    if idx < existing.len() {
                        match action_of[idx] {
                            Some(action) => {
                                if let Some(OdkuAction::Update { merged: slot, .. }) =
                                    actions.get_mut(action)
                                {
                                    *slot = merged.clone();
                                }
                            }
                            None => {
                                action_of[idx] = Some(actions.len());
                                actions.push(OdkuAction::Update {
                                    idx,
                                    merged: merged.clone(),
                                });
                            }
                        }
                    } else {
                        let action = action_of[idx].expect("insert action for planned row");
                        if let Some(OdkuAction::Insert { values, .. }) = actions.get_mut(action) {
                            *values = merged.clone();
                        }
                    }
                    // TiDB reports an ODKU row whose update changed the target
                    // as two affected rows and one duplicate, even when the
                    // conflict is with a row this statement inserted earlier.
                    if merged != old {
                        affected += 2;
                        duplicates += 1;
                    }
                    planned[idx].values = merged;
                }
                None => {
                    action_of.push(Some(actions.len()));
                    planned.push(crate::table_reader::ScannedRow {
                        handle: 0,
                        physical_id: 0,
                        values: values.clone(),
                    });
                    actions.push(OdkuAction::Insert {
                        values,
                        rowid,
                        generated_id,
                    });
                }
            }
        }
        self.txn.with_txn(|txn| {
            for action in actions {
                match action {
                    OdkuAction::Update { idx, merged } => {
                        crate::update::apply_row_update(txn, info, &existing[idx], merged)?;
                    }
                    OdkuAction::Insert {
                        values,
                        rowid,
                        generated_id,
                    } => {
                        write_row(txn, &self.domain, info, &values, rowid)?;
                        if let Some(id) = generated_id {
                            record_last_insert_id(id);
                        }
                        affected += 1;
                    }
                }
            }
            Ok(())
        })?;
        crate::internal::note_dml_affected(affected);
        if batched {
            crate::internal::set_dml_info(records_info(source, duplicates));
        }
        Ok(())
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        Ok(None)
    }

    fn close(&mut self) -> Result<()> {
        Ok(())
    }

    fn schema(&self) -> &[String] {
        &[]
    }
}

/// MySQL OK-packet info for INSERT/REPLACE over a batched row source.
fn records_info(records: u64, duplicates: u64) -> String {
    format!(
        "Records: {records}  Duplicates: {duplicates}  Warnings: {}",
        sealdb_expression::warnings_len()
    )
}

/// Resolve `ON DUPLICATE KEY UPDATE` against the target row and the source
/// query's tables. Unknown columns are 1054, ambiguous ones 1052, and
/// `VALUES(col)` names only columns of the insert target.
fn validate_on_duplicate(
    info: &sealdb_meta::TableInfo,
    stmt: &InsertStmt,
    scope: &[(String, String)],
) -> Result<()> {
    if stmt.on_duplicate.is_empty() {
        return Ok(());
    }
    let target: Vec<String> = info
        .columns
        .iter()
        .filter(|c| !c.hidden)
        .map(|c| c.name.o().to_string())
        .collect();
    let table_name = info.name.o();
    for item in &stmt.on_duplicate {
        if let Some(qualifier) = item.table.as_deref() {
            let matches_target = qualifier.eq_ignore_ascii_case(table_name)
                || qualifier
                    .rsplit_once('.')
                    .is_some_and(|(_, t)| t.eq_ignore_ascii_case(table_name));
            if !matches_target {
                return Err(Error::new(
                    ERR_BAD_FIELD,
                    format!(
                        "Unknown column '{}.{}' in 'field list'",
                        qualifier, item.column
                    ),
                ));
            }
        }
        if !target.iter().any(|c| c.eq_ignore_ascii_case(&item.column)) {
            return Err(Error::new(
                ERR_BAD_FIELD,
                format!("Unknown column '{}' in 'field list'", item.column),
            ));
        }
        validate_odku_expr(&item.expr, table_name, &target, scope)?;
    }
    Ok(())
}

fn validate_odku_expr(
    expr: &ExprNode,
    target_name: &str,
    target: &[String],
    scope: &[(String, String)],
) -> Result<()> {
    match expr {
        ExprNode::Column(col) => {
            if let Some(qualifier) = col.table.as_deref() {
                let matches_target = qualifier.eq_ignore_ascii_case(target_name)
                    || qualifier
                        .rsplit_once('.')
                        .is_some_and(|(_, t)| t.eq_ignore_ascii_case(target_name));
                if matches_target {
                    if !target.iter().any(|c| c.eq_ignore_ascii_case(&col.name)) {
                        return Err(Error::new(
                            ERR_BAD_FIELD,
                            format!(
                                "Unknown column '{}.{}' in 'field list'",
                                qualifier, col.name
                            ),
                        ));
                    }
                } else if !scope.iter().any(|(alias, name)| {
                    alias.eq_ignore_ascii_case(qualifier) && name.eq_ignore_ascii_case(&col.name)
                }) {
                    return Err(Error::new(
                        ERR_BAD_FIELD,
                        format!(
                            "Unknown column '{}.{}' in 'field list'",
                            qualifier, col.name
                        ),
                    ));
                }
            } else {
                let mut matches =
                    usize::from(target.iter().any(|c| c.eq_ignore_ascii_case(&col.name)));
                matches += scope
                    .iter()
                    .filter(|(_, name)| name.eq_ignore_ascii_case(&col.name))
                    .count();
                if matches > 1 {
                    return Err(Error::new(
                        ERR_AMBIGUOUS_COLUMN,
                        format!("Column '{}' in field list is ambiguous", col.name),
                    ));
                }
                if matches == 0 {
                    return Err(Error::new(
                        ERR_BAD_FIELD,
                        format!("Unknown column '{}' in 'field list'", col.name),
                    ));
                }
            }
        }
        ExprNode::Func(f) => {
            if f.name.eq_ignore_ascii_case("values") {
                let Some(ExprNode::Column(col)) = f.args.first() else {
                    return Ok(());
                };
                if !target.iter().any(|c| c.eq_ignore_ascii_case(&col.name)) {
                    return Err(Error::new(
                        ERR_BAD_FIELD,
                        format!("Unknown column '{}' in 'field list'", col.name),
                    ));
                }
                return Ok(());
            }
            for arg in &f.args {
                validate_odku_expr(arg, target_name, target, scope)?;
            }
        }
        ExprNode::Binary(b) => {
            validate_odku_expr(&b.left, target_name, target, scope)?;
            validate_odku_expr(&b.right, target_name, target, scope)?;
        }
        ExprNode::Unary(u) => validate_odku_expr(&u.expr, target_name, target, scope)?,
        ExprNode::Cast(c) => validate_odku_expr(&c.expr, target_name, target, scope)?,
        ExprNode::InList(i) => {
            validate_odku_expr(&i.expr, target_name, target, scope)?;
            for item in &i.list {
                validate_odku_expr(item, target_name, target, scope)?;
            }
        }
        ExprNode::Between(b) => {
            validate_odku_expr(&b.expr, target_name, target, scope)?;
            validate_odku_expr(&b.low, target_name, target, scope)?;
            validate_odku_expr(&b.high, target_name, target, scope)?;
        }
        ExprNode::Like(l) => {
            validate_odku_expr(&l.expr, target_name, target, scope)?;
            validate_odku_expr(&l.pattern, target_name, target, scope)?;
        }
        ExprNode::IsNull(i) => validate_odku_expr(&i.expr, target_name, target, scope)?,
        ExprNode::Case(c) => {
            if let Some(operand) = &c.operand {
                validate_odku_expr(operand, target_name, target, scope)?;
            }
            for (when, then) in &c.when_then {
                validate_odku_expr(when, target_name, target, scope)?;
                validate_odku_expr(then, target_name, target, scope)?;
            }
            if let Some(otherwise) = &c.else_expr {
                validate_odku_expr(otherwise, target_name, target, scope)?;
            }
        }
        ExprNode::Row(r) => {
            for item in &r.exprs {
                validate_odku_expr(item, target_name, target, scope)?;
            }
        }
        ExprNode::Value(_)
        | ExprNode::Exists(_)
        | ExprNode::InSubquery(_)
        | ExprNode::ScalarSubquery(_)
        | ExprNode::QuantifiedCmp(_)
        | ExprNode::Window(_) => {}
    }
    Ok(())
}

/// Evaluate `ON DUPLICATE KEY UPDATE` against the existing target row (bare
/// columns), the would-be inserted row (`VALUES(col)`), and the extra source
/// columns widened into the source projection.
fn apply_on_duplicate_assignments(
    info: &sealdb_meta::TableInfo,
    list: &[sealdb_parser::ast::Assignment],
    target_names: &[String],
    extra_cols: &[(String, String)],
    existing: &[Datum],
    inserted: &[Datum],
    extra_values: &[Datum],
    merged: &mut [Datum],
) -> Result<()> {
    let table_name = info.name.o();
    let mut schema: Vec<String> = target_names
        .iter()
        .map(|name| format!("{table_name}.{name}"))
        .collect();
    let mut row = existing.to_vec();
    for (i, (alias, column)) in extra_cols.iter().enumerate() {
        schema.push(format!("{alias}.{column}"));
        row.push(extra_values.get(i).cloned().unwrap_or(Datum::Null));
    }
    for item in list {
        let idx = target_names
            .iter()
            .position(|c| c.eq_ignore_ascii_case(&item.column))
            .ok_or_else(|| {
                Error::new(
                    ERR_BAD_FIELD,
                    format!("Unknown column '{}' in 'field list'", item.column),
                )
            })?;
        let mut expr = item.expr.clone();
        substitute_values(&mut expr, target_names, inserted);
        let value = eval_expr_with_row(&expr, &schema, &row)?;
        // The assignment lands in `merged` as the expression's datum; storage
        // expects the column's own type/scale (`c1 = 1` into DECIMAL(6,4)
        // stores and prints `1.0000`).
        let value = match info.columns.get(idx) {
            Some(col) => cast_for_column(&value, col)?,
            None => value,
        };
        if let Some(slot) = merged.get_mut(idx) {
            *slot = value;
        }
    }
    Ok(())
}

/// Replace `VALUES(col)` with the pending row's value for `col`.
fn substitute_values(expr: &mut ExprNode, target_names: &[String], inserted: &[Datum]) {
    let replacement = if let ExprNode::Func(f) = &*expr
        && f.name.eq_ignore_ascii_case("values")
        && f.args.len() == 1
        && let ExprNode::Column(col) = &f.args[0]
    {
        let value = target_names
            .iter()
            .position(|c| c.eq_ignore_ascii_case(&col.name))
            .and_then(|i| inserted.get(i).cloned())
            .unwrap_or(Datum::Null);
        Some(value)
    } else {
        None
    };
    if let Some(datum) = replacement {
        *expr = ExprNode::Value(ValueExpr { datum });
        return;
    }
    match expr {
        ExprNode::Func(f) => {
            for arg in &mut f.args {
                substitute_values(arg, target_names, inserted);
            }
        }
        ExprNode::Binary(b) => {
            substitute_values(&mut b.left, target_names, inserted);
            substitute_values(&mut b.right, target_names, inserted);
        }
        ExprNode::Unary(u) => substitute_values(&mut u.expr, target_names, inserted),
        ExprNode::Cast(c) => substitute_values(&mut c.expr, target_names, inserted),
        ExprNode::InList(i) => {
            substitute_values(&mut i.expr, target_names, inserted);
            for item in &mut i.list {
                substitute_values(item, target_names, inserted);
            }
        }
        ExprNode::Between(b) => {
            substitute_values(&mut b.expr, target_names, inserted);
            substitute_values(&mut b.low, target_names, inserted);
            substitute_values(&mut b.high, target_names, inserted);
        }
        ExprNode::Like(l) => {
            substitute_values(&mut l.expr, target_names, inserted);
            substitute_values(&mut l.pattern, target_names, inserted);
        }
        ExprNode::IsNull(i) => substitute_values(&mut i.expr, target_names, inserted),
        ExprNode::Case(c) => {
            if let Some(operand) = &mut c.operand {
                substitute_values(operand, target_names, inserted);
            }
            for (when, then) in &mut c.when_then {
                substitute_values(when, target_names, inserted);
                substitute_values(then, target_names, inserted);
            }
            if let Some(otherwise) = &mut c.else_expr {
                substitute_values(otherwise, target_names, inserted);
            }
        }
        ExprNode::Row(r) => {
            for item in &mut r.exprs {
                substitute_values(item, target_names, inserted);
            }
        }
        ExprNode::Value(_)
        | ExprNode::Column(_)
        | ExprNode::Exists(_)
        | ExprNode::InSubquery(_)
        | ExprNode::ScalarSubquery(_)
        | ExprNode::QuantifiedCmp(_)
        | ExprNode::Window(_) => {}
    }
}

fn eval_insert_row(
    info: &sealdb_meta::TableInfo,
    columns: &[String],
    row: &[sealdb_parser::ast::ExprNode],
    subquery: &mut SubqueryEvalCtx,
    row_no: usize,
) -> Result<(Vec<Datum>, Option<i64>)> {
    let mut values = Vec::with_capacity(row.len());
    // Parallel to `values`: whether the value is a *binary* string (`UNHEX`,
    // `0x…`), which TiDB converts to a column charset by truncating instead of
    // substituting `?`.
    let mut binary_values = Vec::with_capacity(row.len());
    let mut deferred_warnings = Vec::new();
    for (i, expr) in row.iter().enumerate() {
        // `DEFAULT` in VALUES: use the column default (NULL when none). A
        // generated column recomputes its value below.
        if is_default_keyword(expr) {
            let value = match column_at(info, columns, i) {
                Some(col) => column_default_value(col)?,
                None => Datum::Null,
            };
            values.push(value);
            binary_values.push(false);
            continue;
        }
        // Writing a generated column is only legal via bare `DEFAULT` or
        // `DEFAULT(itself)` (TiDB 3105); any other expression is refused.
        if let Some(target) = column_at(info, columns, i)
            && target.generated_expr.is_some()
        {
            let allowed = default_column_arg(expr)
                .is_some_and(|name| name.eq_ignore_ascii_case(target.name.o()));
            if !allowed {
                return Err(Error::new(
                    ERR_NON_DEFAULT_VALUE_FOR_GENERATED_COLUMN,
                    format!(
                        "The value specified for generated column '{}' in table '{}' is not allowed.",
                        target.name.o(),
                        info.name.o()
                    ),
                ));
            }
        }
        // `DEFAULT(col)` names the column it takes the default of, so it is
        // independent of the value's position in the row.
        if let Some(name) = default_column_arg(expr) {
            let col = info
                .columns
                .iter()
                .find(|c| c.name.o().eq_ignore_ascii_case(name))
                .ok_or_else(|| {
                    Error::new(
                        ERR_BAD_FIELD,
                        format!("Unknown column '{name}' in 'field list'"),
                    )
                })?;
            values.push(column_default_value(col)?);
            binary_values.push(false);
            continue;
        }
        // TiDB reports the column-assignment out-of-range warning before a
        // CAST expression's own truncation warning, so hold the latter until
        // after the row has been assigned to its columns.
        let start = sealdb_expression::warnings_len();
        // Drop a marker left over from planning-time constant folding.
        let _ = sealdb_expression::take_binary_value();
        let value = subquery.eval(expr, &[], &[])?;
        let binary_value =
            sealdb_expression::take_binary_value() || matches!(&value, Datum::Bit(_));
        // TiDB binary literals (`0x…` / `x'…'`) are strings in a character
        // context: `INSERT INTO vc VALUES (0x5c27090a)` stores the raw bytes,
        // not the decimal value of the literal.
        let value = match (expr, &value) {
            (ExprNode::Value(v), Datum::Bit(b))
                if column_at(info, columns, i)
                    .is_some_and(|col| col.field_type.is_character_type()) =>
            {
                let _ = v;
                Datum::Bytes(b.clone())
            }
            _ => value,
        };
        if row.len() == 1 && matches!(expr, ExprNode::Cast(_)) {
            deferred_warnings = sealdb_expression::take_warnings_from(start);
        }
        values.push(value);
        binary_values.push(binary_value);
    }
    let rowid = rowid_override(columns, &values)?;
    let mapped = map_insert_row(info, columns, &values, &binary_values, row_no)?;
    for (code, message) in deferred_warnings {
        sealdb_expression::note_warning(code, message);
    }
    Ok((mapped, rowid))
}

/// Column at VALUES position `i`, honouring an explicit column list.
fn column_at<'a>(
    info: &'a sealdb_meta::TableInfo,
    columns: &[String],
    i: usize,
) -> Option<&'a sealdb_meta::ColumnInfo> {
    if columns.is_empty() {
        return info.columns.get(i);
    }
    let name = columns.get(i)?;
    info.columns
        .iter()
        .find(|c| c.name.o().eq_ignore_ascii_case(name))
}

/// TiDB resolves the explicit column list before touching any row: unknown
/// names are `1054` and a repeated target is `1110`.
///
/// `_tidb_rowid` is a pseudo column of tables whose handle is not a single
/// integer PRIMARY KEY. Writing it needs `tidb_opt_write_row_id`, and only a
/// heap table (not a common handle) can actually store an explicit value.
fn validate_insert_columns(info: &sealdb_meta::TableInfo, columns: &[String]) -> Result<()> {
    for (i, name) in columns.iter().enumerate() {
        if is_tidb_rowid(name) {
            check_write_row_id(info)?;
        } else if !info
            .columns
            .iter()
            .any(|c| c.name.o().eq_ignore_ascii_case(name))
        {
            return Err(Error::new(
                ERR_BAD_FIELD,
                format!("Unknown column '{name}' in 'field list'"),
            ));
        }
        if columns[..i]
            .iter()
            .any(|seen| seen.eq_ignore_ascii_case(name))
        {
            return Err(Error::new(
                ERR_FIELD_SPECIFIED_TWICE,
                format!("Column '{name}' specified twice"),
            ));
        }
    }
    Ok(())
}

fn is_tidb_rowid(name: &str) -> bool {
    name.eq_ignore_ascii_case("_tidb_rowid")
}

/// Reject an explicit `_tidb_rowid` write the way TiDB does:
/// * a single-integer PRIMARY KEY handle has no such column (`1054`);
/// * a common handle cannot take one, and a heap table needs
///   `tidb_opt_write_row_id` (`1105`).
pub(crate) fn check_write_row_id(info: &sealdb_meta::TableInfo) -> Result<()> {
    if info.pk_is_handle {
        return Err(Error::new(
            ERR_BAD_FIELD,
            "Unknown column '_tidb_rowid' in 'field list'",
        ));
    }
    if info.common_handle || !sealdb_expression::allow_write_row_id() {
        return Err(Error::new(
            ERR_UNKNOWN_ERROR,
            "insert, update and replace statements for _tidb_rowid are not supported",
        ));
    }
    Ok(())
}

fn wrong_value_count_on_row(row_no: usize) -> Error {
    Error::new(
        ERR_WRONG_VALUE_COUNT_ON_ROW,
        format!("Column count doesn't match value count at row {row_no}"),
    )
}

/// Refuse an explicit NULL in a NOT NULL column (1048); `INSERT IGNORE`
/// downgrades it to a warning and stores the type's zero value.
fn enforce_not_null_columns(
    info: &sealdb_meta::TableInfo,
    values: &mut [Datum],
    ignore: bool,
    multi_row: bool,
) -> Result<()> {
    for col in &info.columns {
        if col.hidden
            || col.auto_increment
            || col.auto_random.is_some()
            || col.generated_expr.is_some()
        {
            continue;
        }
        if col.field_type.flag & sealdb_types::field_type::NOT_NULL_FLAG == 0 {
            continue;
        }
        let idx = col.offset as usize;
        if !values.get(idx).is_some_and(|value| value.is_null()) {
            continue;
        }
        let msg = format!("Column '{}' cannot be null", col.name.o());
        // `@@tidb_enable_strict_not_null_check = OFF` silently stores the
        // implicit default. With the check on, a non-strict sql_mode turns a
        // batch source (multiple VALUES rows or INSERT … SELECT) into a
        // warning, while a single VALUES row still aborts.
        if !ignore && !sealdb_expression::strict_not_null_check() {
            values[idx] = implicit_zero(&col.field_type);
        } else if ignore || (!strict_sql_mode() && multi_row) {
            sealdb_expression::note_warning(ERR_BAD_NULL, msg);
            values[idx] = implicit_zero(&col.field_type);
        } else {
            return Err(Error::new(ERR_BAD_NULL, msg));
        }
    }
    Ok(())
}

/// The type's zero value, used when a NOT NULL column gets no value.
pub(crate) fn implicit_zero(ft: &FieldType) -> Datum {
    use sealdb_types::field_type::*;
    match ft.tp {
        TYPE_YEAR => Datum::Year(0),
        TYPE_TINY | TYPE_SHORT | TYPE_LONG | TYPE_INT24 | TYPE_LONG_LONG => {
            if ft.is_unsigned() {
                Datum::U64(0)
            } else {
                Datum::I64(0)
            }
        }
        TYPE_FLOAT | TYPE_DOUBLE => Datum::F64(0.0),
        TYPE_NEW_DECIMAL => Datum::Decimal(
            sealdb_types::MyDecimal::parse("0")
                .map(|d| d.with_frac(ft.decimal))
                .unwrap_or_else(|_| sealdb_types::MyDecimal::from_i64(0)),
        ),
        TYPE_DATE | TYPE_DATETIME | TYPE_TIMESTAMP | TYPE_DURATION => {
            let mut t = sealdb_types::Time::new(0, 0, 0, 0, 0, 0);
            t.tp = match ft.tp {
                TYPE_DATE => sealdb_types::TimeType::Date,
                TYPE_TIMESTAMP => sealdb_types::TimeType::Timestamp,
                TYPE_DURATION => sealdb_types::TimeType::Time,
                _ => sealdb_types::TimeType::Datetime,
            };
            t.fsp = ft.decimal.max(0) as i8;
            Datum::Time(t)
        }
        TYPE_ENUM | TYPE_SET => Datum::Enum(EnumValue {
            label: Vec::new(),
            number: 0,
        }),
        TYPE_BIT => Datum::Bit(Vec::new()),
        _ => Datum::Bytes(Vec::new()),
    }
}

/// Use the column's `DEFAULT <expr>` body for an inserted row.
pub(crate) fn column_default_value(col: &sealdb_meta::ColumnInfo) -> Result<Datum> {
    // A TIMESTAMP literal default was pinned to UTC at DDL time
    // (`OriginDefaultValue`); use it as-is instead of re-reading the printed
    // default in the current session zone.
    if let Some(text) = col.origin_default_expr.as_deref()
        && col.field_type.tp == TYPE_TIMESTAMP
        && let Ok(mut t) = sealdb_types::parse_datetime_literal(text)
    {
        t.tp = sealdb_types::TimeType::Timestamp;
        return Ok(Datum::Time(t));
    }
    let Some(text) = col.default_expr.as_deref() else {
        // A strict insert that leaves a NOT NULL column without a default
        // fails (1364); generated and auto-increment columns are filled later.
        let not_null = col.field_type.flag & sealdb_types::field_type::NOT_NULL_FLAG != 0;
        if not_null
            && strict_sql_mode()
            && !col.auto_increment
            && col.auto_random.is_none()
            && col.generated_expr.is_none()
        {
            return Err(Error::new(
                ERR_FIELD_NOT_DEFAULT,
                format!("Field '{}' doesn't have a default value", col.name.o()),
            ));
        }
        return Ok(Datum::Null);
    };
    // A year default spelled `'0000'`/`'0'` is the zero year; the quoted
    // literal path would otherwise read `'0'` as the two-digit year 2000.
    if col.field_type.tp == TYPE_YEAR
        && let Some(inner) = text
            .trim()
            .strip_prefix('\'')
            .and_then(|s| s.strip_suffix('\''))
        && !inner.is_empty()
        && inner.chars().all(|c| c == '0')
    {
        return Ok(Datum::Year(0));
    }
    let expr = sealdb_parser::sqlparser_bridge::lower::parse_expr(text)?;
    let value = eval_expr_with_row(&expr, &[], &[])?;
    cast_for_column(&value, col)
}

fn is_default_keyword(expr: &sealdb_parser::ast::ExprNode) -> bool {
    matches!(
        expr,
        sealdb_parser::ast::ExprNode::Column(c)
            if c.table.is_none() && c.schema.is_none() && c.name.eq_ignore_ascii_case("default")
    )
}

/// `DEFAULT(col)` in a VALUES list: returns the referenced column name.
fn default_column_arg(expr: &sealdb_parser::ast::ExprNode) -> Option<&str> {
    let sealdb_parser::ast::ExprNode::Func(f) = expr else {
        return None;
    };
    if !f.name.eq_ignore_ascii_case("default") || f.args.len() != 1 {
        return None;
    }
    match &f.args[0] {
        sealdb_parser::ast::ExprNode::Column(c) if c.table.is_none() && c.schema.is_none() => {
            Some(c.name.as_str())
        }
        _ => None,
    }
}

fn map_insert_row(
    info: &sealdb_meta::TableInfo,
    columns: &[String],
    row: &[Datum],
    binary: &[bool],
    row_no: usize,
) -> Result<Vec<Datum>> {
    CAST_ROW_NO.with(|cell| cell.set(row_no));
    // `INSERT INTO t VALUES()` / `INSERT INTO t () VALUES ()`: one row built
    // entirely from column defaults.
    if columns.is_empty() && row.is_empty() {
        let mut values = vec![Datum::Null; info.columns.len()];
        for col in &info.columns {
            if col.hidden {
                continue;
            }
            values[col.offset as usize] = column_default_value(col)?;
        }
        fill_generated_columns(info, &mut values)?;
        return Ok(values);
    }
    if columns.is_empty() {
        // Hidden expression-index columns are not part of the user-visible
        // column list, so `INSERT INTO t VALUES (...)` counts only the rest.
        let visible: Vec<&sealdb_meta::ColumnInfo> =
            info.columns.iter().filter(|c| !c.hidden).collect();
        if row.len() != visible.len() {
            return Err(wrong_value_count_on_row(row_no));
        }
        // Cast literals to column FieldType (UNSIGNED BIGINT / DECIMAL scale).
        let mut values = vec![Datum::Null; info.columns.len()];
        for (idx, (col, v)) in visible.iter().zip(row.iter()).enumerate() {
            values[col.offset as usize] =
                cast_for_column_impl(v, col, binary.get(idx).copied().unwrap_or(false))?;
        }
        fill_generated_columns(info, &mut values)?;
        return Ok(values);
    }
    if row.len() != columns.len() {
        return Err(wrong_value_count_on_row(row_no));
    }
    let mut values = Vec::with_capacity(info.columns.len());
    for col in &info.columns {
        if let Some(idx) = columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(col.name.o()))
        {
            values.push(cast_for_column_impl(
                &row[idx],
                col,
                binary.get(idx).copied().unwrap_or(false),
            )?);
        } else {
            values.push(column_default_value(col)?);
        }
    }
    fill_generated_columns(info, &mut values)?;
    Ok(values)
}

/// Restrict `INSERT ... PARTITION (p...)` to the named partitions.
///
/// TiDB walks each row through the table's partitioning rule; rows whose
/// physical partition is not in the target list are dropped. `INSERT IGNORE`
/// turns the skip into `Warning 1748`, plain inserts fail.
fn filter_target_partitions(
    info: &sealdb_meta::TableInfo,
    names: &[String],
    pending: Vec<PendingRow>,
    ignore: bool,
) -> Result<Vec<PendingRow>> {
    let Some(part) = info.partition.as_ref() else {
        return Ok(pending);
    };
    let mut targets = Vec::with_capacity(names.len());
    for name in names {
        let matches: Vec<i64> = part
            .definitions
            .iter()
            .filter(|def| def.name.o().eq_ignore_ascii_case(name))
            .map(|def| def.id)
            .collect();
        if matches.is_empty() {
            return Err(Error::new(
                ERR_UNKNOWN_PARTITION,
                format!("Unknown partition '{name}' in table '{}'", info.name.o()),
            ));
        }
        targets.extend(matches);
    }
    let mut kept = Vec::with_capacity(pending.len());
    for row in pending {
        let target = match locate_partition_for_row(info, &row.values) {
            Ok(target) => target,
            Err(e) if ignore && e.code == ERR_NO_PARTITION_FOR_GIVEN_VALUE => {
                sealdb_expression::note_warning(e.code, e.message);
                continue;
            }
            Err(e) => return Err(e),
        };
        if targets.contains(&target) {
            kept.push(row);
        } else if ignore {
            sealdb_expression::note_warning(
                ERR_ROW_DOES_NOT_MATCH_GIVEN_PARTITION_SET,
                "Found a row not matching the given partition set".to_string(),
            );
        } else {
            return Err(Error::new(
                ERR_ROW_DOES_NOT_MATCH_GIVEN_PARTITION_SET,
                "Found a row not matching the given partition set",
            ));
        }
    }
    Ok(kept)
}

/// Assign `AUTO_INCREMENT` values (TiDB `allocAutoRandomID` / auto-id
/// fallback): `NULL`, `DEFAULT` and the literal `0` request a new id; an
/// explicit non-zero value raises the allocator so it is never reused.
fn fill_auto_increment(
    domain: &sealdb_domain::Domain,
    info: &sealdb_meta::TableInfo,
    values: &mut [Datum],
    has_odku: bool,
) -> Result<Option<i64>> {
    let mut generated = None;
    for col in &info.columns {
        if !col.auto_increment {
            continue;
        }
        let idx = col.offset as usize;
        let Some(current) = values.get(idx).cloned() else {
            continue;
        };
        let needs_alloc = match &current {
            Datum::Null => true,
            // `NO_AUTO_VALUE_ON_ZERO` keeps an explicit `0` as a real value;
            // an omitted column is `NULL` and still allocates.
            _ => {
                numeric_form(&current).is_some_and(|v| v == 0.0)
                    && !sealdb_expression::sql_mode_flag("NO_AUTO_VALUE_ON_ZERO")
            }
        };
        if needs_alloc {
            // TiDB `AUTO_ID_CACHE`: ids are handed out in batches of the
            // table option (1 by default).
            let next = domain.next_auto_id_with_cache(info.id, info.auto_id_cache.unwrap_or(1))?;
            if let Some(max) = auto_increment_ceiling(&col.field_type)
                && next > max
            {
                // Under `INSERT IGNORE` TiDB reports the exhausted allocator
                // as warnings (`constant <max+1> overflows <type>` from the
                // constant cast, then 1467) and keeps the row.
                if IGNORE_MODE.with(std::cell::Cell::get) {
                    sealdb_expression::note_warning(
                        sealdb_errno::ERR_DATA_OUT_OF_RANGE,
                        format!(
                            "constant {} overflows {}",
                            max + 1,
                            numeric_type_name(col.field_type.tp)
                        ),
                    );
                    sealdb_expression::note_warning(
                        ERR_AUTOINC_READ_FAILED,
                        "Failed to read auto-increment value from storage engine",
                    );
                    // `ON DUPLICATE KEY UPDATE` can still resolve the row, so
                    // the allocator saturates at the column maximum and lets
                    // the conflict path update it; otherwise the row keeps 0.
                    let fallback = if has_odku { max } else { 0 };
                    values[idx] = cast_for_column(&Datum::I64(fallback), col)?;
                    continue;
                }
                return Err(Error::new(
                    ERR_AUTOINC_READ_FAILED,
                    "Failed to read auto-increment value from storage engine",
                ));
            }
            values[idx] = cast_for_column(&Datum::I64(next), col)?;
            generated.get_or_insert(next);
        } else if let Some(v) = numeric_form(&current) {
            domain.bump_auto_id(info.id, v as i64)?;
        }
    }
    Ok(generated)
}

/// Assign `AUTO_RANDOM(n)` values (TiDB `allocAutoRandomID`): `NULL` and `0`
/// request a new id whose top `n` bits are XOR-shuffled with a pseudo-random
/// value; any other value is an explicit insert, which needs
/// `@@allow_auto_random_explicit_insert`.
fn fill_auto_random(
    domain: &sealdb_domain::Domain,
    info: &sealdb_meta::TableInfo,
    values: &mut [Datum],
) -> Result<()> {
    for col in &info.columns {
        let Some(bits) = col.auto_random else {
            continue;
        };
        let idx = col.offset as usize;
        let Some(current) = values.get(idx).cloned() else {
            continue;
        };
        let needs_alloc = match &current {
            Datum::Null => true,
            other => numeric_form(other).is_none_or(|v| v == 0.0),
        };
        if needs_alloc {
            let id = domain.next_auto_id(info.id)? as u64;
            let shard_mask = if bits == 0 || bits >= 63 {
                0
            } else {
                ((1u64 << bits) - 1) << (63 - bits)
            };
            // Deterministic shuffle: same input id, same stored value.
            let shard = id.wrapping_mul(0x9E37_79B9_7F4A_7C15) & shard_mask;
            let value = (shard | (id & !shard_mask)) as i64;
            values[idx] = cast_for_column(&Datum::I64(value), col)?;
        } else {
            if !sealdb_expression::allow_auto_random_explicit_insert() {
                return Err(Error::new(
                    ERR_INVALID_AUTO_RANDOM,
                    "Invalid auto random: Explicit insertion on auto_random column is disabled. \
                     Try to set @@allow_auto_random_explicit_insert = true.",
                ));
            }
            if let Some(v) = numeric_form(&current) {
                domain.bump_auto_id(info.id, v as i64)?;
            }
        }
    }
    Ok(())
}

/// `UPDATE` with an explicit `AUTO_INCREMENT` value raises the allocator so the
/// id is never handed out again (TiDB `allocAutoRandomID` semantics).
pub(crate) fn bump_auto_increment(
    domain: &sealdb_domain::Domain,
    info: &sealdb_meta::TableInfo,
    values: &[Datum],
) -> Result<()> {
    for col in &info.columns {
        if !col.auto_increment {
            continue;
        }
        if let Some(current) = values.get(col.offset as usize)
            && let Some(v) = numeric_form(current)
            && v > 0.0
        {
            domain.bump_auto_id(info.id, v as i64)?;
        }
    }
    Ok(())
}

/// Evaluate `GENERATED ALWAYS AS (expr)` columns in ascending offset order so a
/// generated column may reference earlier generated ones. Values are stored
/// physically (VIRTUAL columns included) until a lazy-read path lands.
pub(crate) fn fill_generated_columns(
    info: &sealdb_meta::TableInfo,
    values: &mut [Datum],
) -> Result<()> {
    if info.columns.iter().all(|c| c.generated_expr.is_none()) {
        return Ok(());
    }
    let schema: Vec<String> = info
        .columns
        .iter()
        .map(|c| c.name.o().to_string())
        .collect();
    let mut exprs = Vec::new();
    for col in &info.columns {
        let Some(text) = col.generated_expr.as_deref() else {
            continue;
        };
        let expr = sealdb_parser::sqlparser_bridge::lower::parse_expr(text)?;
        exprs.push((col.offset as usize, expr, col));
    }
    let previous = GENERATED_COLUMN.with(|flag| flag.replace(true));
    let result = (|| -> Result<()> {
        for (offset, expr, col) in exprs {
            if offset >= values.len() {
                continue;
            }
            // Multi-valued index part: materialize the JSON array as canonical
            // text so uniqueness checks can compare elements.
            if let sealdb_parser::ast::expressions::ExprNode::Cast(c) = &expr
                && c.target.trim().to_ascii_uppercase().ends_with("ARRAY")
            {
                let raw = eval_expr_with_row(&c.expr, &schema, values)?;
                values[offset] = Datum::Bytes(canonical_array_text(&raw).into_bytes());
                continue;
            }
            let value = eval_expr_with_row(&expr, &schema, values)?;
            // TiDB binary literals (`0x…` / `x'…'`) are strings in a character
            // context: `INSERT INTO vc VALUES (0x5c27090a)` stores the raw
            // bytes, not the decimal value of the literal.
            let value = match (&expr, &value) {
                (sealdb_parser::ast::expressions::ExprNode::Value(v), Datum::Bit(b))
                    if col.field_type.is_character_type() =>
                {
                    Datum::Bytes(b.clone())
                }
                _ => value,
            };
            values[offset] = cast_for_column(&value, col)?;
        }
        Ok(())
    })();
    GENERATED_COLUMN.with(|flag| flag.set(previous));
    result
}

pub(crate) fn cast_for_column(v: &Datum, col: &sealdb_meta::ColumnInfo) -> Result<Datum> {
    cast_for_column_impl(v, col, false)
}

/// `binary` marks a source value that is a binary string (`UNHEX`, `0x…`):
/// converting it to a `utf8` (utf8mb3) column truncates at the first
/// four-byte character instead of substituting `?`.
fn cast_for_column_impl(v: &Datum, col: &sealdb_meta::ColumnInfo, binary: bool) -> Result<Datum> {
    if matches!(col.field_type.tp, TYPE_ENUM | TYPE_SET) {
        return cast_enum_set(v, &col.field_type, col.name.o());
    }
    // A temporal string may carry an explicit zone (`…T16:31:15-10`, `…Z`).
    // DATETIME keeps the session-local wall clock of that instant; TIMESTAMP
    // stores the UTC instant itself.
    if matches!(
        col.field_type.tp,
        TYPE_DATE | TYPE_DATETIME | TYPE_TIMESTAMP
    ) && let Datum::Bytes(bytes) = v
        && let Some((wall, offset)) =
            sealdb_types::time::parse_datetime_with_zone(&String::from_utf8_lossy(bytes))?
    {
        let utc = sealdb_expression::shift_time_minutes(wall, -i64::from(offset))?;
        let mut t = if col.field_type.tp == TYPE_TIMESTAMP {
            utc
        } else {
            sealdb_expression::utc_to_session_local(utc)?
        };
        match col.field_type.tp {
            TYPE_DATE => {
                t.hour = 0;
                t.minute = 0;
                t.second = 0;
                t.micro = 0;
                t.fsp = 0;
                t.tp = sealdb_types::TimeType::Date;
            }
            TYPE_TIMESTAMP => t.tp = sealdb_types::TimeType::Timestamp,
            _ => t.tp = sealdb_types::TimeType::Datetime,
        }
        t = sealdb_types::time::round_to_fsp(t, col.field_type.decimal.clamp(0, 6) as i8);
        return Ok(Datum::Time(t));
    }
    // A character value the column charset cannot represent: strict mode fails
    // with 1366 `Incorrect string value`, otherwise the unconvertible
    // characters are stored as `?` with the same warning. Generated columns
    // always take the replacement form (TiDB evaluates them through the cast
    // path, which substitutes instead of aborting).
    if let Datum::Bytes(bytes) = v
        && col.field_type.is_character_type()
    {
        match charset_convert(&col.field_type.charset, bytes, binary) {
            CharsetOutcome::Fine => {}
            CharsetOutcome::Converted {
                offending,
                converted,
            } => {
                if GENERATED_COLUMN.with(std::cell::Cell::get) {
                    return Ok(Datum::Bytes(converted));
                }
                let err = invalid_charset_error(&offending, col.name.o());
                if strict_sql_mode() {
                    return Err(err);
                }
                sealdb_expression::note_warning(
                    ERR_TRUNCATED_WRONG_VALUE_FOR_FIELD,
                    err.message.clone(),
                );
                return Ok(Datum::Bytes(converted));
            }
        }
    }
    // CHAR/VARCHAR length is counted in characters (binary in bytes).
    if let Datum::Bytes(bytes) = v
        && matches!(
            col.field_type.tp,
            TYPE_STRING | TYPE_VAR_STRING | TYPE_VARCHAR
        )
    {
        let limit = col.field_type.flen.max(0) as usize;
        let count = if col.field_type.charset == "binary" {
            bytes.len()
        } else {
            String::from_utf8_lossy(bytes).chars().count()
        };
        if limit > 0 && count > limit {
            let text = String::from_utf8_lossy(bytes);
            // MySQL treats truncation that only drops trailing whitespace
            // specially: `CHAR` drops it silently, `VARCHAR` reports 1265
            // even under strict mode (`insert into vctt values ('ab\n\n\n', …)`).
            let trailing_ws = text.chars().skip(limit).all(|c| c.is_ascii_whitespace());
            if trailing_ws {
                if col.field_type.tp != TYPE_STRING {
                    sealdb_expression::note_warning(
                        ERR_WARN_DATA_TRUNCATED,
                        format!(
                            "Data truncated for column '{}' at row {}",
                            col.name.o(),
                            cast_row_no()
                        ),
                    );
                }
                let cut: String = text.chars().take(limit).collect();
                return Ok(Datum::Bytes(cut.into_bytes()));
            }
            let msg = format!("Data too long for column '{}' at row 1", col.name.o());
            if strict_sql_mode() {
                return Err(Error::new(ERR_DATA_TOO_LONG, msg));
            }
            sealdb_expression::note_warning(ERR_TRUNCATED_WRONG_VALUE_FOR_FIELD, msg);
            let cut: String = text.chars().take(limit).collect();
            return Ok(Datum::Bytes(cut.into_bytes()));
        }
    }
    // A BLOB/TEXT value longer than its storage class allows is 1406 in strict
    // mode; otherwise it is cut at a character boundary with the same warning.
    if let Datum::Bytes(bytes) = v
        && let Some(limit) = lob_byte_limit(&col.field_type)
        && bytes.len() > limit
    {
        let msg = format!("Data too long for column '{}' at row 1", col.name.o());
        if strict_sql_mode() {
            return Err(Error::new(ERR_DATA_TOO_LONG, msg));
        }
        sealdb_expression::note_warning(ERR_DATA_TOO_LONG, msg);
        let mut cut = &bytes[..limit];
        if col.field_type.charset != "binary" {
            while !cut.is_empty() && std::str::from_utf8(cut).is_err() {
                cut = &cut[..cut.len() - 1];
            }
        }
        return Ok(Datum::Bytes(cut.to_vec()));
    }
    // A string that has no numeric prefix at all cannot be converted to a
    // numeric column: `INSERT` reports MySQL 1366 `Incorrect <type> value`,
    // while `UPDATE`'s assignment cast reports 1292 (handled below).
    if let Datum::Bytes(bytes) = v
        && is_numeric_type(col.field_type.tp)
        && !UPDATE_ASSIGNMENT.with(std::cell::Cell::get)
        && let Some(outcome) = invalid_numeric_text(&String::from_utf8_lossy(bytes), col)?
    {
        return outcome;
    }
    // `UPDATE`'s assignment cast reports 1292 `Truncated incorrect DOUBLE
    // value` for a string with no numeric prefix (both INT and DOUBLE
    // columns), not the generic integer/float truncation error.
    if UPDATE_ASSIGNMENT.with(std::cell::Cell::get)
        && is_numeric_type(col.field_type.tp)
        && let Datum::Bytes(bytes) = v
    {
        let text = String::from_utf8_lossy(bytes);
        if !has_numeric_prefix(&text) {
            return Err(Error::new(
                ERR_TRUNCATED_WRONG_VALUE,
                format!("Truncated incorrect DOUBLE value: '{text}'"),
            ));
        }
    }
    // `ALLOW_INVALID_DATES` keeps a valid year/month with an out-of-range day
    // (`2004-04-31`); TiDB stores the wall date as written. Outside strict
    // mode a zero month/day (`2017-00-00`) is likewise kept instead of being
    // collapsed to the zero date.
    if matches!(col.field_type.tp, TYPE_DATE | TYPE_DATETIME)
        && let Datum::Bytes(bytes) = v
        && let Some(t) = relaxed_temporal(&String::from_utf8_lossy(bytes), col.field_type.tp)
        && (sealdb_expression::sql_mode_flag("ALLOW_INVALID_DATES")
            || ((t.month == 0 || t.day == 0)
                && !sealdb_expression::sql_mode_flag("NO_ZERO_IN_DATE"))
            || (t.year == 0 && !sealdb_expression::sql_mode_flag("NO_ZERO_DATE")))
    {
        return Ok(Datum::Time(t));
    }
    if is_integer_type(col.field_type.tp)
        && !v.is_null()
        && let Some(n) = raw_int_assignment(v)
    {
        return checked_int(n, col);
    }
    // Assigning an out-of-range YEAR is an assignment error (1264 with the
    // column name), not the 1690 of a plain `CAST(... AS YEAR)`.
    if col.field_type.tp == TYPE_YEAR
        && let Some(n) = raw_int_assignment(v)
        && !(n == 0
            || (1..=69).contains(&n)
            || (70..=99).contains(&n)
            || (1901..=2155).contains(&n))
    {
        let msg = format!(
            "Out of range value for column '{}' at row {}",
            col.name.o(),
            cast_row_no()
        );
        if strict_sql_mode() {
            return Err(Error::new(ERR_WARN_DATA_OUT_OF_RANGE, msg));
        }
        sealdb_expression::note_warning(ERR_WARN_DATA_OUT_OF_RANGE, msg);
        return Ok(Datum::Year(0));
    }
    // BIT(M): a numeric assignment must fit in M bits. `BIT(64)` reinterprets a
    // negative value as unsigned (`-1` is all ones), narrower widths reject it
    // and any value above 2^M-1 with 1406.
    if col.field_type.tp == TYPE_BIT
        && let Some(n) = bit_assignment_number(v)
    {
        let m = col.field_type.flen.clamp(1, 64);
        if m < 64 {
            let limit = (1i128 << m) - 1;
            if n < 0 || n > limit {
                let msg = format!("Data too long for column '{}' at row 1", col.name.o());
                if strict_sql_mode() {
                    return Err(Error::new(ERR_DATA_TOO_LONG, msg));
                }
                sealdb_expression::note_warning(ERR_DATA_TOO_LONG, msg);
            }
        } else if n > i128::from(u64::MAX) {
            let msg = format!(
                "Out of range value for column '{}' at row {}",
                col.name.o(),
                cast_row_no()
            );
            if strict_sql_mode() {
                return Err(Error::new(ERR_WARN_DATA_OUT_OF_RANGE, msg));
            }
            sealdb_expression::note_warning(ERR_WARN_DATA_OUT_OF_RANGE, msg);
        }
    }
    let mut out = match v.cast(&col.field_type) {
        Ok(out) => out,
        Err(err) => {
            let err = temporal_column_error(v, col, err);
            // Non-strict mode stores the zero date/time with warning 1292.
            if is_temporal(col.field_type.tp)
                && err.code == ERR_TRUNCATED_WRONG_VALUE
                && !strict_sql_mode()
            {
                sealdb_expression::note_warning(err.code, err.message.clone());
                return Ok(implicit_zero(&col.field_type));
            }
            return Err(err);
        }
    };
    // TiDB `handleZeroDatetime`: parsing accepts a zero month/day, but the
    // `NO_ZERO_IN_DATE` / `NO_ZERO_DATE` policy decides whether the row can be
    // written. Strict mode rejects the value with 1292; otherwise it is stored
    // as the zero date with a 1292 warning.
    if matches!(
        col.field_type.tp,
        TYPE_DATE | TYPE_DATETIME | sealdb_types::TYPE_TIMESTAMP
    ) && matches!(v, Datum::Bytes(_))
        && let Datum::Time(t) = out
    {
        let zero_date = t.year == 0 && t.month == 0 && t.day == 0;
        let zero_in_date = !zero_date && (t.month == 0 || t.day == 0);
        let rejected = if col.field_type.tp == sealdb_types::TYPE_TIMESTAMP {
            // A TIMESTAMP can never hold an invalid calendar date.
            zero_in_date
        } else {
            (zero_in_date && sealdb_expression::sql_mode_flag("NO_ZERO_IN_DATE"))
                || (zero_date && sealdb_expression::sql_mode_flag("NO_ZERO_DATE"))
        };
        if rejected {
            let err =
                temporal_column_error(v, col, Error::new(ERR_TRUNCATED_WRONG_VALUE, String::new()));
            if strict_sql_mode() {
                return Err(err);
            }
            sealdb_expression::note_warning(err.code, err.message.clone());
            return Ok(implicit_zero(&col.field_type));
        }
    }
    // TIMESTAMP columns store UTC; session wall clock → UTC on write. A wall
    // clock outside 1970-01-01 00:00:01 .. 2038-01-19 03:14:07 becomes the zero
    // timestamp (1292 warning) outside strict mode and fails with 1292 in it.
    if col.field_type.tp == sealdb_types::TYPE_TIMESTAMP
        && let Datum::Time(t) = out
    {
        if t.year == 0 {
            if sealdb_expression::sql_mode_flag("NO_ZERO_DATE") {
                // The message quotes the value as the user wrote it (`0`, not
                // the normalized zero timestamp). An UPDATE assignment reports
                // the value alone; INSERT/REPLACE append the column and row.
                let value_text = v.display();
                let msg = if UPDATE_ASSIGNMENT.with(std::cell::Cell::get) {
                    format!("Incorrect timestamp value: '{value_text}'")
                } else {
                    format!(
                        "Incorrect timestamp value: '{value_text}' for column '{}' at row {}",
                        col.name.o(),
                        cast_row_no()
                    )
                };
                if strict_sql_mode() {
                    return Err(Error::new(ERR_TRUNCATED_WRONG_VALUE, msg));
                }
                sealdb_expression::note_warning(ERR_TRUNCATED_WRONG_VALUE, msg);
            }
        } else {
            // A wall clock in a DST spring-forward gap does not exist; MySQL
            // rejects it in strict mode and moves it past the gap otherwise.
            if !sealdb_expression::session_local_time_exists(&t) {
                let msg = format!(
                    "Incorrect timestamp value: '{t}' for column '{}' at row 1",
                    col.name.o()
                );
                if strict_sql_mode() {
                    return Err(Error::new(ERR_TRUNCATED_WRONG_VALUE, msg));
                }
                sealdb_expression::note_warning(ERR_TRUNCATED_WRONG_VALUE, msg);
                let mut shifted = sealdb_expression::session_local_to_utc_gap_shift(t)?;
                shifted.tp = sealdb_types::TimeType::Timestamp;
                return Ok(Datum::Time(shifted));
            }
            let utc = sealdb_expression::session_local_to_utc(t).unwrap_or(t);
            let stamp = (
                utc.year, utc.month, utc.day, utc.hour, utc.minute, utc.second,
            );
            if stamp <= (1970, 1, 1, 0, 0, 0) || stamp > (2038, 1, 19, 3, 14, 7) {
                let msg = format!(
                    "Incorrect timestamp value: '{t}' for column '{}' at row 1",
                    col.name.o()
                );
                if strict_sql_mode() {
                    return Err(Error::new(ERR_TRUNCATED_WRONG_VALUE, msg));
                }
                sealdb_expression::note_warning(ERR_TRUNCATED_WRONG_VALUE, msg);
                let mut zero = sealdb_types::time::Time::new(0, 0, 0, 0, 0, 0);
                zero.tp = sealdb_types::TimeType::Timestamp;
                return Ok(Datum::Time(zero));
            }
        }
        let mut t = t;
        t.tp = sealdb_types::TimeType::Timestamp;
        t = sealdb_expression::session_local_to_utc(t)?;
        t.tp = sealdb_types::TimeType::Timestamp;
        out = Datum::Time(t);
    }
    check_decimal_scale(v, col)?;
    check_numeric_range(v, out, col)
}

/// TiDB `CastColumnValue`: assigning a value with more fractional digits than
/// the column scale truncates and reports 1292 (`Truncated incorrect DECIMAL
/// value: '…'`), as a warning outside strict mode and an error inside it.
fn check_decimal_scale(raw: &Datum, col: &sealdb_meta::ColumnInfo) -> Result<()> {
    let ft = &col.field_type;
    if !matches!(ft.tp, TYPE_DECIMAL | TYPE_NEW_DECIMAL) || ft.decimal < 0 || raw.is_null() {
        return Ok(());
    }
    let text = match raw {
        Datum::Decimal(d) => d.to_string(),
        Datum::F64(v) => format!("{v}"),
        Datum::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        _ => return Ok(()),
    };
    // An exponent form (`12e-3`) hides its fractional digits behind the shift:
    // TiDB rounds the expanded value (`0.012`, three fractional digits) but
    // reports the original spelling in the warning.
    let expanded = if text.contains(['e', 'E']) {
        sealdb_types::MyDecimal::parse(text.trim())
            .map(|d| d.to_string())
            .unwrap_or_else(|_| text.clone())
    } else {
        text.clone()
    };
    let (frac_text, frac_digits) = match expanded.split_once('.') {
        Some((_, frac)) => {
            let digits = frac.chars().take_while(|c| c.is_ascii_digit()).count();
            (frac, digits)
        }
        None => ("", 0),
    };
    let scale = ft.decimal.max(0) as usize;
    if frac_digits <= scale {
        return Ok(());
    }
    // Discarding only trailing zeros (`1.000000` into DECIMAL(4,2)) changes
    // neither the value nor TiDB's truncation flag, so no warning is raised.
    if frac_text[scale..frac_digits].bytes().all(|b| b == b'0') {
        return Ok(());
    }
    // An INSERT reports the value-conversion form (1366 with the column
    // name); an UPDATE assignment reports 1292 `Truncated incorrect DECIMAL
    // value`. Both keep the truncated value and warn even in strict mode.
    if UPDATE_ASSIGNMENT.with(std::cell::Cell::get) {
        sealdb_expression::note_warning(
            ERR_TRUNCATED_WRONG_VALUE,
            format!("Truncated incorrect DECIMAL value: '{text}'"),
        );
    } else {
        let msg = format!(
            "Incorrect decimal value: '{text}' for column '{}' at row 1",
            col.name.o()
        );
        sealdb_expression::note_warning(ERR_TRUNCATED_WRONG_VALUE_FOR_FIELD, msg);
    }
    Ok(())
}

/// `FLOAT(M,D)` / `DOUBLE(M,D)` and integer range checks, mirroring TiDB's
/// `ProduceFloat64` / `ProduceInteger`: an out-of-range assignment is clamped
/// with warning 1264 outside strict mode and fails with 1264 inside it.
fn check_numeric_range(raw: &Datum, out: Datum, col: &sealdb_meta::ColumnInfo) -> Result<Datum> {
    let ft = &col.field_type;
    let msg = || {
        format!(
            "Out of range value for column '{}' at row {}",
            col.name.o(),
            cast_row_no()
        )
    };
    if is_integer_type(ft.tp)
        && let Some(v) = raw_int_assignment(raw)
    {
        return checked_int(v, col);
    }
    // DECIMAL precision: more integer digits than `M - D` is out of range.
    if matches!(ft.tp, TYPE_DECIMAL | TYPE_NEW_DECIMAL) {
        let text = match raw {
            Datum::Decimal(d) => d.to_string(),
            Datum::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
            other => other.display(),
        };
        // An exponent form (`1e30`) carries zeros the raw text does not show;
        // expand it so the integer-digit count matches the stored value.
        let text = if text.contains(['e', 'E']) {
            sealdb_types::MyDecimal::parse(text.trim()).map_or(text, |d| d.to_string())
        } else {
            text
        };
        // An UNSIGNED DECIMAL keeps a negative assignment as its unsigned
        // zero (TiDB `ProduceDecimal`): 1264 outside strict mode, an error
        // inside it.
        if ft.is_unsigned() && text.trim_start().starts_with('-') {
            if strict_sql_mode() {
                return Err(Error::new(ERR_WARN_DATA_OUT_OF_RANGE, msg()));
            }
            sealdb_expression::note_warning(ERR_WARN_DATA_OUT_OF_RANGE, msg());
            let zero = sealdb_types::MyDecimal::from_i64(0).with_frac(ft.decimal);
            return Ok(Datum::Decimal(zero));
        }
        let int_digits = text
            .trim()
            .trim_start_matches(['+', '-'])
            .split('.')
            .next()
            .unwrap_or("")
            .trim_start_matches('0')
            .len();
        // MyDecimal keeps at most 81 digits; a longer literal is truncated from
        // the left with warning 1292.
        let trimmed = text.trim();
        let sign_len = usize::from(trimmed.starts_with(['+', '-']));
        let magnitude = trimmed[sign_len..].replace('.', "");
        let truncated_value = (magnitude.len() > 81).then(|| {
            format!(
                "{}{}",
                &trimmed[..sign_len],
                &magnitude[magnitude.len() - 81..]
            )
        });
        let max_int = (ft.flen - ft.decimal.max(0)).max(0) as usize;
        if int_digits > max_int {
            if strict_sql_mode() {
                return Err(Error::new(ERR_WARN_DATA_OUT_OF_RANGE, msg()));
            }
            sealdb_expression::note_warning(ERR_WARN_DATA_OUT_OF_RANGE, msg());
            if let Some(value) = truncated_value {
                sealdb_expression::note_warning(
                    ERR_TRUNCATED_WRONG_VALUE,
                    format!("Truncated incorrect DECIMAL value: '{value}'"),
                );
            }
            let scale = ft.decimal.max(0) as usize;
            let mut clamped = if max_int == 0 {
                "0".to_string()
            } else {
                "9".repeat(max_int)
            };
            if scale > 0 {
                clamped.push('.');
                clamped.push_str(&"9".repeat(scale));
            }
            if text.trim_start().starts_with('-') {
                clamped.insert(0, '-');
            }
            if let Ok(d) = sealdb_types::MyDecimal::parse(&clamped) {
                return Ok(Datum::Decimal(d.with_frac(ft.decimal)));
            }
        }
        return Ok(out);
    }
    if !matches!(ft.tp, TYPE_FLOAT | TYPE_DOUBLE) {
        return Ok(out);
    }
    let Datum::F64(v) = out else {
        return Ok(out);
    };
    if !v.is_finite() {
        // The literal overflowed the storage width (`FLOAT` narrows to f32).
        if strict_sql_mode() {
            return Err(Error::new(ERR_WARN_DATA_OUT_OF_RANGE, msg()));
        }
        sealdb_expression::note_warning(ERR_WARN_DATA_OUT_OF_RANGE, msg());
        let max = if ft.tp == TYPE_FLOAT {
            f64::from(f32::MAX)
        } else {
            f64::MAX
        };
        return Ok(Datum::F64(if v.is_sign_negative() { -max } else { max }));
    }
    // An unsigned float keeps a negative value as its unsigned zero, but a
    // strict-mode insert reports it as out of range.
    if ft.is_unsigned() && v < 0.0 {
        if strict_sql_mode() {
            return Err(Error::new(ERR_WARN_DATA_OUT_OF_RANGE, msg()));
        }
        sealdb_expression::note_warning(ERR_WARN_DATA_OUT_OF_RANGE, msg());
        return Ok(Datum::F64(0.0));
    }
    if ft.flen <= 0 || ft.decimal < 0 {
        return Ok(out);
    }
    let m = i32::from(ft.flen);
    let d = i32::from(ft.decimal.clamp(0, 30));
    if d > m {
        return Ok(out);
    }
    let max = if d == 0 {
        10f64.powi(m) - 1.0
    } else {
        10f64.powi(m - d) - 10f64.powi(-d)
    };
    let rounded = if d == 0 {
        v.round()
    } else {
        let scale = 10f64.powi(d);
        (v * scale).round() / scale
    };
    if rounded.abs() <= max + f64::EPSILON {
        return Ok(Datum::F64(rounded));
    }
    if strict_sql_mode() {
        return Err(Error::new(ERR_WARN_DATA_OUT_OF_RANGE, msg()));
    }
    sealdb_expression::note_warning(ERR_WARN_DATA_OUT_OF_RANGE, msg());
    Ok(Datum::F64(if v < 0.0 { -max } else { max }))
}

fn is_integer_type(tp: u8) -> bool {
    matches!(
        tp,
        TYPE_TINY | TYPE_SHORT | TYPE_INT24 | TYPE_LONG | TYPE_LONG_LONG
    )
}

/// Numeric value of an assignment to an integer column. MySQL rounds a
/// fractional number (away from zero: `5.5` → 6) and parses the leading
/// numeric prefix of a string.
fn raw_int_assignment(d: &Datum) -> Option<i128> {
    match d {
        Datum::I64(v) => Some(i128::from(*v)),
        Datum::U64(v) => Some(i128::from(*v)),
        Datum::Year(v) => Some(i128::from(*v)),
        Datum::F64(v) if v.is_finite() => Some(v.round() as i128),
        Datum::Decimal(d) => d.to_f64().ok().map(|v| v.round() as i128),
        Datum::Bytes(b) => {
            let text = String::from_utf8_lossy(b);
            let text = text.trim();
            // Scientific notation (`'1e100'`) is a complete float literal;
            // convert it as a whole so an out-of-range value is caught
            // (`1264`) instead of keeping its leading digits.
            if let Ok(v) = text.parse::<f64>() {
                return Some(v.round() as i128);
            }
            let mut end = 0;
            for (i, c) in text.char_indices() {
                let ok = c.is_ascii_digit() || (c == '.' && i > 0) || (c == '-' && i == 0);
                if !ok {
                    break;
                }
                end = i + c.len_utf8();
            }
            text[..end].parse::<f64>().ok().map(|v| v.round() as i128)
        }
        _ => None,
    }
}

/// Exact integer value of an assignment to a `BIT(M)` column. Strings must be
/// fully numeric (`'9'`); a binary literal keeps its byte semantics instead.
fn bit_assignment_number(d: &Datum) -> Option<i128> {
    match d {
        Datum::I64(v) => Some(i128::from(*v)),
        Datum::U64(v) => Some(i128::from(*v)),
        Datum::Year(v) => Some(i128::from(*v)),
        Datum::F64(v) if v.is_finite() => Some(v.trunc() as i128),
        Datum::Decimal(d) => d
            .to_string()
            .parse::<i128>()
            .ok()
            .or_else(|| d.to_f64().ok().map(|v| v.trunc() as i128)),
        Datum::Bytes(b) => {
            let text = String::from_utf8_lossy(b);
            let text = text.trim();
            let digits = text.strip_prefix('-').unwrap_or(text);
            if !digits.is_empty() && digits.bytes().all(|c| c.is_ascii_digit()) {
                text.parse::<i128>().ok()
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Byte capacity of a BLOB/TEXT storage class. A length-less `VAR_STRING` with
/// a character set is MySQL `TEXT` (65,535 bytes).
fn lob_byte_limit(ft: &FieldType) -> Option<usize> {
    match ft.tp {
        TYPE_TINY_BLOB => Some(255),
        TYPE_BLOB => Some(65_535),
        TYPE_MEDIUM_BLOB => Some(16_777_215),
        TYPE_LONG_BLOB => Some(u32::MAX as usize),
        TYPE_VAR_STRING if ft.flen < 0 && ft.charset != "binary" => Some(65_535),
        _ => None,
    }
}

/// Largest value an AUTO_INCREMENT integer column can hand out; `None` for a
/// non-integer type.
fn auto_increment_ceiling(ft: &FieldType) -> Option<i64> {
    let bits = match ft.tp {
        TYPE_TINY => 8,
        TYPE_SHORT => 16,
        TYPE_INT24 => 24,
        TYPE_LONG => 32,
        TYPE_LONG_LONG => 64,
        _ => return None,
    };
    Some(if ft.is_unsigned() {
        if bits == 64 {
            i64::MAX
        } else {
            (1i64 << bits) - 1
        }
    } else if bits == 64 {
        i64::MAX
    } else {
        (1i64 << (bits - 1)) - 1
    })
}

/// Apply TiDB's integer range clamp (`1264` strict error; clamp + warning
/// outside strict mode).
fn is_numeric_type(tp: u8) -> bool {
    is_integer_type(tp)
        || matches!(
            tp,
            TYPE_FLOAT | TYPE_DOUBLE | TYPE_DECIMAL | TYPE_NEW_DECIMAL
        )
}

fn numeric_type_name(tp: u8) -> &'static str {
    match tp {
        TYPE_TINY => "tinyint",
        TYPE_SHORT => "smallint",
        TYPE_INT24 => "mediumint",
        TYPE_LONG => "int",
        TYPE_LONG_LONG => "bigint",
        TYPE_FLOAT => "float",
        TYPE_DOUBLE => "double",
        TYPE_DECIMAL | TYPE_NEW_DECIMAL => "decimal",
        _ => "value",
    }
}

/// Whether `text` starts with an optionally signed number (digits, at most one
/// decimal point) — the prefix TiDB's string→number conversion consumes.
fn has_numeric_prefix(text: &str) -> bool {
    let trimmed = text.trim_start();
    let rest = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
    let mut digits = 0;
    let mut dots = 0;
    for c in rest.chars() {
        match c {
            '0'..='9' => digits += 1,
            '.' if dots == 0 => dots += 1,
            _ => break,
        }
    }
    digits > 0
}

/// A plain decimal literal (`-1.25`), which `DECIMAL` stores exactly.
fn is_plain_decimal(text: &str) -> bool {
    let trimmed = text.trim();
    let rest = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
    if rest.is_empty() {
        return false;
    }
    let mut parts = rest.split('.');
    let int = parts.next().unwrap_or("");
    let frac = parts.next();
    if parts.next().is_some() {
        return false;
    }
    let all_digits = |s: &str| s.chars().all(|c| c.is_ascii_digit());
    all_digits(int)
        && frac.is_none_or(all_digits)
        && (int.chars().count() + frac.map(str::len).unwrap_or(0) > 0)
}

/// Whether `text` is a well-formed decimal with an exponent (`12e-3`,
/// `1.5E+2`). TiDB's `MyDecimal.FromString` shifts the mantissa by the
/// exponent, so such a literal is a valid DECIMAL value.
fn is_exponent_form(text: &str) -> bool {
    let trimmed = text.trim();
    let rest = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
    let Some((mantissa, exponent)) = rest.split_once(['e', 'E']) else {
        return false;
    };
    let exponent = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
    if exponent.is_empty() || !exponent.bytes().all(|c| c.is_ascii_digit()) {
        return false;
    }
    is_plain_decimal(mantissa)
}

/// `Some(Ok(zero))` / `Some(warning)` for a string the target numeric type
/// rejects outright, `None` when the normal cast should handle it.
fn invalid_numeric_text(
    text: &str,
    col: &sealdb_meta::ColumnInfo,
) -> Result<Option<Result<Datum>>> {
    let ft = &col.field_type;
    if ft.tp == TYPE_DECIMAL || ft.tp == TYPE_NEW_DECIMAL {
        if is_plain_decimal(text) || is_exponent_form(text) {
            return Ok(None);
        }
        let msg = format!(
            "Incorrect decimal value: '{}' for column '{}' at row 1",
            sealdb_errno::error_value(text),
            col.name.o()
        );
        if strict_sql_mode() {
            return Ok(Some(Err(Error::new(
                sealdb_errno::ERR_TRUNCATED_WRONG_VALUE_FOR_FIELD,
                msg,
            ))));
        }
        sealdb_expression::note_warning(sealdb_errno::ERR_TRUNCATED_WRONG_VALUE_FOR_FIELD, msg);
        return Ok(Some(Ok(implicit_zero(&ft))));
    }
    if has_numeric_prefix(text) {
        return Ok(None);
    }
    let msg = format!(
        "Incorrect {} value: '{}' for column '{}' at row 1",
        numeric_type_name(ft.tp),
        sealdb_errno::error_value(text),
        col.name.o()
    );
    if strict_sql_mode() {
        return Ok(Some(Err(Error::new(
            sealdb_errno::ERR_TRUNCATED_WRONG_VALUE_FOR_FIELD,
            msg,
        ))));
    }
    sealdb_expression::note_warning(sealdb_errno::ERR_TRUNCATED_WRONG_VALUE_FOR_FIELD, msg);
    Ok(Some(Ok(implicit_zero(&ft))))
}

fn checked_int(v: i128, col: &sealdb_meta::ColumnInfo) -> Result<Datum> {
    let ft = &col.field_type;
    let bits = match ft.tp {
        TYPE_TINY => 8,
        TYPE_SHORT => 16,
        TYPE_INT24 => 24,
        TYPE_LONG => 32,
        TYPE_LONG_LONG => 64,
        _ => return Ok(Datum::I64(v as i64)),
    };
    let unsigned = ft.is_unsigned();
    let (min, max): (i128, i128) = if unsigned {
        (0, (1i128 << bits) - 1)
    } else {
        (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1)
    };
    if v >= min && v <= max {
        return Ok(if unsigned {
            Datum::U64(v as u64)
        } else {
            Datum::I64(v as i64)
        });
    }
    let msg = format!(
        "Out of range value for column '{}' at row {}",
        col.name.o(),
        cast_row_no()
    );
    if strict_sql_mode() {
        return Err(Error::new(ERR_WARN_DATA_OUT_OF_RANGE, msg));
    }
    // An `INSERT … SELECT` / `UPDATE` negative value is a constant cast:
    // TiDB reports 1690 (`constant -1 overflows bigint`) instead of 1264.
    if unsigned
        && v < 0
        && (INSERT_SELECT_SOURCE.with(std::cell::Cell::get)
            || UPDATE_ASSIGNMENT.with(std::cell::Cell::get))
    {
        sealdb_expression::note_warning(
            ERR_DATA_OUT_OF_RANGE,
            format!("constant {v} overflows {}", numeric_type_name(ft.tp)),
        );
    } else {
        sealdb_expression::note_warning(ERR_WARN_DATA_OUT_OF_RANGE, msg);
    }
    let clamped = v.clamp(min, max);
    Ok(if unsigned {
        Datum::U64(clamped as u64)
    } else {
        Datum::I64(clamped as i64)
    })
}

/// TiDB `sql_mode` strictness for DML value coercion.
fn strict_sql_mode() -> bool {
    !IGNORE_MODE.with(std::cell::Cell::get)
        && (sealdb_expression::sql_mode_flag("STRICT_TRANS_TABLES")
            || sealdb_expression::sql_mode_flag("STRICT_ALL_TABLES"))
}

fn is_temporal(tp: u8) -> bool {
    matches!(tp, TYPE_DATE | TYPE_DATETIME | TYPE_TIMESTAMP)
}

/// TiDB `ErrTruncatedWrongValueForField` for a DATE/DATETIME column: the
/// message names the column and the 1-based row of the failing value.
fn temporal_column_error(v: &Datum, col: &sealdb_meta::ColumnInfo, err: Error) -> Error {
    if err.code != ERR_TRUNCATED_WRONG_VALUE {
        return err;
    }
    // A negative number has no TIME form: TiDB reports `Incorrect time value`
    // with the value as written, not the truncated-clock-value wording.
    if col.field_type.tp == sealdb_types::TYPE_DURATION
        && !matches!(v, Datum::Bytes(_))
        && !v.is_null()
    {
        let text = v.display();
        if text.trim_start().starts_with('-') {
            return Error::new(
                ERR_TRUNCATED_WRONG_VALUE,
                format!(
                    "Incorrect time value: '{text}' for column '{}' at row 1",
                    col.name.o()
                ),
            );
        }
        return err;
    }
    if !is_temporal(col.field_type.tp) {
        return err;
    }
    let Datum::Bytes(bytes) = v else {
        return err;
    };
    let kind = match col.field_type.tp {
        TYPE_DATE => "date",
        TYPE_TIMESTAMP => "timestamp",
        _ => "datetime",
    };
    Error::new(
        ERR_TRUNCATED_WRONG_VALUE,
        format!(
            "Incorrect {kind} value: '{}' for column '{}' at row 1",
            String::from_utf8_lossy(bytes),
            col.name.o()
        ),
    )
}

/// `ALLOW_INVALID_DATES`: accept `YYYY-MM-DD[ HH:MM:SS[.ffffff]]` while only
/// range-checking month (1-12) and day (1-31), the two fields MySQL keeps
/// strict under that mode.
fn relaxed_temporal(s: &str, tp: u8) -> Option<sealdb_types::Time> {
    let s = s.trim();
    let (date, clock) = match s.split_once([' ', 'T']) {
        Some((date, clock)) => (date, Some(clock.trim())),
        None => (s, None),
    };
    let mut date_parts = date.split('-');
    let year: u16 = date_parts.next()?.parse().ok()?;
    let month: u8 = date_parts.next()?.parse().ok()?;
    let day: u8 = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some() || month > 12 || day > 31 {
        return None;
    }
    let (hour, minute, second, micro) = match clock {
        Some(clock) => {
            let mut clock_parts = clock.split(':');
            let hour: u8 = clock_parts.next()?.parse().ok()?;
            let minute: u8 = clock_parts.next()?.parse().ok()?;
            let sec_part = clock_parts.next()?;
            let (sec, frac) = match sec_part.split_once('.') {
                Some((sec, frac)) => (sec, Some(frac)),
                None => (sec_part, None),
            };
            let second: u8 = sec.parse().ok()?;
            if clock_parts.next().is_some() || hour > 23 || minute > 59 || second > 59 {
                return None;
            }
            let micro = match frac {
                Some(frac) if !frac.is_empty() && frac.bytes().all(|b| b.is_ascii_digit()) => {
                    let digits: String = frac.chars().take(6).collect();
                    let padded = format!("{digits:0<6}");
                    padded.parse::<u32>().ok()?
                }
                Some(_) => return None,
                None => 0,
            };
            (hour, minute, second, micro)
        }
        None => (0, 0, 0, 0),
    };
    Some(sealdb_types::Time {
        year,
        month,
        day,
        hour: hour as u16,
        minute,
        second,
        micro,
        fsp: if micro == 0 { 0 } else { 6 },
        neg: false,
        tp: match tp {
            TYPE_DATE => sealdb_types::TimeType::Date,
            TYPE_TIMESTAMP => sealdb_types::TimeType::Timestamp,
            _ => sealdb_types::TimeType::Datetime,
        },
    })
}

/// MySQL treats numbers (and numeric strings) as ENUM/SET indexes.
fn numeric_form(v: &Datum) -> Option<f64> {
    match v {
        Datum::I64(i) => Some(*i as f64),
        Datum::U64(u) => Some(*u as f64),
        Datum::F64(f) => Some(*f),
        Datum::Decimal(d) => d.to_f64().ok(),
        Datum::Bytes(b) => {
            let s = std::str::from_utf8(b).ok()?.trim();
            if s.is_empty() {
                return None;
            }
            s.parse::<f64>().ok()
        }
        _ => None,
    }
}

fn label_index(elems: &[String], s: &str) -> Option<usize> {
    elems
        .iter()
        .position(|e| e == s)
        .or_else(|| elems.iter().position(|e| e.eq_ignore_ascii_case(s)))
}

/// ENUM/SET value coercion (`insert.go` `getColDefaultValue` /
/// `FieldType.HandleWrongValue`): numeric → index, label → member, invalid →
/// `''` with a warning (strict mode errors).
fn cast_enum_set(v: &Datum, ft: &FieldType, col: &str) -> Result<Datum> {
    if v.is_null() {
        return Ok(Datum::Null);
    }
    let elems = ft.elems.as_deref().unwrap_or(&[]);
    if ft.tp == TYPE_ENUM {
        // A string literal that matches a member is that member (`b = '4'`
        // picks the label `4`, not index 4); numeric values and unmatched
        // numeric strings are indexes. A hex/bit literal compares by its raw
        // bytes, so `enum(0x61)` selects the label `a`.
        // A value already coerced to this ENUM (e.g. `VALUES(col)` in
        // `ON DUPLICATE KEY UPDATE`) carries its label, not an index.
        if matches!(v, Datum::Bytes(_) | Datum::Bit(_) | Datum::Enum(_)) {
            let s = String::from_utf8_lossy(&v.as_bytes()).into_owned();
            if s.is_empty() {
                return Ok(Datum::Enum(EnumValue::new(Vec::new(), 0)));
            }
            if let Some(i) = label_index(elems, &s) {
                return Ok(Datum::Enum(EnumValue::new(
                    elems[i].clone().into_bytes(),
                    i as u64 + 1,
                )));
            }
        }
        if let Some(n) = numeric_form(v) {
            let idx = n.round() as i64;
            if idx >= 1 && (idx as usize) <= elems.len() {
                return Ok(Datum::Enum(EnumValue::new(
                    elems[idx as usize - 1].clone().into_bytes(),
                    idx as u64,
                )));
            }
            return invalid_enum_set_value(v, col, Datum::Enum(EnumValue::new(Vec::new(), 0)));
        }
        let s = String::from_utf8_lossy(&v.as_bytes()).into_owned();
        if s.is_empty() {
            return Ok(Datum::Enum(EnumValue::new(Vec::new(), 0)));
        }
        return invalid_enum_set_value(v, col, Datum::Enum(EnumValue::new(Vec::new(), 0)));
    }

    // SET: numeric value is the bitmask; strings are comma-separated members.
    let mut bits: u64 = 0;
    if !matches!(v, Datum::Bytes(_) | Datum::Bit(_))
        && let Some(n) = numeric_form(v)
    {
        bits = if n <= 0.0 { 0 } else { n.round() as u64 };
    } else {
        let s = String::from_utf8_lossy(&v.as_bytes()).into_owned();
        if s.is_empty() {
            return Ok(Datum::Enum(EnumValue::new(Vec::new(), 0)));
        }
        for part in s.split(',') {
            match label_index(elems, part) {
                Some(i) if i < 64 => bits |= 1u64 << i,
                _ => {
                    // MySQL drops unknown members with a truncation warning.
                    if strict_sql_mode() {
                        return Err(truncated_error(v, col));
                    }
                    sealdb_expression::note_warning(
                        ERR_WARN_DATA_TRUNCATED,
                        format!("Data truncated for column '{col}' at row 1"),
                    );
                }
            }
        }
    }
    let valid_bits = if elems.len() >= 64 {
        u64::MAX
    } else {
        (1u64 << elems.len()) - 1
    };
    if bits & !valid_bits != 0 {
        if strict_sql_mode() {
            return Err(truncated_error(v, col));
        }
        sealdb_expression::note_warning(
            ERR_WARN_DATA_TRUNCATED,
            format!("Data truncated for column '{col}' at row 1"),
        );
        bits &= valid_bits;
    }
    let mut labels: Vec<&str> = Vec::new();
    for (i, e) in elems.iter().enumerate() {
        if bits & (1u64 << i) != 0 {
            labels.push(e.as_str());
        }
    }
    Ok(Datum::Enum(EnumValue::new(
        labels.join(",").into_bytes(),
        bits,
    )))
}

/// Result of converting a UTF-8 string to a column charset.
enum CharsetOutcome {
    Fine,
    /// At least one character (or raw byte run) is not representable.
    /// `offending` is the first such piece for the 1366 message, `converted`
    /// the storage form with `?` replacements (and any invalid UTF-8 tail cut).
    Converted {
        offending: Vec<u8>,
        converted: Vec<u8>,
    },
}

/// Convert a UTF-8 value to the representation stored for `charset`.
///
/// SealDB stores strings as UTF-8, so conversion only validates (and repairs)
/// the value: characters the charset cannot encode become MySQL's `?`
/// replacement. A malformed UTF-8 sequence ends the value at the last valid
/// boundary, mirroring TiDB's `Charset.Convert` (the valid prefix is kept).
fn charset_convert(charset: &str, bytes: &[u8], binary: bool) -> CharsetOutcome {
    let charset = charset.to_ascii_lowercase();
    if charset == "binary" {
        return CharsetOutcome::Fine;
    }
    // `@@tidb_skip_utf8_check` / `@@tidb_skip_ascii_check` disable the
    // corresponding 1366 validation; the bytes are stored verbatim.
    let skip_utf8_validity =
        matches!(charset.as_str(), "utf8" | "utf8mb4") && sealdb_expression::skip_utf8_check();
    let skip_ascii_validity = charset == "ascii" && sealdb_expression::skip_ascii_check();
    let allow_mb4 = !sealdb_expression::check_mb4_value();
    let mut converted = Vec::with_capacity(bytes.len());
    let mut offending: Option<Vec<u8>> = None;
    match std::str::from_utf8(bytes) {
        Ok(text) => {
            for ch in text.chars() {
                let representable = match charset.as_str() {
                    "ascii" => skip_ascii_validity || ch.is_ascii(),
                    // utf8 is MySQL's utf8mb3: only the BMP is representable
                    // unless `tidb_check_mb4_value_in_utf8` is OFF.
                    "utf8" => allow_mb4 || ch.len_utf8() <= 3,
                    // The shipped GBK table is a small subset; accept the
                    // Unicode blocks GBK covers so ordinary CJK text is not
                    // mistaken for an unconvertible character.
                    "gbk" => {
                        ch.is_ascii()
                            || matches!(
                                ch as u32,
                                0x3000..=0x303F
                                    | 0x3040..=0x30FF
                                    | 0x4E00..=0x9FA5
                                    | 0xFF01..=0xFF5E
                            )
                            || sealdb_parser::charset::encoding_gbk::UNICODE_GBK_SUBSET
                                .iter()
                                .any(|(c, _)| *c == ch)
                    }
                    _ => true,
                };
                if representable {
                    let mut buf = [0u8; 4];
                    converted.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                } else {
                    if offending.is_none() {
                        let mut buf = [0u8; 4];
                        offending = Some(ch.encode_utf8(&mut buf).as_bytes().to_vec());
                    }
                    // A binary source (`UNHEX`, `0x…`) into a `utf8` (utf8mb3)
                    // column is truncated at the four-byte character; a
                    // character source substitutes `?` instead.
                    if charset == "utf8" && !allow_mb4 && binary {
                        break;
                    }
                    converted.push(b'?');
                }
            }
        }
        Err(err) => {
            // Invalid UTF-8 bytes are stored verbatim when the column's charset
            // check is disabled.
            if skip_utf8_validity || skip_ascii_validity {
                return CharsetOutcome::Fine;
            }
            let valid = err.valid_up_to();
            converted.extend_from_slice(&bytes[..valid]);
            // ASCII conversion names the whole invalid tail (TiDB ascii
            // encoder reports every remaining byte); the UTF-8 checkpoints
            // report the first bad byte.
            let tail = &bytes[valid..];
            offending = Some(match charset.as_str() {
                // ASCII conversion names the whole invalid tail (TiDB ascii
                // encoder reports every remaining byte).
                "ascii" => tail.to_vec(),
                // GBK reads a lead byte plus its trail byte as one unit, so a
                // trailing `\xFF\xFF` is one unrepresentable chunk.
                "gbk" if tail[0] >= 0x80 => tail[..tail.len().min(2)].to_vec(),
                // The UTF-8 checkpoints report the first bad byte.
                _ => vec![tail[0]],
            });
        }
    }
    match offending {
        Some(offending) => CharsetOutcome::Converted {
            offending,
            converted,
        },
        None => CharsetOutcome::Fine,
    }
}

/// TiDB renders the unrepresentable bytes as `\xHH` escapes in the 1366
/// message (`Incorrect string value '\xE6\x88\x91' for column 'a'`).
pub(crate) fn escape_charset_bytes(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &b in bytes {
        match b {
            b'\\' => out.push_str("\\\\"),
            b'\'' => out.push_str("\\'"),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\x{b:02X}")),
        }
    }
    out
}

fn invalid_charset_error(offending: &[u8], col: &str) -> Error {
    Error::new(
        ERR_TRUNCATED_WRONG_VALUE_FOR_FIELD,
        format!(
            "Incorrect string value '{}' for column '{}'",
            escape_charset_bytes(offending),
            col
        ),
    )
}

/// Value rendering in `Duplicate entry '…'`: binary strings escape their
/// non-printable bytes as `\xHH` (TiDB prints the byte slice), while other
/// values keep their display form.
pub(crate) fn duplicate_part_text(v: &Datum) -> String {
    match v {
        Datum::Bytes(b) | Datum::Bit(b) => escape_charset_bytes(b),
        other => other.display(),
    }
}

fn truncated_error(_v: &Datum, col: &str) -> Error {
    Error::new(
        ERR_WARN_DATA_TRUNCATED,
        format!("Data truncated for column '{col}' at row 1"),
    )
}

thread_local! {
    /// `UPDATE`'s assignment cast builds TiDB's `ErrTruncated` without format
    /// arguments, so the placeholders reach the client verbatim
    /// (`Data truncated for column '%s' at row %d`); `INSERT` formats them.
    static UPDATE_ASSIGNMENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Evaluating a `GENERATED ALWAYS AS` column converts strings lossily: an
    /// ascii generated column derived from a utf8 source shows `?` instead of
    /// failing the insert (TiDB's generated-value cast path).
    static GENERATED_COLUMN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// `INSERT IGNORE`: conversion errors become warnings and the row keeps a
    /// coerced value, as if sql_mode were non-strict.
    static IGNORE_MODE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Marks the surrounding insert as `INSERT IGNORE` until dropped.
struct IgnoreModeGuard(bool);

impl IgnoreModeGuard {
    fn new(ignore: bool) -> Self {
        Self(IGNORE_MODE.with(|flag| flag.replace(ignore)))
    }
}

impl Drop for IgnoreModeGuard {
    fn drop(&mut self) {
        IGNORE_MODE.with(|flag| flag.set(self.0));
    }
}

/// Marks the surrounding cast as an `UPDATE` assignment; returns the previous
/// state so the caller can restore it.
pub(crate) fn set_update_assignment(on: bool) -> bool {
    UPDATE_ASSIGNMENT.with(|flag| flag.replace(on))
}

fn truncated_error_message(col: &str) -> Error {
    let message = if UPDATE_ASSIGNMENT.with(std::cell::Cell::get) {
        "Data truncated for column '%s' at row %d".to_string()
    } else {
        format!("Data truncated for column '{col}' at row 1")
    };
    Error::new(ERR_WARN_DATA_TRUNCATED, message)
}

fn invalid_enum_set_value(_v: &Datum, col: &str, fallback: Datum) -> Result<Datum> {
    if strict_sql_mode() {
        return Err(truncated_error_message(col));
    }
    sealdb_expression::note_warning(
        ERR_WARN_DATA_TRUNCATED,
        format!("Data truncated for column '{col}' at row 1"),
    );
    Ok(fallback)
}

/// One `INSERT`/`REPLACE` row after column mapping: the values aligned to the
/// table's column offsets, plus an explicit `_tidb_rowid` when the statement
/// named one (`tidb_opt_write_row_id`).
struct PendingRow {
    values: Vec<Datum>,
    rowid: Option<i64>,
    /// AUTO_INCREMENT id allocated for this row, if any. Only rows that end up
    /// inserted publish it through `LAST_INSERT_ID()`.
    generated_id: Option<i64>,
}

/// Parse the statement's `_tidb_rowid` value for one row. `NULL` means "let the
/// allocator pick one" (`1242`-free), the literal `0` does too unless
/// `NO_AUTO_VALUE_ON_ZERO` is set.
fn rowid_override(columns: &[String], row: &[Datum]) -> Result<Option<i64>> {
    let Some(idx) = columns.iter().position(|name| is_tidb_rowid(name)) else {
        return Ok(None);
    };
    let Some(value) = row.get(idx) else {
        return Ok(None);
    };
    match value {
        Datum::Null => Ok(None),
        Datum::I64(v) => Ok(Some(*v)),
        Datum::U64(v) => Ok(Some(*v as i64)),
        Datum::Year(v) => Ok(Some(*v)),
        other => {
            let text = other.display();
            text.trim().parse::<i64>().map(Some).map_err(|_| {
                Error::new(
                    ERR_TRUNCATED_WRONG_VALUE,
                    format!("Incorrect integer value: '{text}' for column '_tidb_rowid' at row 1"),
                )
            })
        }
    }
}

/// Allocate a row id for a heap row. A table with an `AUTO_INCREMENT` column
/// hands both out of the same auto-id chunk (TiDB `table.Allocator`), and both
/// honour `AUTO_ID_CACHE`; `SHARD_ROW_ID_BITS` randomizes the high bits.
fn next_row_handle(domain: &Domain, info: &sealdb_meta::TableInfo) -> Result<i64> {
    let step = info.auto_id_cache.unwrap_or(1);
    if info.shard_row_id_bits > 0 {
        // TiDB `SHARD_ROW_ID_BITS`: heap-table row ids embed a shard prefix that
        // changes every `tidb_shard_allocate_step` rows of a batch.
        domain.next_sharded_handle(info.id, info.shard_row_id_bits)
    } else if info.columns.iter().any(|c| c.auto_increment) {
        domain.next_auto_id_with_cache(info.id, step)
    } else {
        domain.next_handle_with_cache(info.id, step)
    }
}

fn write_row(
    txn: &mut dyn sealdb_kv::Transaction,
    domain: &Domain,
    info: &sealdb_meta::TableInfo,
    values: &[Datum],
    explicit_rowid: Option<i64>,
) -> Result<(i64, i64)> {
    let col_ids: Vec<_> = info.columns.iter().map(|c| c.id).collect();
    // Integer primary-key handle only when `pk_is_handle`; otherwise allocate
    // a unique _tidb_rowid (TiDB heap table). The handle is the PRIMARY KEY
    // column value (not always column 0 — e.g. `wkey, pkey PRIMARY KEY(pkey)`).
    let pk_offset = info
        .indices
        .iter()
        .find(|idx| idx.primary)
        .and_then(|idx| idx.columns.first())
        .map(|c| c.offset as usize);
    let handle = if let Some(explicit) = explicit_rowid {
        // `_tidb_rowid` given by the statement (heap table only). `0` still
        // asks for an allocated id unless `NO_AUTO_VALUE_ON_ZERO` is set.
        if explicit == 0 && !sealdb_expression::sql_mode_flag("NO_AUTO_VALUE_ON_ZERO") {
            next_row_handle(domain, info)?
        } else {
            // Keep the allocator ahead of an explicit id so a later implicit
            // insert cannot reuse it (TiDB `bumpRowID`). A sharded table only
            // tracks the low `64 - bits` bits.
            let bits = info.shard_row_id_bits.min(63) as u32;
            let bump = if bits == 0 {
                explicit
            } else {
                ((explicit as u64) << bits >> bits) as i64
            };
            // An `AUTO_INCREMENT` table shares one allocator between the column
            // and `_tidb_rowid`, so the explicit handle raises that counter.
            if info.columns.iter().any(|c| c.auto_increment) {
                domain.bump_auto_id(info.id, bump)?;
            } else {
                domain.bump_handle(info.id, bump)?;
            }
            explicit
        }
    } else if info.pk_is_handle {
        match pk_offset.and_then(|off| values.get(off)) {
            Some(Datum::I64(v)) => *v,
            Some(Datum::Year(v)) => *v,
            Some(Datum::U64(v)) => *v as i64,
            _ => next_row_handle(domain, info)?,
        }
    } else {
        next_row_handle(domain, info)?
    };
    let physical_id = locate_partition_for_row(info, values)?;
    let key = encode_row_key_with_handle(physical_id, handle);
    if info.indices.iter().any(|idx| idx.primary) && txn.get(key.as_bytes())?.is_some() {
        let value = pk_offset
            .and_then(|off| values.get(off))
            .map(duplicate_part_text)
            .unwrap_or_default();
        return Err(Error::new(
            ERR_DUP_ENTRY,
            format!(
                "Duplicate entry '{value}' for key '{}.PRIMARY'",
                info.name.o()
            ),
        ));
    }
    txn.set(key.as_bytes(), &encode_row(&col_ids, values))?;
    write_index_keys(txn, info, handle, values, physical_id)?;
    Ok((handle, physical_id))
}

/// Error text for one index part. TiDB renders a prefix part from the raw
/// byte slice (`name(2)` over `"测试"` reports the first two bytes), which
/// need not be valid UTF-8; lossy rendering keeps the harness comparable.
fn indexed_part_text(column: &sealdb_meta::IndexColumn, value: &Datum) -> String {
    if column.length >= 0
        && let Datum::Bytes(bytes) = value
    {
        let n = (column.length as usize).min(bytes.len());
        return String::from_utf8_lossy(&bytes[..n]).into_owned();
    }
    duplicate_part_text(value)
}

/// Value an index part compares with. A prefix part (`col(N)`) only
/// keeps its first `N` characters, matching TiDB's `TruncateIndexValue`.
fn indexed_part_value(column: &sealdb_meta::IndexColumn, value: &Datum) -> Datum {
    if column.length < 0 {
        return value.clone();
    }
    let Datum::Bytes(bytes) = value else {
        return value.clone();
    };
    let length = column.length as usize;
    let truncated = match std::str::from_utf8(bytes) {
        Ok(text) => text.chars().take(length).collect::<String>().into_bytes(),
        // Binary strings count bytes, not characters.
        Err(_) => bytes[..bytes.len().min(length)].to_vec(),
    };
    Datum::Bytes(truncated)
}

/// Canonical text of a JSON array of integers: `[2,22]` → `[2, 22]`.
fn canonical_array_text(value: &Datum) -> String {
    let raw = match value {
        Datum::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        Datum::I64(n) => format!("[{n}]"),
        Datum::U64(n) => format!("[{n}]"),
        other => other.display(),
    };
    let text = raw.trim();
    if !text.starts_with('[') || !text.ends_with(']') {
        return text.to_string();
    }
    let items: Vec<String> = text[1..text.len() - 1]
        .split(',')
        .filter_map(|part| {
            let part = part.trim();
            (!part.is_empty()).then(|| part.to_string())
        })
        .collect();
    format!("[{}]", items.join(", "))
}

/// Elements of a multi-valued index key (`[1, 11]` → `1`, `11`).
fn multi_valued_elements(value: &Datum) -> Vec<String> {
    let Datum::Bytes(bytes) = value else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    if !text.starts_with('[') || !text.ends_with(']') {
        return Vec::new();
    }
    text[1..text.len() - 1]
        .split(',')
        .map(|part| part.trim().trim_matches('"').to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

/// True when an index column is the hidden part of a multi-valued index.
fn multi_valued_column(info: &sealdb_meta::TableInfo, col: &sealdb_meta::IndexColumn) -> bool {
    info.columns
        .get(col.offset as usize)
        .and_then(|c| c.generated_expr.as_deref())
        .is_some_and(|expr| expr.to_ascii_uppercase().contains("ARRAY"))
}

/// Index columns equal on every column (NULL never conflicts). Prefix parts
/// only compare their stored prefix, so `'qaa'` and `'qbb'` collide on `a(1)`.
/// Returns the first conflicting unique index, if any.
pub(crate) fn index_conflict(
    info: &sealdb_meta::TableInfo,
    a: &[Datum],
    b: &[Datum],
) -> Option<usize> {
    for (i, index) in info.indices.iter().enumerate() {
        if !index.unique {
            continue;
        }
        let mut all_eq = true;
        for col in &index.columns {
            let av = a
                .get(col.offset as usize)
                .map(|value| indexed_part_value(col, value));
            let bv = b
                .get(col.offset as usize)
                .map(|value| indexed_part_value(col, value));
            if multi_valued_column(info, col) {
                match (av, bv) {
                    (Some(av), Some(bv))
                        if !av.is_null()
                            && !bv.is_null()
                            && multi_valued_elements(&av)
                                .iter()
                                .any(|e| multi_valued_elements(&bv).contains(e)) => {}
                    _ => {
                        all_eq = false;
                        break;
                    }
                }
                continue;
            }
            match (av, bv) {
                (Some(av), Some(bv)) if !av.is_null() && !bv.is_null() && av == bv => {}
                _ => {
                    all_eq = false;
                    break;
                }
            }
        }
        if all_eq && index_applies(info, index, a) && index_applies(info, index, b) {
            return Some(i);
        }
    }
    None
}

fn find_conflict(
    info: &sealdb_meta::TableInfo,
    rows: &[crate::table_reader::ScannedRow],
    skip: Option<usize>,
    values: &[Datum],
) -> Option<(usize, usize)> {
    rows.iter().enumerate().find_map(|(i, row)| {
        if Some(i) == skip {
            return None;
        }
        index_conflict(info, &row.values, values).map(|idx| (i, idx))
    })
}

pub(crate) fn duplicate_error(
    info: &sealdb_meta::TableInfo,
    index_idx: usize,
    values: &[Datum],
) -> Error {
    duplicate_error_from_row(info, index_idx, values, None)
}

/// `INSERT` conflict message: a multi-valued index reports the overlapping
/// element (`Duplicate entry '2'`), while other paths report the whole key.
fn duplicate_error_from_row(
    info: &sealdb_meta::TableInfo,
    index_idx: usize,
    values: &[Datum],
    existing: Option<&[Datum]>,
) -> Error {
    let index = &info.indices[index_idx];
    if let Some(existing) = existing
        && let Some(col) = index.columns.iter().find(|c| multi_valued_column(info, c))
    {
        let new = values
            .get(col.offset as usize)
            .map(multi_valued_elements)
            .unwrap_or_default();
        let old = existing
            .get(col.offset as usize)
            .map(multi_valued_elements)
            .unwrap_or_default();
        if let Some(element) = new.iter().find(|e| old.contains(e)) {
            let name = index.name.o().to_string();
            return Error::new(
                ERR_DUP_ENTRY,
                format!(
                    "Duplicate entry '{element}' for key '{}.{name}'",
                    info.name.o()
                ),
            );
        }
    }
    let value = index
        .columns
        .iter()
        .map(|c| {
            values
                .get(c.offset as usize)
                .map(|value| indexed_part_text(c, value))
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join("-");
    let name = if index.primary {
        "PRIMARY".to_string()
    } else {
        index.name.o().to_string()
    };
    Error::new(
        ERR_DUP_ENTRY,
        format!(
            "Duplicate entry '{value}' for key '{}.{name}'",
            info.name.o()
        ),
    )
}
