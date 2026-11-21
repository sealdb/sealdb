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
    ERR_BAD_FIELD, ERR_BAD_NULL, ERR_MULTI_UPDATE_KEY_CONFLICT, ERR_NO_DB,
    ERR_NO_PARTITION_FOR_GIVEN_VALUE, ERR_NO_REFERENCED_ROW_2, ERR_NO_SUCH_TABLE,
    ERR_NON_DEFAULT_VALUE_FOR_GENERATED_COLUMN, ERR_NON_UPDATABLE_TABLE,
    ERR_ROW_DOES_NOT_MATCH_GIVEN_PARTITION_SET, ERR_UNKNOWN_PARTITION, ERR_VIEW_CHECK_FAILED,
    Error, Result,
};
use sealdb_expression::{eval_expr_with_row, eval_filter, locate_partition_for_row};
use sealdb_kv::{LockWait, RowLock, Transaction, TxnHandle};
use sealdb_meta::{ColumnInfo, TableInfo};
use sealdb_parser::ast::ddl::TableName;
use sealdb_parser::ast::expressions::ValueExpr;
use sealdb_parser::ast::{Assignment, ExprNode, Join, JoinType, UpdateStmt, WithClause};
use sealdb_tablecodec::{encode_row, encode_row_key_with_handle};
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;
use std::collections::{HashMap, HashSet};
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
        let prev = sealdb_expression::set_current_dml_table(Some(self.stmt.table.name.clone()));
        let prev_assign = crate::insert::set_update_assignment(true);
        let out = self.open_stmt();
        crate::insert::set_update_assignment(prev_assign);
        sealdb_expression::set_current_dml_table(prev);
        out
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
    fn open_stmt(&mut self) -> Result<()> {
        if let Some(join) = self.stmt.join.clone() {
            if self.stmt.additional_joins.is_empty() {
                return self.update_join(&join);
            }
            return self.update_join_chain(&join);
        }
        if self
            .stmt
            .derived_tables
            .iter()
            .any(|name| name.eq_ignore_ascii_case(&self.stmt.table.name))
        {
            return Err(Error::new(
                ERR_NON_UPDATABLE_TABLE,
                format!(
                    "The target table {} of the UPDATE is not updatable",
                    self.stmt.table.name
                ),
            ));
        }
        let info = load_table(&self.domain, &self.current_db, &self.stmt.table)?;
        // `SET _tidb_rowid = …`: TiDB only accepts an explicit row handle on a
        // heap table with `tidb_opt_write_row_id`, otherwise 1105 (and a
        // single-integer PK table has no such column, 1054).
        if self
            .stmt
            .list
            .iter()
            .any(|assign| assign.column.eq_ignore_ascii_case("_tidb_rowid"))
        {
            crate::insert::check_write_row_id(&info)?;
            return Err(Error::new(
                sealdb_errno::ERR_UNKNOWN_ERROR,
                "insert, update and replace statements for _tidb_rowid are not supported",
            ));
        }
        let col_names = col_names(&info);
        let assignments =
            resolve_default_assignments(&self.stmt.list, &[(info.name.o(), &info, 0)], &col_names)?;
        // A heap / common-handle table exposes `_tidb_rowid` to predicates; the
        // execute-time filter sees the scanned row's handle as that column.
        let rowid_eval = !info.pk_is_handle;
        let mut eval_names = col_names.clone();
        if rowid_eval {
            eval_names.push("_tidb_rowid".to_string());
        }
        let eval_row = |scanned: &crate::table_reader::ScannedRow| {
            let mut values = scanned.values.clone();
            if rowid_eval {
                values.push(Datum::I64(scanned.handle));
            }
            values
        };
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
                && !eval_filter(filter, &eval_names, &eval_row(scanned))?
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
                &assignments,
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
            cast_assigned_values(&assignments, &info, &col_names, &mut new_values)?;
            crate::insert::fill_generated_columns(&info, &mut new_values)?;
            reject_null_not_null(&info, &mut new_values, self.stmt.ignore)?;
            crate::insert::bump_auto_increment(&self.domain, &info, &new_values)?;
            let assigned = assigned_columns(&assignments);
            let child = fk_child_violation(
                &self.domain,
                &self.txn,
                &self.current_db,
                &info,
                &new_values,
                &assigned,
            )?;
            if apply_fk_outcome(child, self.stmt.ignore)? {
                continue;
            }
            let parent = fk_parent_violation(
                &self.domain,
                &self.txn,
                &info,
                &scanned.values,
                &new_values,
                &assigned,
            )?;
            if apply_fk_outcome(parent, self.stmt.ignore)? {
                continue;
            }
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
        let writes: Vec<(ScannedRow, Vec<Datum>)> = plans
            .into_iter()
            .zip(keep)
            .filter(|((scanned, new_values), keep)| *keep && *new_values != scanned.values)
            .map(|((scanned, new_values), _)| (scanned, new_values))
            .collect();
        let changed = writes.len() as u64;
        self.txn
            .with_txn(|txn| apply_row_updates(txn, &info, &writes))?;
        crate::internal::note_dml_affected(changed);
        crate::internal::set_dml_info(format!(
            "Rows matched: {matched_count}  Changed: {changed}  Warnings: {}",
            sealdb_expression::warnings_len()
        ));
        Ok(())
    }
}

/// Merge one alias's rewritten row into the per-handle accumulator; overrides
/// are the columns this alias changed versus the scanned base row.
fn record_join_row(
    merged: &mut HashMap<(i64, i64, i64), (ScannedRow, Vec<Datum>, Vec<(usize, Datum)>)>,
    info: &TableInfo,
    scanned: &ScannedRow,
    values: &[Datum],
) {
    let key = (info.id, scanned.physical_id, scanned.handle);
    let overrides: Vec<(usize, Datum)> = values
        .iter()
        .enumerate()
        .filter(|(i, v)| scanned.values.get(*i) != Some(*v))
        .map(|(i, v)| (i, v.clone()))
        .collect();
    let entry = merged
        .entry(key)
        .or_insert_with(|| (scanned.clone(), scanned.values.clone(), Vec::new()));
    entry.2.extend(overrides);
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
        let mut tables: Vec<(&str, &TableInfo, usize)> = Vec::new();
        if let Some(info) = left_info.as_ref() {
            tables.push((join.left.name.as_str(), info, 0));
        }
        if let Some(info) = right_info.as_ref() {
            tables.push((join.right.name.as_str(), info, left_names.len()));
        }
        let assignments = resolve_default_assignments(&self.stmt.list, &tables, &schema)?;
        reject_derived_assignments(&assignments, &tables, &schema, &self.stmt.derived_tables)?;
        check_multi_update_key(&tables, &assignments, &schema)?;
        let left_values: Vec<Vec<Datum>> = left_rows.iter().map(|r| r.values.clone()).collect();
        let right_values: Vec<Vec<Datum>> = right_rows.iter().map(|r| r.values.clone()).collect();
        let pairs = hash_join_pairs(join, &left_names, &right_names, &left_values, &right_values)?;
        let left_width = left_names.len();
        // A self-join addresses one physical row through both aliases: their
        // assignments merge (base values plus per-alias column overrides).
        type JoinRow = (ScannedRow, Vec<Datum>, Vec<(usize, Datum)>);
        let mut merged: HashMap<(i64, i64, i64), JoinRow> = HashMap::new();
        let ignore = self.stmt.ignore;
        for (left_i, right_i) in pairs {
            let left_row = left_i.map(|i| &left_rows[i]);
            let right_row = right_i.map(|i| &right_rows[i]);
            let joined = concat_optional(left_row, right_row, left_width, right_names.len());
            // A join `ON` without a hash key (the flattened derived-table arm's
            // predicate) is evaluated here; the hash pairing above already
            // enforced the equality key.
            if matches!(join.tp, JoinType::Inner | JoinType::Cross)
                && !eval_filter(&join.on, &schema, &joined)?
            {
                continue;
            }
            if let Some(filter) = &self.stmt.selection
                && !eval_filter(filter, &schema, &joined)?
            {
                continue;
            }
            let mut left_new = left_row.map(|r| r.values.clone());
            let mut right_new = right_row.map(|r| r.values.clone());
            apply_assignments(
                &assignments,
                &schema,
                &joined,
                left_width,
                left_new.as_deref_mut(),
                right_new.as_deref_mut(),
            )?;
            cast_joined_assigned_values(
                &assignments,
                &schema,
                left_width,
                left_info.as_ref(),
                right_info.as_ref(),
                &mut left_new,
                &mut right_new,
            )?;
            if let (Some(info), Some(scanned), Some(values)) = (&left_info, left_row, left_new) {
                record_join_row(&mut merged, info, scanned, &values);
            }
            if let (Some(info), Some(scanned), Some(values)) = (&right_info, right_row, right_new) {
                record_join_row(&mut merged, info, scanned, &values);
            }
        }
        let mut matched = 0u64;
        let mut changed = 0u64;
        self.txn.with_txn(|txn| {
            for ((table_id, _, _), (scanned, base, overrides)) in merged {
                let Some(info) = left_info
                    .as_ref()
                    .filter(|info| info.id == table_id)
                    .or_else(|| right_info.as_ref().filter(|info| info.id == table_id))
                else {
                    continue;
                };
                // A table that the SET list does not address contributes join
                // rows but is neither matched nor changed (TiDB counts the
                // target rows of the updated tables only).
                if overrides.is_empty() {
                    continue;
                }
                matched += 1;
                let mut values = base;
                for (slot, value) in overrides {
                    values[slot] = value;
                }
                if values == scanned.values {
                    continue;
                }
                crate::insert::fill_generated_columns(info, &mut values)?;
                reject_null_not_null(info, &mut values, ignore)?;
                if values != scanned.values {
                    changed += 1;
                    apply_row_update(txn, info, &scanned, values)?;
                }
            }
            Ok(())
        })?;
        crate::internal::note_dml_affected(changed);
        crate::internal::set_dml_info(format!(
            "Rows matched: {matched}  Changed: {changed}  Warnings: {}",
            sealdb_expression::warnings_len()
        ));
        Ok(())
    }
}

impl UpdateExec {
    /// `UPDATE t1, t2, t3 SET …` / three-table JOIN: same row-by-row apply as
    /// the two-table path, but over a flat join chain (comma lists become
    /// `JOIN … ON TRUE`, so the product is filtered by `ON` / `WHERE`).
    fn update_join_chain(&mut self, join: &Join) -> Result<()> {
        let lock = dml_row_lock(&self.txn)?;
        let mut factors: Vec<Join> = vec![join.clone()];
        factors.extend(self.stmt.additional_joins.iter().cloned());
        let mut chain_tables = vec![join.left.clone()];
        chain_tables.extend(factors.iter().map(|factor| factor.right.clone()));
        let sides = chain_tables.len();
        let mut names = Vec::with_capacity(sides);
        let mut rows = Vec::with_capacity(sides);
        let mut infos: Vec<Option<TableInfo>> = Vec::with_capacity(sides);
        for table in &chain_tables {
            let (side_names, side_rows, side_info) = scan_dml_side(
                &self.domain,
                &self.current_db,
                &self.txn,
                self.stmt.with.as_ref(),
                table,
                lock,
            )?;
            names.push(side_names);
            rows.push(side_rows);
            infos.push(side_info);
        }
        let mut offsets = Vec::with_capacity(sides);
        let mut width = 0usize;
        for side_names in &names {
            offsets.push(width);
            width += side_names.len();
        }
        let schema: Vec<String> = chain_tables
            .iter()
            .zip(&names)
            .flat_map(|(table, cols)| cols.iter().map(move |c| format!("{}.{}", table.name, c)))
            .collect();
        let tables: Vec<(&str, &TableInfo, usize)> = chain_tables
            .iter()
            .zip(infos.iter())
            .zip(offsets.iter())
            .filter_map(|((table, info), offset)| {
                info.as_ref()
                    .map(|info| (table.name.as_str(), info, *offset))
            })
            .collect();
        let assignments = resolve_default_assignments(&self.stmt.list, &tables, &schema)?;
        reject_derived_assignments(&assignments, &tables, &schema, &self.stmt.derived_tables)?;
        check_multi_update_key(&tables, &assignments, &schema)?;
        // Aliases of one physical table may target the same row; assignments
        // from every alias merge onto that row (first base values, then each
        // alias's overrides in SET order).
        type ChainRow = (ScannedRow, Vec<Datum>, Vec<(usize, Datum)>);
        let mut merged: HashMap<(i64, i64, i64), ChainRow> = HashMap::new();
        let ignore = self.stmt.ignore;
        if rows.iter().any(Vec::is_empty) {
            return Ok(());
        }
        let mut indices = vec![0usize; sides];
        loop {
            let mut joined = Vec::with_capacity(width);
            for (side, &index) in indices.iter().enumerate() {
                joined.extend(rows[side][index].values.iter().cloned());
            }
            let mut matched = true;
            for factor in &factors {
                if !matches!(&factor.on, ExprNode::Value(v) if v.datum == Datum::I64(1))
                    && !eval_filter(&factor.on, &schema, &joined)?
                {
                    matched = false;
                    break;
                }
            }
            if matched
                && let Some(selection) = &self.stmt.selection
                && !eval_filter(selection, &schema, &joined)?
            {
                matched = false;
            }
            if matched {
                // Overrides are grouped per alias: TiDB applies each alias's
                // rewritten row in join order, so a later alias wins when two
                // aliases assign the same column.
                let mut per_side: Vec<Vec<(usize, Datum)>> = vec![Vec::new(); sides];
                for item in &assignments {
                    let idx = find_schema_index(&schema, item.table.as_deref(), &item.column)?;
                    let Some((side, slot)) = offsets
                        .iter()
                        .enumerate()
                        .find(|(i, start)| idx >= **start && idx < **start + names[*i].len())
                        .map(|(i, start)| (i, idx - start))
                    else {
                        continue;
                    };
                    let Some(info) = infos[side].as_ref() else {
                        continue;
                    };
                    let Some(col) = info.columns.get(slot) else {
                        continue;
                    };
                    let value = eval_expr_with_row(&item.expr, &schema, &joined)?;
                    let value = crate::insert::cast_for_column(&value, col)?;
                    per_side[side].push((slot, value));
                }
                for (side, overrides) in per_side.into_iter().enumerate() {
                    if overrides.is_empty() {
                        continue;
                    }
                    let Some(info) = infos[side].as_ref() else {
                        continue;
                    };
                    let scanned = &rows[side][indices[side]];
                    let key = (info.id, scanned.physical_id, scanned.handle);
                    let entry = merged
                        .entry(key)
                        .or_insert_with(|| (scanned.clone(), scanned.values.clone(), Vec::new()));
                    entry.2.extend(overrides);
                }
            }
            let mut carry = true;
            for (side, index) in indices.iter_mut().enumerate() {
                *index += 1;
                if *index < rows[side].len() {
                    carry = false;
                    break;
                }
                *index = 0;
            }
            if carry {
                break;
            }
        }
        let mut matched = 0u64;
        let mut changed = 0u64;
        self.txn.with_txn(|txn| {
            for ((table_id, _, _), (scanned, base, overrides)) in merged {
                let Some(info) = infos
                    .iter()
                    .flatten()
                    .find(|info| info.id == table_id)
                    .cloned()
                else {
                    continue;
                };
                let mut values = base;
                for (slot, value) in overrides {
                    values[slot] = value;
                }
                matched += 1;
                if values == scanned.values {
                    continue;
                }
                crate::insert::fill_generated_columns(&info, &mut values)?;
                reject_null_not_null(&info, &mut values, ignore)?;
                if values != scanned.values {
                    changed += 1;
                    apply_row_update(txn, &info, &scanned, values)?;
                }
            }
            Ok(())
        })?;
        crate::internal::note_dml_affected(changed);
        crate::internal::set_dml_info(format!(
            "Rows matched: {matched}  Changed: {changed}  Warnings: {}",
            sealdb_expression::warnings_len()
        ));
        Ok(())
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
/// Match a multi-table DELETE target list entry against a FROM/JOIN factor.
/// An aliased factor is only addressable by its alias (`DELETE t1 FROM t1 AS a`
/// is an unknown table in TiDB).
pub(crate) fn table_matches(target: &TableName, source: &TableName, current_db: &str) -> bool {
    let (target_schema, target_name) = target.qualified(current_db);
    let source_schema = source.schema.as_deref().unwrap_or(current_db);
    source_schema.eq_ignore_ascii_case(&target_schema)
        && source.name.eq_ignore_ascii_case(&target_name)
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

/// `(db, child table, fk, referenced table)` that blocks this UPDATE.
type FkViolation = (i32, String);

/// Body TiDB renders for both 1451 and 1452 (`… constaint fails (...)`).
fn fk_failure_body(
    db: &str,
    child: &TableInfo,
    fk: &sealdb_meta::ForeignKeyInfo,
    parent: &str,
) -> String {
    let columns = fk.cols.join("`, `");
    let refs = if fk.ref_cols.is_empty() {
        "id".to_string()
    } else {
        fk.ref_cols.join("`, `")
    };
    format!(
        "(`{db}`.`{}`, CONSTRAINT `{}` FOREIGN KEY (`{columns}`) REFERENCES `{parent}` (`{refs}`))",
        child.name.o(),
        fk.name
    )
}

/// Child-side FK check for one post-update row (`1452`): every non-NULL FK
/// value must match a row in the referenced table.
pub(crate) fn fk_child_violation(
    domain: &Domain,
    txn: &Arc<TxnHandle>,
    db: &str,
    info: &TableInfo,
    values: &[Datum],
    assigned: &HashSet<String>,
) -> Result<Option<FkViolation>> {
    if !sealdb_planner::core::foreign_key_checks() {
        return Ok(None);
    }
    let names = col_names(info);
    for fk in &info.foreign_keys {
        if fk.on_update_cascade {
            continue;
        }
        if !fk
            .cols
            .iter()
            .any(|c| assigned.contains(&c.to_ascii_lowercase()))
        {
            continue;
        }
        let Some(local): Option<Vec<usize>> = fk
            .cols
            .iter()
            .map(|c| names.iter().position(|n| n.eq_ignore_ascii_case(c)))
            .collect()
        else {
            continue;
        };
        let key_values: Vec<Datum> = local.iter().map(|&i| values[i].clone()).collect();
        if key_values.iter().any(Datum::is_null) {
            continue;
        }
        let Some(parent) = domain
            .infoschema()
            .table_by_name(db, &fk.ref_table)
            .cloned()
        else {
            continue;
        };
        let parent_names = col_names(&parent);
        let ref_cols: Vec<String> = if fk.ref_cols.is_empty() {
            parent
                .indices
                .iter()
                .find(|idx| idx.primary)
                .map(|idx| idx.columns.iter().map(|c| c.name.o().to_string()).collect())
                .unwrap_or_default()
        } else {
            fk.ref_cols.clone()
        };
        let Some(ref_idx): Option<Vec<usize>> = ref_cols
            .iter()
            .map(|c| parent_names.iter().position(|n| n.eq_ignore_ascii_case(c)))
            .collect()
        else {
            continue;
        };
        let physical = sealdb_planner::core::prune_partitions(&parent, None);
        let parent_rows = scan_table(txn, &parent, RowLock::None, LockWait::Block, &physical)?;
        let exists = parent_rows.iter().any(|row| {
            ref_idx.iter().zip(&key_values).all(|(&i, value)| {
                !row.values[i].is_null()
                    && (row.values[i] == *value
                        || sealdb_expression::datum_sql_eq(&row.values[i], value).unwrap_or(false))
            })
        });
        if !exists {
            return Ok(Some((
                ERR_NO_REFERENCED_ROW_2,
                format!(
                    "Cannot add or update a child row: a foreign key constraint fails {}",
                    fk_failure_body(db, info, fk, parent.name.o())
                ),
            )));
        }
    }
    Ok(None)
}

/// Parent-side FK check for one updated row (`1451`): if a key column that a
/// child references changes, no child row may still reference the old value.
pub(crate) fn fk_parent_violation(
    domain: &Domain,
    txn: &Arc<TxnHandle>,
    info: &TableInfo,
    old_values: &[Datum],
    new_values: &[Datum],
    assigned: &HashSet<String>,
) -> Result<Option<FkViolation>> {
    if !sealdb_planner::core::foreign_key_checks() {
        return Ok(None);
    }
    let is = domain.infoschema();
    let names = col_names(info);
    for schema in is.all_schemas() {
        let db = schema.name.o().to_string();
        for child in is.schema_tables(&db) {
            for fk in &child.foreign_keys {
                if fk.on_update_cascade || !fk.ref_table.eq_ignore_ascii_case(info.name.o()) {
                    continue;
                }
                let ref_cols: Vec<String> = if fk.ref_cols.is_empty() {
                    info.indices
                        .iter()
                        .find(|idx| idx.primary)
                        .map(|idx| idx.columns.iter().map(|c| c.name.o().to_string()).collect())
                        .unwrap_or_default()
                } else {
                    fk.ref_cols.clone()
                };
                if !ref_cols
                    .iter()
                    .any(|c| assigned.contains(&c.to_ascii_lowercase()))
                {
                    continue;
                }
                let Some(parent_idx): Option<Vec<usize>> = ref_cols
                    .iter()
                    .map(|c| names.iter().position(|n| n.eq_ignore_ascii_case(c)))
                    .collect()
                else {
                    continue;
                };
                let old_key: Vec<Datum> =
                    parent_idx.iter().map(|&i| old_values[i].clone()).collect();
                let new_key: Vec<Datum> =
                    parent_idx.iter().map(|&i| new_values[i].clone()).collect();
                if old_key.iter().any(Datum::is_null) || old_key == new_key {
                    continue;
                }
                let child_names = col_names(&child);
                let Some(child_idx): Option<Vec<usize>> = fk
                    .cols
                    .iter()
                    .map(|c| child_names.iter().position(|n| n.eq_ignore_ascii_case(c)))
                    .collect()
                else {
                    continue;
                };
                let physical = sealdb_planner::core::prune_partitions(&child, None);
                let child_rows =
                    scan_table(txn, &child, RowLock::None, LockWait::Block, &physical)?;
                let referenced = child_rows.iter().any(|row| {
                    child_idx.iter().zip(&old_key).all(|(&i, value)| {
                        !row.values[i].is_null()
                            && (row.values[i] == *value
                                || sealdb_expression::datum_sql_eq(&row.values[i], value)
                                    .unwrap_or(false))
                    })
                });
                if referenced {
                    return Ok(Some((
                        sealdb_errno::ERR_ROW_IS_REFERENCED_2,
                        format!(
                            "Cannot delete or update a parent row: a foreign key constraint fails {}",
                            fk_failure_body(&db, &child, fk, info.name.o())
                        ),
                    )));
                }
            }
        }
    }
    Ok(None)
}

/// Assigned column names (lower-case) of the UPDATE list.
fn assigned_columns(assignments: &[Assignment]) -> HashSet<String> {
    assignments
        .iter()
        .map(|item| item.column.to_ascii_lowercase())
        .collect()
}

/// `UPDATE IGNORE` drops the offending row instead of failing: child-side
/// violations warn (`1452`), parent-side ones are skipped silently (TiDB does
/// not surface a warning for the referenced-row check).
pub(crate) fn apply_fk_outcome(violation: Option<FkViolation>, ignore: bool) -> Result<bool> {
    let Some((code, message)) = violation else {
        return Ok(false);
    };
    if !ignore {
        return Err(Error::new(code, message));
    }
    if code == ERR_NO_REFERENCED_ROW_2 {
        sealdb_expression::note_warning(code, message);
    }
    Ok(true)
}

/// Coerce each assigned value to its target column's type, exactly like an
/// INSERT would (temporal/enum/float-range checks).
fn cast_assigned_values(
    assignments: &[Assignment],
    info: &TableInfo,
    schema: &[String],
    values: &mut [Datum],
) -> Result<()> {
    for item in assignments {
        let idx = find_schema_index(schema, item.table.as_deref(), &item.column)?;
        if let Some(col) = info.columns.get(idx) {
            values[idx] = crate::insert::cast_for_column(&values[idx], col)?;
        }
    }
    Ok(())
}

/// Two-table variant: an assignment's column slot lives on the left or the
/// right side of the joined schema.
#[allow(clippy::too_many_arguments)]
fn cast_joined_assigned_values(
    assignments: &[Assignment],
    schema: &[String],
    left_width: usize,
    left_info: Option<&TableInfo>,
    right_info: Option<&TableInfo>,
    left_values: &mut Option<Vec<Datum>>,
    right_values: &mut Option<Vec<Datum>>,
) -> Result<()> {
    for item in assignments {
        let idx = find_schema_index(schema, item.table.as_deref(), &item.column)?;
        if idx < left_width {
            if let (Some(info), Some(values)) = (left_info, left_values.as_mut())
                && let Some(col) = info.columns.get(idx)
            {
                let casted = crate::insert::cast_for_column(&values[idx], col)?;
                values[idx] = casted;
            }
        } else if let (Some(info), Some(values)) = (right_info, right_values.as_mut()) {
            let slot = idx - left_width;
            if let Some(col) = info.columns.get(slot) {
                let casted = crate::insert::cast_for_column(&values[slot], col)?;
                values[slot] = casted;
            }
        }
    }
    Ok(())
}

/// Derived / comma-rewritten sides are readable but not updatable targets:
/// assigning to one is TiDB 1288 (`The target table b of the UPDATE is not
/// updatable`).
fn reject_derived_assignments(
    assignments: &[Assignment],
    tables: &[(&str, &TableInfo, usize)],
    schema: &[String],
    derived: &[String],
) -> Result<()> {
    for item in assignments {
        let idx = find_schema_index(schema, item.table.as_deref(), &item.column)?;
        let Some((alias, _, _)) = tables
            .iter()
            .find(|(_, info, offset)| idx >= *offset && idx < *offset + info.columns.len())
        else {
            continue;
        };
        if derived.iter().any(|name| name.eq_ignore_ascii_case(alias)) {
            return Err(Error::new(
                ERR_NON_UPDATABLE_TABLE,
                format!("The target table {alias} of the UPDATE is not updatable"),
            ));
        }
    }
    Ok(())
}

/// TiDB `CheckUpdateList` (1706): when one physical table is updated through
/// several aliases, updating its primary-key or partition-key column through
/// any of them is rejected, because the aliases address the same rows.
fn check_multi_update_key(
    tables: &[(&str, &TableInfo, usize)],
    assignments: &[Assignment],
    schema: &[String],
) -> Result<()> {
    let mut flags = vec![(false, false, false); tables.len()];
    for item in assignments {
        let idx = find_schema_index(schema, item.table.as_deref(), &item.column)?;
        let Some(pos) = tables
            .iter()
            .position(|(_, info, offset)| idx >= *offset && idx < *offset + info.columns.len())
        else {
            continue;
        };
        let (alias, info, offset) = tables[pos];
        let col = &info.columns[idx - offset];
        let name = col.name.o();
        flags[pos].0 = true;
        if info.indices.iter().filter(|i| i.primary).any(|i| {
            i.columns
                .iter()
                .any(|c| c.name.o().eq_ignore_ascii_case(name))
        }) {
            flags[pos].1 = true;
        }
        if let Some(part) = &info.partition
            && part
                .columns
                .iter()
                .any(|c| c.o().eq_ignore_ascii_case(name))
        {
            flags[pos].2 = true;
        }
        let _ = alias;
    }
    let mut seen: HashMap<i64, (String, bool, bool)> = HashMap::new();
    for ((alias, info, _), (updated, pk, part)) in tables.iter().zip(flags.iter()) {
        if !updated {
            continue;
        }
        match seen.get(&info.id) {
            Some((other, other_pk, other_part)) => {
                if *other_pk || *pk || *other_part || *part {
                    return Err(Error::new(
                        ERR_MULTI_UPDATE_KEY_CONFLICT,
                        format!(
                            "Primary key/partition key update is not allowed since the table is updated both as '{other}' and '{alias}'."
                        ),
                    ));
                }
            }
            None => {
                seen.insert(info.id, (alias.to_string(), *pk, *part));
            }
        }
    }
    Ok(())
}

/// `DEFAULT` target of an assignment: `Some(None)` for the bare keyword,
/// `Some(Some((table, column)))` for `DEFAULT(col)`, `None` for a plain value.
fn default_target(expr: &ExprNode) -> Option<Option<(Option<String>, String)>> {
    match expr {
        ExprNode::Column(c)
            if c.schema.is_none()
                && c.table.is_none()
                && c.name.eq_ignore_ascii_case("default") =>
        {
            Some(None)
        }
        ExprNode::Func(f) if f.name.eq_ignore_ascii_case("default") && f.args.len() == 1 => {
            match &f.args[0] {
                ExprNode::Column(c) => Some(Some((
                    c.table.clone().or_else(|| c.schema.clone()),
                    c.name.clone(),
                ))),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Resolve TiDB's `SET c = DEFAULT` / `SET c = DEFAULT(x)` and enforce the
/// generated-column write guard (3105): a generated column may only be
/// assigned the bare `DEFAULT` or `DEFAULT(itself)`, both of which mean
/// "recompute from the base columns".
fn resolve_default_assignments(
    list: &[Assignment],
    tables: &[(&str, &TableInfo, usize)],
    schema: &[String],
) -> Result<Vec<Assignment>> {
    let mut out = Vec::with_capacity(list.len());
    for item in list {
        let idx = find_schema_index(schema, item.table.as_deref(), &item.column)?;
        let owner = tables
            .iter()
            .find(|(_, info, offset)| idx >= *offset && idx < *offset + info.columns.len());
        let Some((alias, info, offset)) = owner else {
            out.push(item.clone());
            continue;
        };
        let col = &info.columns[idx - offset];
        let generated = col.generated_expr.is_some();
        let target = default_target(&item.expr);
        let generated_error = || {
            Error::new(
                ERR_NON_DEFAULT_VALUE_FOR_GENERATED_COLUMN,
                format!(
                    "The value specified for generated column '{}' in table '{}' is not allowed.",
                    col.name.o(),
                    info.name.o()
                ),
            )
        };
        match (generated, target) {
            (false, Some(None)) => out.push(Assignment {
                table: item.table.clone(),
                column: item.column.clone(),
                expr: ExprNode::Value(ValueExpr {
                    datum: crate::insert::column_default_value(col)?,
                }),
            }),
            (false, Some(Some((table, name)))) => {
                let source = find_default_column(info, table.as_deref(), &name)?;
                out.push(Assignment {
                    table: item.table.clone(),
                    column: item.column.clone(),
                    expr: ExprNode::Value(ValueExpr {
                        datum: crate::insert::column_default_value(source)?,
                    }),
                });
            }
            (false, None) => out.push(item.clone()),
            // `SET gen_col = DEFAULT` / `SET gen_col = DEFAULT(gen_col)`:
            // drop the assignment so `fill_generated_columns` recomputes it.
            (true, Some(None)) => {}
            (true, Some(Some((table, name))))
                if name.eq_ignore_ascii_case(col.name.o())
                    && table
                        .as_deref()
                        .is_none_or(|t| t.eq_ignore_ascii_case(alias)) => {}
            (true, _) => return Err(generated_error()),
        }
    }
    Ok(out)
}

fn find_default_column<'a>(
    info: &'a TableInfo,
    table: Option<&str>,
    name: &str,
) -> Result<&'a ColumnInfo> {
    if let Some(table) = table
        && !table.eq_ignore_ascii_case(info.name.o())
    {
        return Err(Error::new(
            ERR_BAD_FIELD,
            format!("Unknown column '{name}' in 'field list'"),
        ));
    }
    info.columns
        .iter()
        .find(|c| c.name.o().eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            Error::new(
                ERR_BAD_FIELD,
                format!("Unknown column '{name}' in 'field list'"),
            )
        })
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

/// `NOT NULL` columns reject an explicit NULL assignment (`1048`); under
/// `UPDATE IGNORE` the row keeps the type's zero value with a warning.
fn reject_null_not_null(
    info: &sealdb_meta::TableInfo,
    values: &mut [Datum],
    ignore: bool,
) -> Result<()> {
    for (col, value) in info.columns.iter().zip(values.iter_mut()) {
        if value.is_null() && col.field_type.flag & sealdb_types::field_type::NOT_NULL_FLAG != 0 {
            let msg = format!("Column '{}' cannot be null", col.name.o());
            let strict = sealdb_expression::sql_mode_flag("STRICT_TRANS_TABLES")
                || sealdb_expression::sql_mode_flag("STRICT_ALL_TABLES");
            if !sealdb_expression::strict_not_null_check() {
                *value = crate::insert::implicit_zero(&col.field_type);
            } else if ignore || !strict {
                sealdb_expression::note_warning(ERR_BAD_NULL, msg);
                *value = crate::insert::implicit_zero(&col.field_type);
            } else {
                return Err(Error::new(ERR_BAD_NULL, msg));
            }
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
            ) && let Some(message) = truncating_pair(info, &bin.left, &bin.right)
            {
                out.push(message);
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
    apply_row_updates(txn, info, &[(scanned.clone(), new_values)])
}

/// Apply several updates of one table atomically: every old row key and index
/// entry is removed before any new row is written, so a statement that shifts
/// primary keys in place (`UPDATE t SET id = id + 1`) cannot overwrite a row
/// that has not been moved yet.
pub(crate) fn apply_row_updates(
    txn: &mut dyn Transaction,
    info: &TableInfo,
    rows: &[(ScannedRow, Vec<Datum>)],
) -> Result<()> {
    let col_ids: Vec<i64> = info.columns.iter().map(|c| c.id).collect();
    let mut planned = Vec::with_capacity(rows.len());
    for (scanned, new_values) in rows {
        // Only an integer clustered PRIMARY KEY changes the row handle; heap
        // tables keep their allocated `_tidb_rowid` across updates.
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
        let new_physical = locate_partition_for_row(info, new_values)?;
        planned.push((scanned, new_values, new_handle, new_physical));
    }
    for (scanned, _, _, _) in &planned {
        delete_index_keys(
            txn,
            info,
            scanned.handle,
            &scanned.values,
            scanned.physical_id,
        )?;
        txn.delete(encode_row_key_with_handle(scanned.physical_id, scanned.handle).as_bytes())?;
    }
    for (_, new_values, new_handle, new_physical) in &planned {
        txn.set(
            encode_row_key_with_handle(*new_physical, *new_handle).as_bytes(),
            &encode_row(&col_ids, new_values),
        )?;
        write_index_keys(txn, info, *new_handle, new_values, *new_physical)?;
    }
    Ok(())
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
        let bound = bind_outer_refs(&item.expr, info.name.o(), col_names, values, &[]);
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
