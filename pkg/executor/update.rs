// F07: single-table UPDATE SET (ORDER BY / LIMIT) and two-table UPDATE JOIN.
// F13: pessimistic UPDATE reads latest and takes exclusive locks.
// Completeness wave / not this feature: n-table UPDATE, UPDATE FROM, comma joins,
// RETURNING (not in TiDB v8.5.8 / MySQL 8.0), MySQL-protocol affected-row counts.
use crate::index::{delete_index_keys, write_index_keys};
use crate::internal::exec::Executor;
use crate::join::hash_join_v1::hash_join_pairs;
use crate::sortexec::sort::sort_rows_by;
use crate::table_reader::{ScannedRow, scan_table};
use sealdb_domain::Domain;
use sealdb_errno::{ERR_BAD_FIELD, ERR_NO_DB, ERR_NO_SUCH_TABLE, Error, Result};
use sealdb_expression::{eval_expr_with_row, eval_filter};
use sealdb_kv::{Transaction, TxnHandle};
use sealdb_meta::TableInfo;
use sealdb_parser::ast::ddl::TableName;
use sealdb_parser::ast::{Assignment, Join, UpdateStmt};
use sealdb_tablecodec::{encode_row, encode_row_key_with_handle};
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;
use std::collections::HashMap;
use std::sync::Arc;

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
        let for_update = self.txn.with_txn(|txn| Ok(txn.is_pessimistic()))?;
        let physical = info.prune_partitions(self.stmt.selection.as_ref());
        let rows = scan_table(&self.txn, &info, for_update, &physical)?;
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
    fn update_join(&mut self, join: &Join) -> Result<()> {
        let left = load_table(&self.domain, &self.current_db, &join.left)?;
        let right = load_table(&self.domain, &self.current_db, &join.right)?;
        let left_names = col_names(&left);
        let right_names = col_names(&right);
        let schema = joined_schema(&join.left.name, &left_names, &join.right.name, &right_names);
        let for_update = self.txn.with_txn(|txn| Ok(txn.is_pessimistic()))?;
        let left_rows = scan_table(&self.txn, &left, for_update, &left.prune_partitions(None))?;
        let right_rows = scan_table(&self.txn, &right, for_update, &right.prune_partitions(None))?;
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
            if let (Some(scanned), Some(values)) = (left_row, left_new) {
                left_updates.insert(
                    (scanned.physical_id, scanned.handle),
                    (scanned.clone(), values),
                );
            }
            if let (Some(scanned), Some(values)) = (right_row, right_new) {
                right_updates.insert(
                    (scanned.physical_id, scanned.handle),
                    (scanned.clone(), values),
                );
            }
        }
        self.txn.with_txn(|txn| {
            for (_, (scanned, values)) in left_updates {
                apply_row_update(txn, &left, &scanned, values)?;
            }
            for (_, (scanned, values)) in right_updates {
                apply_row_update(txn, &right, &scanned, values)?;
            }
            Ok(())
        })
    }
}

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

pub(crate) fn col_names(info: &TableInfo) -> Vec<String> {
    info.columns
        .iter()
        .map(|c| c.name.o().to_string())
        .collect()
}

pub(crate) fn table_matches(name: &TableName, wanted: &TableName, current_db: &str) -> bool {
    let (s1, t1) = name.qualified(current_db);
    let (s2, t2) = wanted.qualified(current_db);
    s1.eq_ignore_ascii_case(&s2) && t1.eq_ignore_ascii_case(&t2)
}

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
    let new_part = info
        .partition_value(&new_values)
        .cloned()
        .unwrap_or(Datum::Null);
    let new_physical = info.locate_partition(&new_part)?;
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
