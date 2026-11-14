// single-table UPDATE SET (ORDER BY / LIMIT) and two-table UPDATE JOIN.
// pessimistic UPDATE reads latest and takes exclusive locks.
// Completeness wave / not this feature: n-table UPDATE, UPDATE FROM, comma joins,
// RETURNING (not in TiDB v8.5.8 / MySQL 8.0), MySQL-protocol affected-row counts.
use crate::cte::{eval_named_cte, pre_evaluate_dml_predicate};
use crate::index::{delete_index_keys, write_index_keys};
use crate::internal::exec::Executor;
use crate::join::hash_join_v1::hash_join_pairs;
use crate::sortexec::sort::sort_rows_by;
use crate::table_reader::{ScannedRow, scan_table};
use sealdb_domain::Domain;
use sealdb_errno::{
    ERR_BAD_FIELD, ERR_BAD_NULL, ERR_NO_DB, ERR_NO_PARTITION_FOR_GIVEN_VALUE, ERR_NO_SUCH_TABLE,
    ERR_ROW_DOES_NOT_MATCH_GIVEN_PARTITION_SET, ERR_UNKNOWN_PARTITION, ERR_VIEW_CHECK_FAILED,
    Error, Result,
};
use sealdb_expression::{eval_expr_with_row, eval_filter, locate_partition_for_row};
use sealdb_kv::{LockWait, RowLock, Transaction, TxnHandle};
use sealdb_meta::TableInfo;
use sealdb_parser::ast::ddl::TableName;
use sealdb_parser::ast::{Assignment, Join, UpdateStmt, WithClause};
use sealdb_tablecodec::{encode_row, encode_row_key_with_handle};
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;
use std::collections::HashMap;
use std::sync::Arc;

/// `UPDATE` executor: single-table path, or two-table JOIN via `hash_join_pairs`.
pub struct UpdateExec {
    domain: Arc<Domain>,
    stmt: UpdateStmt,
    current_db: String,
    txn: Arc<TxnHandle>,
    done: bool,
}

impl UpdateExec {
    pub fn new(
        domain: Arc<Domain>,
        stmt: UpdateStmt,
        current_db: impl Into<String>,
        txn: Arc<TxnHandle>,
    ) -> Self {
        Self {
            domain,
            stmt,
            current_db: current_db.into(),
            txn,
            done: false,
        }
    }
}

impl Executor for UpdateExec {
    fn open(&mut self) -> Result<()> {
        if let Some(join) = self.stmt.join.clone() {
            return self.update_join(&join);
        }
        let info = load_table(&self.domain, &self.current_db, &self.stmt.table)?;
        let col_names = col_names(&info);
        let lock = dml_row_lock(&self.txn)?;
        let filter = match &self.stmt.selection {
            Some(expr) => Some(pre_evaluate_dml_predicate(
                expr,
                &self.domain,
                &self.current_db,
                &self.txn,
            )?),
            None => None,
        };
        // Prune on the pre-evaluated predicate: a raw scalar subquery cannot be
        // reasoned about and would prune every partition away.
        let mut physical = sealdb_planner::core::prune_partitions(&info, filter.as_ref());
        let targets = resolve_partition_ids(&info, &self.stmt.table.partition_names)?;
        if !targets.is_empty() {
            physical.retain(|id| targets.contains(id));
        }
        let snapshot = scan_table(&self.txn, &info, lock, LockWait::Block, &physical)?;
        enforce_truncating_comparisons(&info, filter.as_ref(), self.stmt.ignore)?;
        let mut matched = Vec::new();
        for scanned in &snapshot {
            if let Some(filter) = &filter
                && !eval_filter(filter, &col_names, &scanned.values)?
            {
                continue;
            }
            matched.push(scanned.clone());
        }
        sort_rows_by(&mut matched, &self.stmt.order_by, &col_names, |row| {
            &row.values
        });
        if let Some(limit) = self.stmt.limit {
            matched.truncate(limit as usize);
        }
        let matched_count = matched.len() as u64;
        // Compute every row's new values first, so uniqueness / partition
        // errors abort the statement before any write (TiDB statement atomicity).
        let mut plans: Vec<(crate::table_reader::ScannedRow, Vec<Datum>)> =
            Vec::with_capacity(matched.len());
        for scanned in matched {
            let mut new_values = scanned.values.clone();
            // `SET c = (SELECT … WHERE inner.k = outer.k)`: bind the outer
            // row's columns so the subquery becomes uncorrelated and can run.
            let list = bind_outer_assignments(
                &self.stmt.list,
                &info,
                &col_names,
                &scanned.values,
                &self.domain,
                &self.current_db,
                &self.txn,
            )?;
            apply_assignments(
                &list,
                &col_names,
                &scanned.values,
                col_names.len(),
                Some(&mut new_values),
                None,
            )?;
            crate::insert::fill_generated_columns(&info, &mut new_values)?;
            reject_null_not_null(&info, &new_values)?;
            crate::insert::bump_auto_increment(&self.domain, &info, &new_values)?;
            if let Some(check) = &self.stmt.check_option_where
                && !eval_filter(check, &col_names, &new_values)?
            {
                let view = self
                    .stmt
                    .check_option_view
                    .clone()
                    .unwrap_or_else(|| self.stmt.table.name.clone());
                return Err(Error::new(
                    ERR_VIEW_CHECK_FAILED,
                    format!("CHECK OPTION failed '{view}'"),
                ));
            }
            plans.push((scanned, new_values));
        }
        // Duplicate keys surface before "no partition for value" just as TiDB's
        // row-by-row apply does for the rows it has already written.
        let validation_snapshot = if info.indices.iter().any(|idx| idx.unique)
            && physical.len() < info.physical_ids().len()
        {
            scan_table(
                &self.txn,
                &info,
                lock,
                LockWait::Block,
                &info.physical_ids(),
            )?
        } else {
            snapshot.clone()
        };
        let mut keep =
            validate_update_uniqueness(&info, &validation_snapshot, &plans, self.stmt.ignore)?;
        for (i, (_, new_values)) in plans.iter().enumerate() {
            if keep[i] {
                keep[i] = check_partition_target(&info, new_values, &targets, self.stmt.ignore)?;
            }
        }
        let mut changed = 0u64;
        self.txn.with_txn(|txn| {
            for ((scanned, new_values), keep) in plans.into_iter().zip(keep) {
                if keep && new_values != scanned.values {
                    apply_row_update(txn, &info, &scanned, new_values)?;
                    changed += 1;
                }
            }
            Ok(())
        })?;
        crate::internal::note_dml_affected(changed);
        crate::internal::set_dml_info(format!(
            "Rows matched: {matched_count}  Changed: {changed}  Warnings: {}",
            sealdb_expression::warnings_len()
        ));
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

impl UpdateExec {
    /// `UPDATE t1 JOIN t2 ON ... SET ...`: join, assign, coalesce by handle, write.
    fn update_join(&mut self, join: &Join) -> Result<()> {
        let lock = dml_row_lock(&self.txn)?;
        let (left_names, left_rows, left_info) = scan_dml_side(
            &self.domain,
            &self.current_db,
            &self.txn,
            self.stmt.with.as_ref(),
            &join.left,
            lock,
        )?;
        let (right_names, right_rows, right_info) = scan_dml_side(
            &self.domain,
            &self.current_db,
            &self.txn,
            self.stmt.with.as_ref(),
            &join.right,
            lock,
        )?;
        let schema = joined_schema(&join.left.name, &left_names, &join.right.name, &right_names);
        let left_values: Vec<Vec<Datum>> = left_rows.iter().map(|r| r.values.clone()).collect();
        let right_values: Vec<Vec<Datum>> = right_rows.iter().map(|r| r.values.clone()).collect();
        let pairs = hash_join_pairs(join, &left_names, &right_names, &left_values, &right_values)?;
        let left_width = left_names.len();
        let mut left_updates: HashMap<(i64, i64), (ScannedRow, Vec<Datum>)> = HashMap::new();
        let mut right_updates: HashMap<(i64, i64), (ScannedRow, Vec<Datum>)> = HashMap::new();
        for (left_i, right_i) in pairs {
            let left_row = left_i.map(|i| &left_rows[i]);
            let right_row = right_i.map(|i| &right_rows[i]);
            let joined = concat_optional(left_row, right_row, left_width, right_names.len());
            if let Some(filter) = &self.stmt.selection
                && !eval_filter(filter, &schema, &joined)?
            {
                continue;
            }
            let mut left_new = left_row.map(|r| r.values.clone());
            let mut right_new = right_row.map(|r| r.values.clone());
            apply_assignments(
                &self.stmt.list,
                &schema,
                &joined,
                left_width,
                left_new.as_deref_mut(),
                right_new.as_deref_mut(),
            )?;
            if let (Some(left), Some(values)) = (&left_info, left_new.as_mut()) {
                crate::insert::fill_generated_columns(left, values)?;
            }
            if let (Some(right), Some(values)) = (&right_info, right_new.as_mut()) {
                crate::insert::fill_generated_columns(right, values)?;
            }
            if left_info.is_some()
                && let (Some(scanned), Some(values)) = (left_row, left_new)
            {
                left_updates.insert(
                    (scanned.physical_id, scanned.handle),
                    (scanned.clone(), values),
                );
            }
            if right_info.is_some()
                && let (Some(scanned), Some(values)) = (right_row, right_new)
            {
                right_updates.insert(
                    (scanned.physical_id, scanned.handle),
                    (scanned.clone(), values),
                );
            }
        }
        // Self-join (`UPDATE t t1, t t2 ...`): both aliases address the same
        // physical rows, so only the left role's rewritten values are applied.
        let same_table = matches!(
            (&left_info, &right_info),
            (Some(left), Some(right)) if left.id == right.id
        );
        self.txn.with_txn(|txn| {
            if let Some(left) = &left_info {
                for (_, (scanned, values)) in left_updates {
                    apply_row_update(txn, left, &scanned, values)?;
                }
            }
            if let Some(right) = &right_info.filter(|_| !same_table) {
                for (_, (scanned, values)) in right_updates {
                    apply_row_update(txn, right, &scanned, values)?;
                }
            }
            Ok(())
        })
    }
}

/// Scan a base table or materialize a CTE for UPDATE/DELETE JOIN.
pub(crate) fn scan_dml_side(
    domain: &Arc<Domain>,
    current_db: &str,
    txn: &Arc<TxnHandle>,
    with: Option<&WithClause>,
    table: &TableName,
    lock: RowLock,
) -> Result<(Vec<String>, Vec<ScannedRow>, Option<TableInfo>)> {
    if let Some(with) = with
        && with.find(&table.name).is_some()
    {
        let out = eval_named_cte(&table.name, with, domain, current_db, txn)?;
        let rows = out
            .rows
            .into_iter()
            .enumerate()
            .map(|(i, values)| ScannedRow {
                handle: i as i64,
                physical_id: 0,
                values,
            })
            .collect();
        return Ok((out.columns, rows, None));
    }
    let info = load_table(domain, current_db, table)?;
    let names = col_names(&info);
    let rows = scan_table(
        txn,
        &info,
        lock,
        LockWait::Block,
        &sealdb_planner::core::prune_partitions(&info, None),
    )?;
    Ok((names, rows, Some(info)))
}

/// Resolve `schema.table` against the session infoschema.
pub(crate) fn load_table(domain: &Domain, current_db: &str, name: &TableName) -> Result<TableInfo> {
    let (schema, table) = name.qualified(current_db);
    if schema.is_empty() {
        return Err(Error::new(ERR_NO_DB, "No database selected"));
    }
    domain
        .infoschema()
        .table_by_name(&schema, &table)
        .cloned()
        .ok_or_else(|| {
            Error::new(
                ERR_NO_SUCH_TABLE,
                format!("Table '{schema}.{table}' doesn't exist"),
            )
        })
}

/// Unqualified column names in table order (join schema is qualified separately).
pub(crate) fn col_names(info: &TableInfo) -> Vec<String> {
    info.columns
        .iter()
        .map(|c| c.name.o().to_string())
        .collect()
}

pub(crate) fn dml_row_lock(txn: &TxnHandle) -> Result<RowLock> {
    Ok(if txn.with_txn(|txn| Ok(txn.is_pessimistic()))? {
        RowLock::Exclusive
    } else {
        RowLock::None
    })
}

/// Case-insensitive schema+name equality after applying `current_db`.
pub(crate) fn table_matches(name: &TableName, wanted: &TableName, current_db: &str) -> bool {
    let (s1, t1) = name.qualified(current_db);
    let (s2, t2) = wanted.qualified(current_db);
    s1.eq_ignore_ascii_case(&s2) && t1.eq_ignore_ascii_case(&t2)
}

/// Concatenate `t.col` names so join filters and SET exprs can resolve both sides.
pub(crate) fn joined_schema(
    left_table: &str,
    left_names: &[String],
    right_table: &str,
    right_names: &[String],
) -> Vec<String> {
    left_names
        .iter()
        .map(|c| format!("{left_table}.{c}"))
        .chain(right_names.iter().map(|c| format!("{right_table}.{c}")))
        .collect()
}

/// Concatenate optional join sides; unmatched outer rows are padded with NULL.
fn concat_optional(
    left: Option<&ScannedRow>,
    right: Option<&ScannedRow>,
    left_width: usize,
    right_width: usize,
) -> Vec<Datum> {
    let mut row = match left {
        Some(scanned) => scanned.values.clone(),
        None => vec![Datum::Null; left_width],
    };
    match right {
        Some(scanned) => row.extend_from_slice(&scanned.values),
        None => row.extend(std::iter::repeat_n(Datum::Null, right_width)),
    }
    row
}

/// Evaluate SET exprs against the original joined row, then write into left/right copies.
pub(crate) fn apply_assignments(
    list: &[Assignment],
    schema: &[String],
    original: &[Datum],
    left_width: usize,
    mut left_values: Option<&mut [Datum]>,
    mut right_values: Option<&mut [Datum]>,
) -> Result<()> {
    for item in list {
        let idx = find_schema_index(schema, item.table.as_deref(), &item.column)?;
        let value = eval_expr_with_row(&item.expr, schema, original)?;
        if idx < left_width {
            if let Some(left) = left_values.as_mut()
                && let Some(slot) = left.get_mut(idx)
            {
                *slot = value;
            }
        } else if let Some(right) = right_values.as_mut()
            && let Some(slot) = right.get_mut(idx - left_width)
        {
            *slot = value;
        }
    }
    Ok(())
}

/// Locate `table.column` (or unique unqualified `column`) in a join/table schema.
fn find_schema_index(schema: &[String], table: Option<&str>, column: &str) -> Result<usize> {
    let mut found = None;
    for (i, name) in schema.iter().enumerate() {
        if !column_matches(name, table, column) {
            continue;
        }
        if found.is_some() && table.is_none() {
            return Err(Error::new(
                ERR_BAD_FIELD,
                format!("Column '{column}' in 'field list' is ambiguous"),
            ));
        }
        found = Some(i);
    }
    found.ok_or_else(|| {
        Error::new(
            ERR_BAD_FIELD,
            format!("Unknown column '{column}' in 'field list'"),
        )
    })
}

fn column_matches(schema_name: &str, table: Option<&str>, column: &str) -> bool {
    let (qual, name) = match schema_name.rsplit_once('.') {
        Some((t, c)) => (Some(t), c),
        None => (None, schema_name),
    };
    let name_ok = name.eq_ignore_ascii_case(column) || schema_name.eq_ignore_ascii_case(column);
    match table {
        Some(wanted) => {
            name_ok
                && (qual.is_none_or(|t| t.eq_ignore_ascii_case(wanted))
                    || schema_name.eq_ignore_ascii_case(&format!("{wanted}.{column}")))
        }
        None => name_ok,
    }
}

/// `NOT NULL` columns reject an explicit NULL assignment (`1048`).
fn reject_null_not_null(info: &sealdb_meta::TableInfo, values: &[Datum]) -> Result<()> {
    for (col, value) in info.columns.iter().zip(values.iter()) {
        if value.is_null() && col.field_type.flag & sealdb_types::field_type::NOT_NULL_FLAG != 0 {
            return Err(Error::new(
                ERR_BAD_NULL,
                format!("Column '{}' cannot be null", col.name.o()),
            ));
        }
    }
    Ok(())
}

/// Resolve `tbl PARTITION (p1, p2)` names to physical ids (empty = all).
pub(crate) fn resolve_partition_ids(info: &TableInfo, names: &[String]) -> Result<Vec<i64>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let Some(part) = info.partition.as_ref() else {
        return Err(Error::new(
            ERR_UNKNOWN_PARTITION,
            format!(
                "Unknown partition '{}' in table '{}'",
                names[0],
                info.name.o()
            ),
        ));
    };
    let mut ids = Vec::with_capacity(names.len());
    for name in names {
        let found = part
            .definitions
            .iter()
            .find(|def| def.name.o().eq_ignore_ascii_case(name))
            .map(|def| def.id);
        let Some(id) = found else {
            return Err(Error::new(
                ERR_UNKNOWN_PARTITION,
                format!("Unknown partition '{name}' in table '{}'", info.name.o()),
            ));
        };
        ids.push(id);
    }
    Ok(ids)
}

/// Enforce `PARTITION (…)` targeting and partition-routing failures for the
/// post-update row: `UPDATE IGNORE` warns and skips (`1526` / `1748`).
fn check_partition_target(
    info: &TableInfo,
    new_values: &[Datum],
    targets: &[i64],
    ignore: bool,
) -> Result<bool> {
    let located = locate_partition_for_row(info, new_values);
    let target = match located {
        Ok(id) => id,
        Err(e) if ignore && e.code == ERR_NO_PARTITION_FOR_GIVEN_VALUE => {
            sealdb_expression::note_warning(e.code, e.message);
            return Ok(false);
        }
        Err(e) => return Err(e),
    };
    if targets.is_empty() || targets.contains(&target) {
        return Ok(true);
    }
    if ignore {
        sealdb_expression::note_warning(
            ERR_ROW_DOES_NOT_MATCH_GIVEN_PARTITION_SET,
            "Found a row not matching the given partition set".to_string(),
        );
        return Ok(false);
    }
    Err(Error::new(
        ERR_ROW_DOES_NOT_MATCH_GIVEN_PARTITION_SET,
        "Found a row not matching the given partition set",
    ))
}

/// Reject an UPDATE that would collide with a unique index: compare every
/// planned new row against the other rows (final state, so intra-statement
/// swaps are allowed only when no two rows end up equal).
fn validate_update_uniqueness(
    info: &TableInfo,
    snapshot: &[ScannedRow],
    plans: &[(ScannedRow, Vec<Datum>)],
    ignore: bool,
) -> Result<Vec<bool>> {
    // `keep[i]` is false for rows skipped by `UPDATE IGNORE`.
    let mut keep = vec![true; plans.len()];
    for i in 0..plans.len() {
        if !keep[i] {
            continue;
        }
        let new_values = &plans[i].1;
        for j in (i + 1)..plans.len() {
            if !keep[j] {
                continue;
            }
            if let Some(idx) = crate::insert::index_conflict(info, new_values, &plans[j].1) {
                if ignore {
                    sealdb_expression::note_warning(
                        crate::insert::duplicate_error(info, idx, &plans[j].1).code,
                        crate::insert::duplicate_error(info, idx, &plans[j].1).message,
                    );
                    keep[j] = false;
                } else {
                    return Err(crate::insert::duplicate_error(info, idx, new_values));
                }
            }
        }
        for existing in snapshot {
            let same_row = existing.handle == plans[i].0.handle
                && existing.physical_id == plans[i].0.physical_id;
            let also_updated = plans
                .iter()
                .any(|(p, _)| p.handle == existing.handle && p.physical_id == existing.physical_id);
            if same_row || also_updated {
                continue;
            }
            if let Some(idx) = crate::insert::index_conflict(info, &existing.values, new_values) {
                if ignore {
                    let err = crate::insert::duplicate_error(info, idx, new_values);
                    sealdb_expression::note_warning(err.code, err.message);
                    keep[i] = false;
                    break;
                }
                return Err(crate::insert::duplicate_error(info, idx, new_values));
            }
        }
    }
    Ok(keep)
}

/// TiDB rejects a numeric predicate constant with trailing garbage (`'2a'`) in
/// strict DML (`1292`); under `IGNORE` it warns (once per conversion).
pub(crate) fn enforce_truncating_comparisons(
    info: &TableInfo,
    filter: Option<&sealdb_parser::ast::ExprNode>,
    ignore: bool,
) -> Result<()> {
    let Some(filter) = filter else {
        return Ok(());
    };
    let mut messages = Vec::new();
    collect_truncating(info, filter, &mut messages);
    for message in messages {
        if ignore {
            sealdb_expression::note_warning(
                sealdb_errno::ERR_TRUNCATED_WRONG_VALUE,
                message.clone(),
            );
            sealdb_expression::note_warning(sealdb_errno::ERR_TRUNCATED_WRONG_VALUE, message);
        } else {
            return Err(Error::new(sealdb_errno::ERR_TRUNCATED_WRONG_VALUE, message));
        }
    }
    Ok(())
}

fn collect_truncating(
    info: &TableInfo,
    expr: &sealdb_parser::ast::ExprNode,
    out: &mut Vec<String>,
) {
    use sealdb_parser::ast::ExprNode;
    use sealdb_parser::opcode::Op;
    match expr {
        ExprNode::Binary(bin) => {
            if matches!(
                bin.op,
                Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge | Op::NullSafeEq
            ) {
                if let Some(message) = truncating_pair(info, &bin.left, &bin.right) {
                    out.push(message);
                }
            }
            collect_truncating(info, &bin.left, out);
            collect_truncating(info, &bin.right, out);
        }
        ExprNode::Unary(unary) => collect_truncating(info, &unary.expr, out),
        ExprNode::Func(func) => {
            for arg in &func.args {
                collect_truncating(info, arg, out);
            }
        }
        ExprNode::Cast(cast) => collect_truncating(info, &cast.expr, out),
        ExprNode::InList(list) => {
            for item in &list.list {
                collect_truncating(info, item, out);
            }
        }
        _ => {}
    }
}

fn truncating_pair(
    info: &TableInfo,
    left: &sealdb_parser::ast::ExprNode,
    right: &sealdb_parser::ast::ExprNode,
) -> Option<String> {
    use sealdb_parser::ast::ExprNode;
    let string_of = |expr: &ExprNode| match expr {
        ExprNode::Value(v) => match &v.datum {
            Datum::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
            _ => None,
        },
        _ => None,
    };
    let is_numeric_column = |expr: &ExprNode| match expr {
        ExprNode::Column(c) => info
            .columns
            .iter()
            .find(|col| col.name.o().eq_ignore_ascii_case(&c.name))
            .is_some_and(|col| {
                matches!(
                    col.field_type.eval_type(),
                    sealdb_types::field_type::EvalType::Int
                        | sealdb_types::field_type::EvalType::Real
                )
            }),
        _ => false,
    };
    let (string, column_side) = if is_numeric_column(left) {
        (string_of(right)?, true)
    } else if is_numeric_column(right) {
        (string_of(left)?, true)
    } else {
        return None;
    };
    if !column_side || !has_truncating_numeric_prefix(&string) {
        return None;
    }
    Some(format!("Truncated incorrect DOUBLE value: '{string}'"))
}

/// True when the text starts with a number but continues with non-numeric junk.
fn has_truncating_numeric_prefix(text: &str) -> bool {
    let trimmed = text.trim();
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        i += 1;
    }
    let digits_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == digits_start {
        return false;
    }
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    i < bytes.len()
}

/// Rewrite one row: drop old index keys, delete the old record if the handle or
/// partition moved, then put the new row and unique/secondary indexes.
pub(crate) fn apply_row_update(
    txn: &mut dyn Transaction,
    info: &TableInfo,
    scanned: &ScannedRow,
    new_values: Vec<Datum>,
) -> Result<()> {
    let col_ids: Vec<i64> = info.columns.iter().map(|c| c.id).collect();
    // Only an integer clustered PRIMARY KEY changes the row handle; heap tables
    // keep their allocated `_tidb_rowid` across updates.
    let new_handle = if info.pk_is_handle {
        info.indices
            .iter()
            .find(|idx| idx.primary)
            .and_then(|idx| idx.columns.first())
            .and_then(|c| new_values.get(c.offset as usize))
            .and_then(|d| match d {
                Datum::I64(v) => Some(*v),
                Datum::Year(v) => Some(*v),
                Datum::U64(v) => Some(*v as i64),
                _ => None,
            })
            .unwrap_or(scanned.handle)
    } else {
        scanned.handle
    };
    let new_physical = locate_partition_for_row(info, &new_values)?;
    delete_index_keys(
        txn,
        info,
        scanned.handle,
        &scanned.values,
        scanned.physical_id,
    )?;
    if new_handle != scanned.handle || new_physical != scanned.physical_id {
        txn.delete(encode_row_key_with_handle(scanned.physical_id, scanned.handle).as_bytes())?;
    }
    txn.set(
        encode_row_key_with_handle(new_physical, new_handle).as_bytes(),
        &encode_row(&col_ids, &new_values),
    )?;
    write_index_keys(txn, info, new_handle, &new_values, new_physical)
}

/// Bind the UPDATE row's columns inside each assignment's subqueries so the
/// subquery can be evaluated without an Apply (`SET name = (SELECT … WHERE
/// inner.id = outer.fk)`).
fn bind_outer_assignments(
    list: &[Assignment],
    info: &TableInfo,
    col_names: &[String],
    values: &[Datum],
    domain: &Arc<Domain>,
    current_db: &str,
    txn: &Arc<TxnHandle>,
) -> Result<Vec<Assignment>> {
    let mut out = Vec::with_capacity(list.len());
    for item in list {
        if !item.expr.has_subquery() {
            out.push(item.clone());
            continue;
        }
        let bound = bind_outer_refs(&item.expr, &info.name.o(), col_names, values, &[]);
        let expr = pre_evaluate_dml_predicate(&bound, domain, current_db, txn)?;
        out.push(Assignment {
            table: item.table.clone(),
            column: item.column.clone(),
            expr,
        });
    }
    Ok(out)
}

/// Replace references to the UPDATE target's columns with their current
/// values. Inside a subquery only table-qualified references to the target
/// table are rewritten, so inner columns of the same name stay untouched.
fn bind_outer_refs(
    expr: &sealdb_parser::ast::ExprNode,
    outer_table: &str,
    col_names: &[String],
    values: &[Datum],
    inner_tables: &[String],
) -> sealdb_parser::ast::ExprNode {
    use sealdb_parser::ast::ExprNode;
    use sealdb_parser::ast::expressions::ValueExpr;

    let inside = !inner_tables.is_empty();
    match expr {
        ExprNode::Column(c) => {
            let qualified_outer = c
                .table
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case(outer_table));
            let bare_outer = c.table.is_none() && !inside;
            let shadowed = c
                .table
                .as_deref()
                .is_some_and(|t| inner_tables.iter().any(|i| i.eq_ignore_ascii_case(t)));
            if !shadowed && (qualified_outer || bare_outer) {
                let name = c.name.as_str();
                if let Some(i) = col_names.iter().position(|n| {
                    n.rsplit('.')
                        .next()
                        .is_some_and(|b| b.eq_ignore_ascii_case(name))
                }) {
                    return ExprNode::Value(ValueExpr {
                        datum: values[i].clone(),
                    });
                }
            }
            expr.clone()
        }
        ExprNode::Binary(b) => {
            let mut b = b.clone();
            b.left = Box::new(bind_outer_refs(
                &b.left,
                outer_table,
                col_names,
                values,
                inner_tables,
            ));
            b.right = Box::new(bind_outer_refs(
                &b.right,
                outer_table,
                col_names,
                values,
                inner_tables,
            ));
            ExprNode::Binary(b)
        }
        ExprNode::Unary(u) => {
            let mut u = u.clone();
            u.expr = Box::new(bind_outer_refs(
                &u.expr,
                outer_table,
                col_names,
                values,
                inner_tables,
            ));
            ExprNode::Unary(u)
        }
        ExprNode::Func(f) => {
            let mut f = f.clone();
            f.args = f
                .args
                .iter()
                .map(|a| bind_outer_refs(a, outer_table, col_names, values, inner_tables))
                .collect();
            ExprNode::Func(f)
        }
        ExprNode::Cast(c) => {
            let mut c = c.clone();
            c.expr = Box::new(bind_outer_refs(
                &c.expr,
                outer_table,
                col_names,
                values,
                inner_tables,
            ));
            ExprNode::Cast(c)
        }
        ExprNode::InList(l) => {
            let mut l = l.clone();
            l.expr = Box::new(bind_outer_refs(
                &l.expr,
                outer_table,
                col_names,
                values,
                inner_tables,
            ));
            l.list = l
                .list
                .iter()
                .map(|e| bind_outer_refs(e, outer_table, col_names, values, inner_tables))
                .collect();
            ExprNode::InList(l)
        }
        ExprNode::Between(b) => {
            let mut b = b.clone();
            b.expr = Box::new(bind_outer_refs(
                &b.expr,
                outer_table,
                col_names,
                values,
                inner_tables,
            ));
            b.low = Box::new(bind_outer_refs(
                &b.low,
                outer_table,
                col_names,
                values,
                inner_tables,
            ));
            b.high = Box::new(bind_outer_refs(
                &b.high,
                outer_table,
                col_names,
                values,
                inner_tables,
            ));
            ExprNode::Between(b)
        }
        ExprNode::Like(l) => {
            let mut l = l.clone();
            l.expr = Box::new(bind_outer_refs(
                &l.expr,
                outer_table,
                col_names,
                values,
                inner_tables,
            ));
            l.pattern = Box::new(bind_outer_refs(
                &l.pattern,
                outer_table,
                col_names,
                values,
                inner_tables,
            ));
            ExprNode::Like(l)
        }
        ExprNode::IsNull(i) => {
            let mut i = i.clone();
            i.expr = Box::new(bind_outer_refs(
                &i.expr,
                outer_table,
                col_names,
                values,
                inner_tables,
            ));
            ExprNode::IsNull(i)
        }
        ExprNode::Case(c) => {
            let mut c = c.clone();
            if let Some(operand) = &c.operand {
                c.operand = Some(Box::new(bind_outer_refs(
                    operand,
                    outer_table,
                    col_names,
                    values,
                    inner_tables,
                )));
            }
            c.when_then = c
                .when_then
                .iter()
                .map(|(when, then)| {
                    (
                        bind_outer_refs(when, outer_table, col_names, values, inner_tables),
                        bind_outer_refs(then, outer_table, col_names, values, inner_tables),
                    )
                })
                .collect();
            if let Some(else_expr) = &c.else_expr {
                c.else_expr = Some(Box::new(bind_outer_refs(
                    else_expr,
                    outer_table,
                    col_names,
                    values,
                    inner_tables,
                )));
            }
            ExprNode::Case(c)
        }
        ExprNode::ScalarSubquery(s) => {
            let mut s = s.clone();
            if let sealdb_parser::ast::ResultSetNode::Select(query) = s.query.as_ref() {
                s.query = Box::new(sealdb_parser::ast::ResultSetNode::Select(bind_outer_query(
                    query,
                    outer_table,
                    col_names,
                    values,
                    inner_tables,
                )));
            }
            ExprNode::ScalarSubquery(s)
        }
        _ => expr.clone(),
    }
}

fn bind_outer_query(
    query: &sealdb_parser::ast::SelectStmt,
    outer_table: &str,
    col_names: &[String],
    values: &[Datum],
    outer_inner: &[String],
) -> sealdb_parser::ast::SelectStmt {
    let mut inner: Vec<String> = outer_inner.to_vec();
    inner.extend(
        query
            .join_tables()
            .into_iter()
            .map(|t| t.resolve_name().to_string()),
    );
    let mut q = query.clone();
    for field in &mut q.fields {
        field.expr = bind_outer_refs(&field.expr, outer_table, col_names, values, &inner);
    }
    if let Some(selection) = &q.selection {
        q.selection = Some(bind_outer_refs(
            selection,
            outer_table,
            col_names,
            values,
            &inner,
        ));
    }
    if let Some(having) = &q.having {
        q.having = Some(bind_outer_refs(
            having,
            outer_table,
            col_names,
            values,
            &inner,
        ));
    }
    q.group_by = q
        .group_by
        .iter()
        .map(|e| bind_outer_refs(e, outer_table, col_names, values, &inner))
        .collect();
    for item in &mut q.order_by {
        item.expr = bind_outer_refs(&item.expr, outer_table, col_names, values, &inner);
    }
    for join in &mut q.joins {
        join.on = bind_outer_refs(&join.on, outer_table, col_names, values, &inner);
    }
    q
}
