use crate::index::write_index_keys;
use crate::join::apply::SubqueryEvalCtx;
use crate::internal::exec::Executor;
use crate::table_reader::scan_table;
use sealdb_domain::Domain;
use sealdb_errno::{
    ERR_DUP_ENTRY, ERR_NO_DB, ERR_NO_SUCH_TABLE, ERR_WRONG_VALUE_COUNT, Error, Result,
};
use sealdb_expression::{eval_expr_with_row, locate_partition_for_row};
use sealdb_kv::{LockWait, TxnHandle};
use sealdb_parser::ast::InsertStmt;
use sealdb_tablecodec::{encode_row, encode_row_key_with_handle};
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;
use std::sync::Arc;

pub struct InsertExec {
    domain: Arc<Domain>,
    stmt: InsertStmt,
    current_db: String,
    txn: Arc<TxnHandle>,
    select: Option<Box<dyn Executor>>,
    done: bool,
}

impl InsertExec {
    pub fn new(
        domain: Arc<Domain>,
        stmt: InsertStmt,
        current_db: impl Into<String>,
        txn: Arc<TxnHandle>,
        select: Option<Box<dyn Executor>>,
    ) -> Self {
        Self {
            domain,
            stmt,
            current_db: current_db.into(),
            txn,
            select,
            done: false,
        }
    }
}

impl Executor for InsertExec {
    fn open(&mut self) -> Result<()> {
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
        let source_rows = if let Some(select) = &mut self.select {
            select.open()?;
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
        if self.select.is_some() {
            for row in &source_rows {
                pending.push(map_insert_row(info, &self.stmt.columns, row)?);
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
            for row in &self.stmt.rows {
                pending.push(eval_insert_row(
                    info,
                    &self.stmt.columns,
                    row,
                    &mut subquery,
                )?);
            }
        }
        if self.stmt.on_duplicate.is_empty() {
            return self.txn.with_txn(|txn| {
                for values in &pending {
                    write_row(txn, &self.domain, info, values)?;
                }
                Ok(())
            });
        }
        // `ON DUPLICATE KEY UPDATE`: locate conflicts against a snapshot of the
        // table, then update the conflicting row instead of inserting.
        let lock = crate::update::dml_row_lock(&self.txn)?;
        let physical = info.prune_partitions(None);
        let mut existing = scan_table(&self.txn, info, lock, LockWait::Block, &physical)?;
        let names = crate::update::col_names(info);
        self.txn.with_txn(|txn| {
            for values in pending {
                match find_conflict(info, &existing, None, &values) {
                    Some((idx, _)) => {
                        let mut merged = existing[idx].values.clone();
                        crate::update::apply_assignments(
                            &self.stmt.on_duplicate,
                            &names,
                            &existing[idx].values,
                            names.len(),
                            Some(&mut merged),
                            None,
                        )?;
                        fill_generated_columns(info, &mut merged)?;
                        if let Some((_, dup_idx)) =
                            find_conflict(info, &existing, Some(idx), &merged)
                        {
                            return Err(duplicate_error(info, dup_idx, &merged));
                        }
                        crate::update::apply_row_update(
                            txn,
                            info,
                            &existing[idx],
                            merged.clone(),
                        )?;
                        existing[idx].values = merged;
                    }
                    None => {
                        let (handle, physical_id) =
                            write_row(txn, &self.domain, info, &values)?;
                        existing.push(crate::table_reader::ScannedRow {
                            handle,
                            physical_id,
                            values,
                        });
                    }
                }
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

fn eval_insert_row(
    info: &sealdb_meta::TableInfo,
    columns: &[String],
    row: &[sealdb_parser::ast::ExprNode],
    subquery: &mut SubqueryEvalCtx,
) -> Result<Vec<Datum>> {
    let mut values = Vec::with_capacity(row.len());
    for expr in row {
        // `DEFAULT` in VALUES: a generated column recomputes it below; other
        // columns fall back to NULL until column defaults land.
        if is_default_keyword(expr) {
            values.push(Datum::Null);
            continue;
        }
        values.push(subquery.eval(expr, &[], &[])?);
    }
    map_insert_row(info, columns, &values)
}

fn is_default_keyword(expr: &sealdb_parser::ast::ExprNode) -> bool {
    matches!(
        expr,
        sealdb_parser::ast::ExprNode::Column(c)
            if c.table.is_none() && c.schema.is_none() && c.name.eq_ignore_ascii_case("default")
    )
}

fn map_insert_row(
    info: &sealdb_meta::TableInfo,
    columns: &[String],
    row: &[Datum],
) -> Result<Vec<Datum>> {
    if columns.is_empty() {
        if row.len() != info.columns.len() {
            return Err(Error::new(
                ERR_WRONG_VALUE_COUNT,
                "Column count doesn't match value count",
            ));
        }
        // Cast literals to column FieldType (UNSIGNED BIGINT / DECIMAL scale).
        let mut values = Vec::with_capacity(info.columns.len());
        for (col, v) in info.columns.iter().zip(row.iter()) {
            values.push(cast_for_column(v, col)?);
        }
        fill_generated_columns(info, &mut values)?;
        return Ok(values);
    }
    if row.len() != columns.len() {
        return Err(Error::new(
            ERR_WRONG_VALUE_COUNT,
            "Column count doesn't match value count",
        ));
    }
    let mut values = Vec::with_capacity(info.columns.len());
    for col in &info.columns {
        if let Some(idx) = columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(col.name.o()))
        {
            values.push(cast_for_column(&row[idx], col)?);
        } else {
            values.push(Datum::Null);
        }
    }
    fill_generated_columns(info, &mut values)?;
    Ok(values)
}

/// Evaluate `GENERATED ALWAYS AS (expr)` columns in ascending offset order so a
/// generated column may reference earlier generated ones. Values are stored
/// physically (VIRTUAL columns included) until a lazy-read path lands.
pub(crate) fn fill_generated_columns(info: &sealdb_meta::TableInfo, values: &mut [Datum]) -> Result<()> {
    if info.columns.iter().all(|c| c.generated_expr.is_none()) {
        return Ok(());
    }
    let schema: Vec<String> = info.columns.iter().map(|c| c.name.o().to_string()).collect();
    let mut exprs = Vec::new();
    for col in &info.columns {
        let Some(text) = col.generated_expr.as_deref() else {
            continue;
        };
        let expr = sealdb_parser::sqlparser_bridge::lower::parse_expr(text)?;
        exprs.push((col.offset as usize, expr, col));
    }
    for (offset, expr, col) in exprs {
        if offset >= values.len() {
            continue;
        }
        let value = eval_expr_with_row(&expr, &schema, values)?;
        values[offset] = cast_for_column(&value, col)?;
    }
    Ok(())
}

fn cast_for_column(v: &Datum, col: &sealdb_meta::ColumnInfo) -> Result<Datum> {
    let mut out = v.cast(&col.field_type)?;
    // TIMESTAMP columns store UTC; session wall clock → UTC on write.
    if col.field_type.tp == sealdb_types::TYPE_TIMESTAMP
        && let Datum::Time(t) = out
    {
        let mut t = t;
        t.tp = sealdb_types::TimeType::Timestamp;
        t = sealdb_expression::session_local_to_utc(t)?;
        t.tp = sealdb_types::TimeType::Timestamp;
        out = Datum::Time(t);
    }
    Ok(out)
}

fn write_row(
    txn: &mut dyn sealdb_kv::Transaction,
    domain: &Domain,
    info: &sealdb_meta::TableInfo,
    values: &[Datum],
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
    let handle = if info.pk_is_handle {
        match pk_offset.and_then(|off| values.get(off)) {
            Some(Datum::I64(v)) => *v,
            Some(Datum::U64(v)) => *v as i64,
            _ => domain.next_handle(info.id)?,
        }
    } else {
        domain.next_handle(info.id)?
    };
    let physical_id = locate_partition_for_row(info, values)?;
    let key = encode_row_key_with_handle(physical_id, handle);
    if info.indices.iter().any(|idx| idx.primary) && txn.get(key.as_bytes())?.is_some() {
        let value = pk_offset
            .and_then(|off| values.get(off))
            .map(|v| v.display())
            .unwrap_or_default();
        return Err(Error::new(
            ERR_DUP_ENTRY,
            format!("Duplicate entry '{value}' for key '{}.PRIMARY'", info.name.o()),
        ));
    }
    txn.set(key.as_bytes(), &encode_row(&col_ids, values))?;
    write_index_keys(txn, info, handle, values, physical_id)?;
    Ok((handle, physical_id))
}

/// Index columns equal on every column (NULL never conflicts). Returns the
/// first conflicting unique index, if any.
fn index_conflict(
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
            let av = a.get(col.offset as usize);
            let bv = b.get(col.offset as usize);
            match (av, bv) {
                (Some(av), Some(bv)) if !av.is_null() && !bv.is_null() && av == bv => {}
                _ => {
                    all_eq = false;
                    break;
                }
            }
        }
        if all_eq {
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

fn duplicate_error(info: &sealdb_meta::TableInfo, index_idx: usize, values: &[Datum]) -> Error {
    let index = &info.indices[index_idx];
    let value = index
        .columns
        .iter()
        .map(|c| {
            values
                .get(c.offset as usize)
                .map(|v| v.display())
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
