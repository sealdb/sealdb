// F07: single-table DELETE (ORDER BY / LIMIT) and two-table DELETE ... JOIN.
// F13: pessimistic DELETE reads latest and takes exclusive locks.
// Completeness wave / not this feature: n-table DELETE, comma FROM without USING,
// RETURNING (not in TiDB v8.5.8 / MySQL 8.0), MySQL-protocol affected-row counts.
use crate::index::delete_index_keys;
use crate::internal::exec::Executor;
use crate::join::hash_join_v1::hash_join_pairs;
use crate::sortexec::sort::sort_rows_by;
use crate::table_reader::{ScannedRow, scan_table};
use crate::update::{col_names, joined_schema, load_table, table_matches};
use sealdb_domain::Domain;
use sealdb_errno::{ERR_NO_SUCH_TABLE, Error, Result};
use sealdb_expression::eval_filter;
use sealdb_kv::{Transaction, TxnHandle};
use sealdb_meta::TableInfo;
use sealdb_parser::ast::DeleteStmt;
use sealdb_parser::ast::Join;
use sealdb_tablecodec::encode_row_key_with_handle;
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// `DELETE` executor: single-table path, or two-table JOIN deleting listed tables.
pub struct DeleteExec {
    domain: Arc<Domain>,
    stmt: DeleteStmt,
    current_db: String,
    txn: Arc<TxnHandle>,
    done: bool,
}

impl DeleteExec {
    pub fn new(
        domain: Arc<Domain>,
        stmt: DeleteStmt,
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

impl Executor for DeleteExec {
    fn open(&mut self) -> Result<()> {
        if let Some(join) = self.stmt.join.clone() {
            return self.delete_join(&join);
        }
        if !self.stmt.tables.is_empty()
            && !self
                .stmt
                .tables
                .iter()
                .all(|t| table_matches(t, &self.stmt.table, &self.current_db))
        {
            return Err(Error::new(
                ERR_NO_SUCH_TABLE,
                "Unknown table in multi-table DELETE",
            ));
        }
        let info = load_table(&self.domain, &self.current_db, &self.stmt.table)?;
        let names = col_names(&info);
        let for_update = self.txn.with_txn(|txn| Ok(txn.is_pessimistic()))?;
        let physical = info.prune_partitions(self.stmt.selection.as_ref());
        let rows = scan_table(&self.txn, &info, for_update, &physical)?;
        let mut matched = Vec::new();
        for scanned in rows {
            if let Some(filter) = &self.stmt.selection
                && !eval_filter(filter, &names, &scanned.values)?
            {
                continue;
            }
            matched.push(scanned);
        }
        sort_rows_by(&mut matched, &self.stmt.order_by, &names, |row| &row.values);
        if let Some(limit) = self.stmt.limit {
            matched.truncate(limit as usize);
        }
        self.txn.with_txn(|txn| {
            for scanned in matched {
                apply_row_delete(txn, &info, &scanned)?;
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

impl DeleteExec {
    /// `DELETE t1 [, t2] FROM t1 JOIN t2 ...`: join, dedup handles, delete listed sides.
    fn delete_join(&mut self, join: &Join) -> Result<()> {
        let delete_left = self
            .stmt
            .tables
            .iter()
            .any(|t| table_matches(t, &join.left, &self.current_db));
        let delete_right = self
            .stmt
            .tables
            .iter()
            .any(|t| table_matches(t, &join.right, &self.current_db));
        if !delete_left && !delete_right {
            return Err(Error::new(
                ERR_NO_SUCH_TABLE,
                "Unknown table in multi-table DELETE",
            ));
        }
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
        let mut left_del: HashSet<(i64, i64)> = HashSet::new();
        let mut right_del: HashSet<(i64, i64)> = HashSet::new();
        let mut left_scanned: HashMap<(i64, i64), ScannedRow> = HashMap::new();
        let mut right_scanned: HashMap<(i64, i64), ScannedRow> = HashMap::new();
        for (left_i, right_i) in pairs {
            let left_row = left_i.map(|i| &left_rows[i]);
            let right_row = right_i.map(|i| &right_rows[i]);
            let joined = concat_optional(left_row, right_row, left_names.len(), right_names.len());
            if let Some(filter) = &self.stmt.selection
                && !eval_filter(filter, &schema, &joined)?
            {
                continue;
            }
            if delete_left && let Some(scanned) = left_row {
                let key = (scanned.physical_id, scanned.handle);
                left_del.insert(key);
                left_scanned.entry(key).or_insert_with(|| scanned.clone());
            }
            if delete_right && let Some(scanned) = right_row {
                let key = (scanned.physical_id, scanned.handle);
                right_del.insert(key);
                right_scanned.entry(key).or_insert_with(|| scanned.clone());
            }
        }
        self.txn.with_txn(|txn| {
            for key in left_del {
                if let Some(scanned) = left_scanned.remove(&key) {
                    apply_row_delete(txn, &left, &scanned)?;
                }
            }
            for key in right_del {
                if let Some(scanned) = right_scanned.remove(&key) {
                    apply_row_delete(txn, &right, &scanned)?;
                }
            }
            Ok(())
        })
    }
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

/// Drop index keys then the row record for one scanned handle.
fn apply_row_delete(
    txn: &mut dyn Transaction,
    info: &TableInfo,
    scanned: &ScannedRow,
) -> Result<()> {
    delete_index_keys(
        txn,
        info,
        scanned.handle,
        &scanned.values,
        scanned.physical_id,
    )?;
    txn.delete(encode_row_key_with_handle(scanned.physical_id, scanned.handle).as_bytes())
}
