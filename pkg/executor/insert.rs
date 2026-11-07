use crate::index::write_index_keys;
use crate::join::apply::SubqueryEvalCtx;
use crate::internal::exec::Executor;
use crate::table_reader::scan_table;
use sealdb_domain::Domain;
use sealdb_errno::{
    ERR_DUP_ENTRY, ERR_NO_DB, ERR_NO_SUCH_TABLE, ERR_WARN_DATA_TRUNCATED, ERR_WRONG_VALUE_COUNT,
    Error, Result,
};
use sealdb_expression::{eval_expr_with_row, locate_partition_for_row};
use sealdb_kv::{LockWait, TxnHandle};
use sealdb_parser::ast::InsertStmt;
use sealdb_tablecodec::{encode_row, encode_row_key_with_handle};
use sealdb_types::Datum;
use sealdb_types::field_type::{FieldType, TYPE_BIT, TYPE_ENUM, TYPE_SET};
use sealdb_types::EnumValue;
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
        for values in &mut pending {
            fill_auto_increment(&self.domain, info, values)?;
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
    for (i, expr) in row.iter().enumerate() {
        // `DEFAULT` in VALUES: use the column default (NULL when none). A
        // generated column recomputes its value below.
        if is_default_keyword(expr) {
            let value = match column_at(info, columns, i) {
                Some(col) => column_default_value(col)?,
                None => Datum::Null,
            };
            values.push(value);
            continue;
        }
        values.push(subquery.eval(expr, &[], &[])?);
    }
    map_insert_row(info, columns, &values)
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

/// Evaluate a column's `DEFAULT <expr>` (NULL when it has none).
fn column_default_value(col: &sealdb_meta::ColumnInfo) -> Result<Datum> {
    let Some(text) = col.default_expr.as_deref() else {
        return Ok(Datum::Null);
    };
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
            values.push(column_default_value(col)?);
        }
    }
    fill_generated_columns(info, &mut values)?;
    Ok(values)
}

/// Assign `AUTO_INCREMENT` values (TiDB `allocAutoRandomID` / auto-id
/// fallback): `NULL`, `DEFAULT` and the literal `0` request a new id; an
/// explicit non-zero value raises the allocator so it is never reused.
fn fill_auto_increment(
    domain: &sealdb_domain::Domain,
    info: &sealdb_meta::TableInfo,
    values: &mut [Datum],
) -> Result<()> {
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
            _ => numeric_form(&current).is_some_and(|v| v == 0.0),
        };
        if needs_alloc {
            let next = domain.next_auto_id(info.id)?;
            values[idx] = cast_for_column(&Datum::I64(next), col)?;
        } else if let Some(v) = numeric_form(&current) {
            domain.bump_auto_id(info.id, v as i64)?;
        }
    }
    Ok(())
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
    if matches!(
        col.field_type.tp,
        TYPE_ENUM | TYPE_SET
    ) {
        return cast_enum_set(v, &col.field_type, col.name.o());
    }
    let mut out = v.cast(&col.field_type)?;
    // TIMESTAMP columns store UTC; session wall clock → UTC on write.
    if col.field_type.tp == TYPE_BIT {
        // BIT(n) stores the low n bits of the numeric value (MySQL truncates).
        let n = col.field_type.flen.max(1) as u32;
        let mask = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
        let bits = match &out {
            Datum::U64(v) => *v,
            Datum::I64(v) => *v as u64,
            Datum::F64(v) => *v as u64,
            Datum::Decimal(d) => d.to_f64()? as u64,
            Datum::Bytes(b) => String::from_utf8_lossy(b).parse::<u64>().unwrap_or(0),
            Datum::Time(t) => t.to_packed_u64(),
            Datum::Enum(e) => e.number,
            Datum::Null => return Ok(Datum::Null),
        };
        return Ok(Datum::U64(bits & mask));
    }
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

/// TiDB `sql_mode` strictness for DML value coercion.
fn strict_sql_mode() -> bool {
    sealdb_expression::sql_mode_flag("STRICT_TRANS_TABLES")
        || sealdb_expression::sql_mode_flag("STRICT_ALL_TABLES")
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
        // numeric strings are indexes.
        if let Datum::Bytes(_) = v {
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
    if !matches!(v, Datum::Bytes(_))
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

fn truncated_error(v: &Datum, col: &str) -> Error {
    let raw = String::from_utf8_lossy(&v.as_bytes()).into_owned();
    Error::new(
        ERR_WARN_DATA_TRUNCATED,
        format!("Data truncated for column '{col}' at row 1 (value '{raw}')"),
    )
}

fn invalid_enum_set_value(v: &Datum, col: &str, fallback: Datum) -> Result<Datum> {
    if strict_sql_mode() {
        return Err(truncated_error(v, col));
    }
    sealdb_expression::note_warning(
        ERR_WARN_DATA_TRUNCATED,
        format!("Data truncated for column '{col}' at row 1"),
    );
    Ok(fallback)
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
