use crate::index::write_index_keys;
use crate::internal::exec::Executor;
use sealdb_domain::Domain;
use sealdb_errno::{
    ERR_DUP_ENTRY, ERR_NO_DB, ERR_NO_SUCH_TABLE, ERR_WRONG_VALUE_COUNT, Error, Result,
};
use sealdb_expression::{eval_expr, eval_expr_with_row, locate_partition_for_row};
use sealdb_kv::TxnHandle;
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
        self.txn.with_txn(|txn| {
            if self.select.is_some() {
                for row in &source_rows {
                    let values = map_insert_row(info, &self.stmt.columns, row)?;
                    write_row(txn, &self.domain, info, &values)?;
                }
            } else {
                for row in &self.stmt.rows {
                    let values = eval_insert_row(info, &self.stmt.columns, row)?;
                    write_row(txn, &self.domain, info, &values)?;
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
) -> Result<Vec<Datum>> {
    let mut values = Vec::with_capacity(row.len());
    for expr in row {
        // `DEFAULT` in VALUES: a generated column recomputes it below; other
        // columns fall back to NULL until column defaults land.
        if is_default_keyword(expr) {
            values.push(Datum::Null);
            continue;
        }
        values.push(eval_expr(expr)?);
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
) -> Result<()> {
    let col_ids: Vec<_> = info.columns.iter().map(|c| c.id).collect();
    // Integer primary-key handle only when `pk_is_handle`; otherwise allocate
    // a unique _tidb_rowid (TiDB heap table). The handle is the PRIMARY KEY
    // column value (not always column 0 — e.g. `wkey, pkey PRIMARY KEY(pkey)`).
    let handle = if info.pk_is_handle {
        let pk_offset = info
            .indices
            .iter()
            .find(|idx| idx.primary)
            .and_then(|idx| idx.columns.first())
            .map(|c| c.offset as usize);
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
        return Err(Error::new(
            ERR_DUP_ENTRY,
            "Duplicate entry for key 'PRIMARY'",
        ));
    }
    txn.set(key.as_bytes(), &encode_row(&col_ids, values))?;
    write_index_keys(txn, info, handle, values, physical_id)
}
