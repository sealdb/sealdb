// single-table UPDATE SET (ORDER BY / LIMIT) and two-table UPDATE JOIN.
// pessimistic UPDATE reads latest and takes exclusive locks.
// Completeness wave / not this feature: n-table UPDATE, UPDATE FROM, comma joins,
// RETURNING (not in TiDB v8.5.8 / MySQL 8.0), MySQL-protocol affected-row counts.
use crate::cte::eval_named_cte;
use crate::index::{delete_index_keys, write_index_keys};
use crate::internal::exec::Executor;
use crate::join::hash_join_v1::hash_join_pairs;
use crate::sortexec::sort::sort_rows_by;
use crate::table_reader::{ScannedRow, scan_table};
use sealdb_domain::Domain;
use sealdb_errno::{
    ERR_BAD_FIELD, ERR_NO_DB, ERR_NO_SUCH_TABLE, ERR_VIEW_CHECK_FAILED, Error, Result,
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
        let physical = info.prune_partitions(self.stmt.selection.as_ref());
        let rows = scan_table(&self.txn, &info, lock, LockWait::Block, &physical)?;
        let mut matched = Vec::new();
        for scanned in rows {
            if let Some(filter) = &self.stmt.selection
                && !eval_filter(filter, &col_names, &scanned.values)?
            {
                continue;
            }
            matched.push(scanned);
        }
        sort_rows_by(&mut matched, &self.stmt.order_by, &col_names, |row| {
            &row.values
        });
        if let Some(limit) = self.stmt.limit {
            matched.truncate(limit as usize);
        }
        self.txn.with_txn(|txn| {
            for scanned in matched {
                let mut new_values = scanned.values.clone();
                apply_assignments(
                    &self.stmt.list,
                    &col_names,
                    &scanned.values,
                    col_names.len(),
                    Some(&mut new_values),
                    None,
                )?;
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
                apply_row_update(txn, &info, &scanned, new_values)?;
            }
            Ok(())
        })
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
        self.txn.with_txn(|txn| {
            if let Some(left) = &left_info {
                for (_, (scanned, values)) in left_updates {
                    apply_row_update(txn, left, &scanned, values)?;
                }
            }
            if let Some(right) = &right_info {
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
        &info.prune_partitions(None),
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
fn apply_assignments(
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

/// Rewrite one row: drop old index keys, delete the old record if the handle or
/// partition moved, then put the new row and unique/secondary indexes.
pub(crate) fn apply_row_update(
    txn: &mut dyn Transaction,
    info: &TableInfo,
    scanned: &ScannedRow,
    new_values: Vec<Datum>,
) -> Result<()> {
    let col_ids: Vec<i64> = info.columns.iter().map(|c| c.id).collect();
    let new_handle = new_values
        .first()
        .and_then(|d| match d {
            Datum::I64(v) => Some(*v),
            _ => None,
        })
        .unwrap_or(scanned.handle);
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
