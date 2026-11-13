// /ALTER TABLE ADD/DROP COLUMN|INDEX|PARTITION, CHANGE/MODIFY/RENAME,
// AFTER/FIRST, ALGORITHM/LOCK (accepted, in-place).
// Completeness wave / not this feature: generated columns, nested subpartition storage,
// multi-schema-state jobs.
use crate::create_table::{
    field_type_from_name, is_temporal_type, normalize_default, referenced_by_foreign_key,
    requote_default,
    references_identifier, ttl_column_name, validate_default_fsp, validate_default_literal,
    validate_display_width, validate_generated_auto_increment, validate_generated_column_refs,
    validate_generated_expr,
};
use crate::index::{add_index, backfill_index, drop_index_keys, remove_index};
use crate::partition::{add_partitions, drop_partitions, exchange_partition};
use crate::table::resolve_name;
use sealdb_errno::{
    ERR_BAD_DB, ERR_BAD_FIELD, ERR_CANT_DROP_FIELD_OR_KEY, ERR_COLUMN_WITH_GENERATED_DEPENDENCIES,
    ERR_DEPENDENT_BY_GENERATED_COLUMN, ERR_DEPENDENT_BY_PARTITION_FUNCTIONAL, ERR_DUP_FIELDNAME,
    ERR_DUP_KEYNAME, ERR_NO_SUCH_TABLE, ERR_TABLE_EXISTS, ERR_TTL_DROP_COLUMN,
    ERR_TTL_FK_REFERENCED, ERR_TTL_INVALID_COLUMN, ERR_TTL_NO_CONFIG,
    ERR_UNSUPPORTED_MODIFY_CHARSET, ERR_UNSUPPORTED_ON_GENERATED_COLUMN, Error, Result,
};
use sealdb_infoschema::InfoSchema;
use sealdb_kv::Storage;
use sealdb_meta::{ColumnInfo, Mutator, TableInfo};
use sealdb_parser::ast::{
    AlterTableSpec, AlterTableStmt, ColumnPosition, GeneratedColumn, TableName,
};
use sealdb_parser::model::CIStr;
use sealdb_tablecodec::{decode_row, encode_row, gen_table_record_prefix, next_prefix};
use sealdb_types::{EvalType, FieldType};

pub fn alter_table(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &AlterTableStmt,
    current_db: &str,
    storage: &dyn Storage,
) -> Result<()> {
    let mut snapshot = is.clone();
    for spec in &stmt.specs {
        match spec {
            AlterTableSpec::AddColumn {
                if_not_exists,
                column,
                position,
            } => add_column(
                meta,
                &snapshot,
                stmt,
                column,
                position,
                *if_not_exists,
                current_db,
                storage,
            )?,
            AlterTableSpec::AlterColumnDefault { name, default } => {
                alter_column_default(meta, &snapshot, stmt, name, default.as_deref(), current_db)?
            }
            AlterTableSpec::DropColumn { if_exists, name } => {
                drop_column(meta, &snapshot, stmt, name, *if_exists, current_db)?;
            }
            AlterTableSpec::AddIndex(index) => {
                add_index(
                    meta,
                    &snapshot,
                    &stmt.table,
                    index,
                    false,
                    current_db,
                    storage,
                )?;
            }
            AlterTableSpec::DropIndex { if_exists, name } => {
                remove_index(
                    meta,
                    &snapshot,
                    &stmt.table,
                    name,
                    *if_exists,
                    current_db,
                    storage,
                )?;
            }
            AlterTableSpec::ChangeColumn {
                old_name,
                new_name,
                type_name,
                default,
                generated,
                not_null,
                charset,
                collate,
                position,
            } => change_column(
                meta,
                &snapshot,
                stmt,
                old_name,
                new_name,
                Some(type_name),
                default.as_deref(),
                generated.as_ref(),
                *not_null,
                charset.as_deref(),
                collate.as_deref(),
                position,
                current_db,
                storage,
            )?,
            AlterTableSpec::ModifyColumn {
                name,
                type_name,
                default,
                generated,
                not_null,
                charset,
                collate,
                position,
            } => change_column(
                meta,
                &snapshot,
                stmt,
                name,
                name,
                Some(type_name),
                default.as_deref(),
                generated.as_ref(),
                *not_null,
                charset.as_deref(),
                collate.as_deref(),
                position,
                current_db,
                storage,
            )?,
            AlterTableSpec::RenameColumn { old_name, new_name } => change_column(
                meta,
                &snapshot,
                stmt,
                old_name,
                new_name,
                None,
                None,
                None,
                None,
                None,
                None,
                &ColumnPosition::None,
                current_db,
                storage,
            )?,
            AlterTableSpec::RenameTable { new_name } => {
                rename_table(meta, &snapshot, stmt, new_name, current_db)?;
            }
            AlterTableSpec::RenameIndex { old_name, new_name } => {
                rename_index(meta, &snapshot, stmt, old_name, new_name, current_db)?
            }
            AlterTableSpec::AlterIndexVisibility { name, invisible } => {
                alter_index_visibility(meta, &snapshot, stmt, name, *invisible, current_db)?
            }
            // ALGORITHM/LOCK and storage table options are accepted and
            // ignored; SealDB DDL is in-place and engine-independent.
            AlterTableSpec::Algorithm(_)
            | AlterTableSpec::Lock(_)
            | AlterTableSpec::TableOption { .. } => {}
            AlterTableSpec::AddPartition { defs } => {
                add_partitions(meta, &snapshot, &stmt.table, defs, current_db)?;
            }
            AlterTableSpec::ExchangePartition {
                partition,
                table: with_table,
            } => {
                exchange_partition(
                    meta,
                    &snapshot,
                    &stmt.table,
                    partition,
                    with_table,
                    current_db,
                )?;
            }
            AlterTableSpec::DropPartition { if_exists, names } => {
                drop_partitions(
                    meta,
                    &snapshot,
                    &stmt.table,
                    names,
                    *if_exists,
                    current_db,
                    storage,
                )?;
            }
            AlterTableSpec::SetTtl {
                expr,
                enable,
                job_interval,
            } => alter_ttl(
                meta,
                &snapshot,
                stmt,
                expr.as_deref(),
                *enable,
                job_interval.as_deref(),
                current_db,
            )?,
            AlterTableSpec::RemoveTtl => {
                let (schema, table) = resolve_name(&stmt.table, current_db)?;
                let db = snapshot.schema_by_name(&schema).ok_or_else(|| {
                    Error::new(ERR_BAD_DB, format!("Unknown database '{schema}'"))
                })?;
                let Some(mut info) = snapshot.table_by_name(&schema, &table).cloned() else {
                    return Err(Error::new(
                        ERR_NO_SUCH_TABLE,
                        format!("Table '{schema}.{table}' doesn't exist"),
                    ));
                };
                info.ttl = None;
                meta.update_table(db.id, &info)?;
                meta.gen_schema_version()?;
            }
            AlterTableSpec::AddForeignKey { ref_table } => {
                let (schema, table) = resolve_name(&stmt.table, current_db)?;
                let _ = table;
                if let Some(parent) = snapshot.table_by_name(&schema, ref_table)
                    && parent.ttl.is_some()
                {
                    return Err(Error::new(
                        ERR_TTL_FK_REFERENCED,
                        "Set TTL for a table referenced by foreign key is not allowed",
                    ));
                }
            }
        }
        snapshot = InfoSchema::from_meta(meta)?;
    }
    Ok(())
}

/// `ALTER TABLE … ALTER COLUMN col SET DEFAULT …` / `DROP DEFAULT`.
fn alter_column_default(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &AlterTableStmt,
    name: &str,
    default: Option<&str>,
    current_db: &str,
) -> Result<()> {
    let (schema, table) = resolve_name(&stmt.table, current_db)?;
    let db = is
        .schema_by_name(&schema)
        .ok_or_else(|| Error::new(ERR_BAD_DB, format!("Unknown database '{schema}'")))?;
    let mut info = is.table_by_name(&schema, &table).cloned().ok_or_else(|| {
        Error::new(
            ERR_NO_SUCH_TABLE,
            format!("Table '{schema}.{table}' doesn't exist"),
        )
    })?;
    let Some(pos) = info
        .columns
        .iter()
        .position(|c| c.name.o().eq_ignore_ascii_case(name))
    else {
        return Err(Error::new(
            ERR_BAD_FIELD,
            format!("Unknown column '{name}' in '{table}'"),
        ));
    };
    if let Some(default) = default {
        let field_type = info.columns[pos].field_type.clone();
        let default = normalize_default(&field_type, default);
        validate_default_literal(name, &field_type, Some(&default))?;
        validate_default_fsp(name, &field_type, Some(&default))?;
        info.columns[pos].default_expr = Some(default);
    } else {
        info.columns[pos].default_expr = None;
    }
    meta.update_table(db.id, &info)?;
    meta.gen_schema_version()?;
    Ok(())
}

/// `ALTER TABLE … TTL = …` / `TTL_ENABLE = …` / `TTL_JOB_INTERVAL = …`.
fn alter_ttl(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &AlterTableStmt,
    expr: Option<&str>,
    enable: Option<bool>,
    job_interval: Option<&str>,
    current_db: &str,
) -> Result<()> {
    let (schema, table) = resolve_name(&stmt.table, current_db)?;
    let db = is
        .schema_by_name(&schema)
        .ok_or_else(|| Error::new(ERR_BAD_DB, format!("Unknown database '{schema}'")))?;
    let mut info = is.table_by_name(&schema, &table).cloned().ok_or_else(|| {
        Error::new(
            ERR_NO_SUCH_TABLE,
            format!("Table '{schema}.{table}' doesn't exist"),
        )
    })?;
    if let Some(expr) = expr {
        crate::create_table::validate_ttl_primary_key(&info)?;
        let column = ttl_column_name(expr);
        let Some(col) = info
            .columns
            .iter()
            .find(|c| c.name.o().eq_ignore_ascii_case(&column))
        else {
            return Err(Error::new(
                ERR_BAD_FIELD,
                format!("Unknown column '{column}' in 'TTL config'"),
            ));
        };
        if !is_temporal_type(col.field_type.tp) {
            return Err(Error::new(
                ERR_TTL_INVALID_COLUMN,
                format!(
                    "Field '{column}' is of a not supported type for TTL config, \
                     expect DATETIME, DATE or TIMESTAMP"
                ),
            ));
        }
        if referenced_by_foreign_key(is, &table) {
            return Err(Error::new(
                ERR_TTL_FK_REFERENCED,
                "Set TTL for a table referenced by foreign key is not allowed",
            ));
        }
        let ttl = info.ttl.get_or_insert_with(|| sealdb_meta::TtlInfo {
            expr: expr.to_string(),
            enable: true,
            job_interval: "1h".to_string(),
        });
        ttl.expr = expr.to_string();
    }
    let Some(ttl) = info.ttl.as_mut() else {
        let name = if enable.is_some() {
            "TTL_ENABLE"
        } else {
            "TTL_JOB_INTERVAL"
        };
        return Err(Error::new(
            ERR_TTL_NO_CONFIG,
            format!("Cannot set {name} on a table without TTL config"),
        ));
    };
    if let Some(enable) = enable {
        ttl.enable = enable;
    }
    if let Some(interval) = job_interval {
        ttl.job_interval = interval.to_string();
    }
    meta.update_table(db.id, &info)?;
    meta.gen_schema_version()?;
    Ok(())
}

fn add_column(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &AlterTableStmt,
    column: &sealdb_parser::ast::ColumnDef,
    position: &ColumnPosition,
    if_not_exists: bool,
    current_db: &str,
    storage: &dyn Storage,
) -> Result<()> {
    let (schema, table) = resolve_name(&stmt.table, current_db)?;
    let db = is
        .schema_by_name(&schema)
        .ok_or_else(|| Error::new(ERR_BAD_DB, format!("Unknown database '{schema}'")))?;
    let mut info = is.table_by_name(&schema, &table).cloned().ok_or_else(|| {
        Error::new(
            ERR_NO_SUCH_TABLE,
            format!("Table '{schema}.{table}' doesn't exist"),
        )
    })?;
    if info
        .columns
        .iter()
        .any(|c| c.name.o().eq_ignore_ascii_case(&column.name))
    {
        if if_not_exists {
            return Ok(());
        }
        return Err(Error::new(
            ERR_DUP_FIELDNAME,
            format!("Duplicate column name '{}'", column.name),
        ));
    }
    info.max_col_id += 1;
    let insert_at = column_insert_index(&info, position)?;
    let mut new_col = ColumnInfo::new(
        info.max_col_id,
        insert_at as i32,
        column.name.clone(),
        field_type_from_name(&column.type_name)?,
    );
    // Normalize a literal default to the column type (TiDB rounds temporal
    // defaults to the column's fractional precision at DDL time).
    new_col.default_expr = crate::create_table::normalize_lob_default(
        &column.name,
        &new_col.field_type,
        column
            .default
            .as_deref()
            .map(|default| normalize_default(&new_col.field_type, default)),
    )?;
    new_col.auto_random = column.auto_random;
    if let Some(generated) = &column.generated {
        validate_generated_expr(&column.name, &generated.expr)?;
        if generated.stored {
            return Err(unsupported_generated(
                "Adding generated stored column through ALTER TABLE",
            ));
        }
        new_col.generated_expr = Some(generated.expr.clone());
        new_col.generated_stored = generated.stored;
    }
    if column.not_null {
        new_col.field_type.flag |= sealdb_types::field_type::NOT_NULL_FLAG;
    }
    validate_display_width(&new_col.name.o().to_string(), &new_col.field_type)?;
    validate_default_literal(
        &new_col.name.o().to_string(),
        &new_col.field_type,
        new_col.default_expr.as_deref(),
    )?;
    validate_default_fsp(
        &new_col.name.o().to_string(),
        &new_col.field_type,
        new_col.default_expr.as_deref(),
    )?;
    let new_id = new_col.id;
    let default_value = column_default_datum(&new_col)?;
    // Existing rows were written before this column existed, so decode them
    // with the pre-ALTER layout (offsets in `info` already include the new
    // column and would mis-map the stored values).
    let old_types = info.col_field_types();
    info.columns.insert(insert_at, new_col);
    recompute_column_offsets(&mut info);
    validate_generated_auto_increment(&info.columns, Some(&column.name))?;
    validate_generated_column_refs(&info.columns, Some(&column.name))?;
    meta.update_table(db.id, &info)?;
    meta.gen_schema_version()?;
    if let Some(value) = default_value {
        backfill_added_column(storage, &info, &old_types, new_id, value)?;
    }
    Ok(())
}

/// Evaluate a `DEFAULT <expr>` body for a freshly added column.
fn column_default_datum(col: &ColumnInfo) -> Result<Option<sealdb_types::Datum>> {
    let Some(text) = col.default_expr.as_deref() else {
        return Ok(None);
    };
    let expr = sealdb_parser::sqlparser_bridge::lower::parse_expr(text)?;
    let value = sealdb_expression::eval_expr_with_row(&expr, &[], &[])?;
    Ok(Some(value.cast(&col.field_type)?))
}

/// `ALTER TABLE … ADD COLUMN … DEFAULT x` fills existing rows with `x`
/// (TiDB/MySQL expose the default for rows written before the column existed).
fn backfill_added_column(
    storage: &dyn Storage,
    info: &TableInfo,
    types: &[(i64, sealdb_types::FieldType)],
    col_id: i64,
    value: sealdb_types::Datum,
) -> Result<()> {
    let mut txn = storage.begin()?;
    for physical_id in info.physical_ids() {
        let start = gen_table_record_prefix(physical_id);
        let end = next_prefix(start.as_bytes());
        let pairs = txn.scan(start.as_bytes(), &end)?;
        for (key, raw) in pairs {
            let decoded = decode_row(&raw, types);
            if decoded.iter().any(|(id, _)| *id == col_id) {
                continue;
            }
            let mut col_ids: Vec<i64> = decoded.iter().map(|(id, _)| *id).collect();
            let mut values: Vec<sealdb_types::Datum> =
                decoded.into_iter().map(|(_, v)| v).collect();
            col_ids.push(col_id);
            values.push(value.clone());
            txn.set(&key, &encode_row(&col_ids, &values))?;
        }
    }
    txn.commit()
}

/// Backfill newly added expression-index hidden columns.
///
/// `types` is the layout of rows already on disk (without the new columns);
/// every missing column is evaluated from the decoded row and appended.
pub(crate) fn backfill_generated_columns(
    storage: &dyn Storage,
    info: &TableInfo,
    types: &[(i64, FieldType)],
    new_col_ids: &[i64],
) -> Result<()> {
    let mut pending = Vec::with_capacity(new_col_ids.len());
    for id in new_col_ids {
        let col = info
            .columns
            .iter()
            .find(|c| c.id == *id)
            .ok_or_else(|| Error::new(ERR_BAD_FIELD, format!("unknown column id {id}")))?;
        let Some(text) = col.generated_expr.as_deref() else {
            continue;
        };
        let expr = sealdb_parser::sqlparser_bridge::lower::parse_expr(text)?;
        pending.push((*id, col, expr));
    }
    if pending.is_empty() {
        return Ok(());
    }
    let schema: Vec<String> = info
        .columns
        .iter()
        .map(|c| c.name.o().to_string())
        .collect();
    let offset_of = |id: i64| {
        info.columns
            .iter()
            .position(|c| c.id == id)
            .unwrap_or(usize::MAX)
    };
    let mut txn = storage.begin()?;
    for physical_id in info.physical_ids() {
        let start = gen_table_record_prefix(physical_id);
        let end = next_prefix(start.as_bytes());
        let pairs = txn.scan(start.as_bytes(), &end)?;
        for (key, raw) in pairs {
            let decoded = decode_row(&raw, types);
            let present = |id: i64| decoded.iter().any(|(d, _)| *d == id);
            if pending.iter().all(|(id, _, _)| present(*id)) {
                continue;
            }
            // Full-width row values in column-offset order, so a generated
            // expression sees the same layout it sees on INSERT.
            let mut values = vec![sealdb_types::Datum::Null; info.columns.len()];
            for (id, v) in &decoded {
                let offset = offset_of(*id);
                if offset < values.len() {
                    values[offset] = v.clone();
                }
            }
            let mut col_ids: Vec<i64> = decoded.iter().map(|(id, _)| *id).collect();
            let mut out: Vec<sealdb_types::Datum> = decoded.into_iter().map(|(_, v)| v).collect();
            for (id, col, expr) in &pending {
                if col_ids.contains(id) {
                    continue;
                }
                let value = sealdb_expression::eval_expr_with_row(expr, &schema, &values)?;
                let value = value.cast(&col.field_type)?;
                values[offset_of(*id)] = value.clone();
                col_ids.push(*id);
                out.push(value);
            }
            txn.set(&key, &encode_row(&col_ids, &out))?;
        }
    }
    txn.commit()
}

/// Recompute the materialised values of every generated column after one of
/// their expressions changed. TiDB reads VIRTUAL columns lazily; this engine
/// stores them, so the rows must be rewritten (dependent generated columns are
/// evaluated in offset order and see the new value).
fn recompute_generated_rows(storage: &dyn Storage, info: &TableInfo) -> Result<()> {
    let mut pending = Vec::new();
    for col in &info.columns {
        let Some(text) = col.generated_expr.as_deref() else {
            continue;
        };
        let expr = sealdb_parser::sqlparser_bridge::lower::parse_expr(text)?;
        pending.push((col.offset as usize, col, expr));
    }
    if pending.is_empty() {
        return Ok(());
    }
    pending.sort_by_key(|(offset, ..)| *offset);
    let schema: Vec<String> = info
        .columns
        .iter()
        .map(|c| c.name.o().to_string())
        .collect();
    let types = info.col_field_types();
    let mut txn = storage.begin()?;
    for physical_id in info.physical_ids() {
        let start = gen_table_record_prefix(physical_id);
        let end = next_prefix(start.as_bytes());
        let pairs = txn.scan(start.as_bytes(), &end)?;
        for (key, raw) in pairs {
            let decoded = decode_row(&raw, &types);
            let mut values = vec![sealdb_types::Datum::Null; info.columns.len()];
            for (id, v) in &decoded {
                if let Some(pos) = info.columns.iter().position(|c| c.id == *id) {
                    values[pos] = v.clone();
                }
            }
            for (offset, col, expr) in &pending {
                if *offset >= values.len() {
                    continue;
                }
                let value = sealdb_expression::eval_expr_with_row(expr, &schema, &values)?;
                values[*offset] = value.cast(&col.field_type)?;
            }
            let col_ids: Vec<i64> = info.columns.iter().map(|c| c.id).collect();
            txn.set(&key, &encode_row(&col_ids, &values))?;
        }
    }
    txn.commit()
}

/// Whether the table's partitioning expression uses `name` (TiDB refuses to
/// drop or rename such a column: 3855).
fn used_by_partition_function(info: &TableInfo, name: &str) -> bool {
    let Some(part) = &info.partition else {
        return false;
    };
    if part
        .columns
        .iter()
        .any(|c| c.o().eq_ignore_ascii_case(name))
    {
        return true;
    }
    !part.expr.is_empty() && references_identifier(&part.expr, name)
}

/// Whether a generated column depends on `name`.
fn has_dependent_generated_column(info: &TableInfo, name: &str) -> bool {
    info.columns.iter().any(|col| {
        col.generated_expr
            .as_deref()
            .is_some_and(|expr| references_identifier(expr, name))
    })
}

fn partition_column_dependency(name: &str) -> Error {
    Error::new(
        ERR_DEPENDENT_BY_PARTITION_FUNCTIONAL,
        format!(
            "Column '{name}' has a partitioning function dependency and cannot be dropped or renamed"
        ),
    )
}

fn generated_column_dependency(name: &str) -> Error {
    Error::new(
        ERR_DEPENDENT_BY_GENERATED_COLUMN,
        format!("Column '{name}' has a generated column dependency."),
    )
}

/// TiDB `ErrUnsupportedOnGeneratedColumn` with the reason TiDB prints, e.g.
/// `'Changing the STORED status' is not supported for generated columns.`
pub(crate) fn unsupported_generated(reason: &str) -> Error {
    Error::new(
        ERR_UNSUPPORTED_ON_GENERATED_COLUMN,
        format!("'{reason}' is not supported for generated columns."),
    )
}

/// Do two generated-column expressions denote the same computation? TiDB
/// compares the restored expression text, so `(a+1)` and `(a + 1)` are equal
/// while `` (`a` + 1) `` and `(a + 2)` are not.
fn same_generated_expr(left: &str, right: &str) -> bool {
    fn canonical(expr: &str) -> String {
        expr.chars()
            .filter(|c| !c.is_whitespace() && *c != '`')
            .flat_map(char::to_lowercase)
            .collect()
    }
    canonical(left) == canonical(right)
}

/// Is `name` part of any index key?
fn is_indexed_column(info: &TableInfo, name: &str) -> bool {
    info.indices.iter().any(|idx| {
        idx.columns
            .iter()
            .any(|c| c.name.o().eq_ignore_ascii_case(name))
    })
}

fn drop_column(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &AlterTableStmt,
    name: &str,
    if_exists: bool,
    current_db: &str,
) -> Result<()> {
    let (schema, table) = resolve_name(&stmt.table, current_db)?;
    let db = is
        .schema_by_name(&schema)
        .ok_or_else(|| Error::new(ERR_BAD_DB, format!("Unknown database '{schema}'")))?;
    let mut info = is.table_by_name(&schema, &table).cloned().ok_or_else(|| {
        Error::new(
            ERR_NO_SUCH_TABLE,
            format!("Table '{schema}.{table}' doesn't exist"),
        )
    })?;
    let Some(pos) = info
        .columns
        .iter()
        .position(|c| c.name.o().eq_ignore_ascii_case(name))
    else {
        if if_exists {
            return Ok(());
        }
        return Err(Error::new(
            ERR_CANT_DROP_FIELD_OR_KEY,
            format!("Can't DROP '{name}'; check that column/key exists"),
        ));
    };
    if used_by_partition_function(&info, name) {
        return Err(partition_column_dependency(name));
    }
    if has_dependent_generated_column(&info, name) {
        return Err(generated_column_dependency(name));
    }
    if info.pk_is_handle
        && info
            .handle_column()
            .is_some_and(|handle| handle.id == info.columns[pos].id)
    {
        return Err(Error::unsupported("DROP handle column"));
    }
    if info.ttl.as_ref().is_some_and(|ttl| {
        crate::create_table::ttl_column_name(&ttl.expr).eq_ignore_ascii_case(name)
    }) {
        return Err(Error::new(
            ERR_TTL_DROP_COLUMN,
            format!("Cannot drop column '{name}': needed in TTL config"),
        ));
    }
    if info.indices.iter().any(|idx| {
        idx.columns
            .iter()
            .any(|c| c.name.o().eq_ignore_ascii_case(name))
    }) {
        return Err(Error::unsupported("DROP COLUMN used by INDEX"));
    }
    if info.columns.len() == 1 {
        return Err(Error::unsupported("DROP last column"));
    }
    info.columns.remove(pos);
    recompute_column_offsets(&mut info);
    meta.update_table(db.id, &info)?;
    meta.gen_schema_version()?;
    Ok(())
}

/// CHANGE / MODIFY / RENAME COLUMN: update the name and rewrite stored
/// values when the FieldType actually changes.
#[allow(clippy::too_many_arguments)]
/// Moving a column must not place a generated column before a column it
/// depends on, or leave a dependent generated column before it (3107).
fn validate_generated_order_after_move(
    info: &TableInfo,
    from: usize,
    position: &ColumnPosition,
) -> Result<()> {
    let mut names: Vec<String> = info
        .columns
        .iter()
        .map(|c| c.name.o().to_string())
        .collect();
    let moved = names.remove(from);
    let to = match position {
        ColumnPosition::First => 0,
        ColumnPosition::After(target) => names
            .iter()
            .position(|n| n.eq_ignore_ascii_case(target))
            .map(|i| i + 1)
            .unwrap_or(names.len()),
        ColumnPosition::None => names.len(),
    };
    let to = to.min(names.len());
    names.insert(to, moved);
    check_generated_order(&names, &info.columns)
}

/// Every generated column may only reference columns defined before it (3107).
pub(crate) fn check_generated_order(names: &[String], columns: &[ColumnInfo]) -> Result<()> {
    let position = |name: &str| names.iter().position(|n| n.eq_ignore_ascii_case(name));
    for col in columns {
        let Some(text) = col.generated_expr.as_deref() else {
            continue;
        };
        let Some(col_pos) = position(col.name.o()) else {
            continue;
        };
        let expr = sealdb_parser::sqlparser_bridge::lower::parse_expr(text)?;
        for (_table, dep) in expr.referenced_columns() {
            // Only references to *generated* columns defined later are refused;
            // a base column may appear after the generated one (admin.check).
            let dep_generated = columns.iter().any(|c| {
                c.name.o().eq_ignore_ascii_case(&dep) && c.generated_expr.is_some()
            });
            if let Some(dep_pos) = position(&dep)
                && dep_pos > col_pos
                && dep_generated
            {
                return Err(Error::new(
                    ERR_COLUMN_WITH_GENERATED_DEPENDENCIES,
                    "Generated column can refer only to generated columns defined prior to it."
                        .to_string(),
                ));
            }
        }
    }
    Ok(())
}

fn change_column(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &AlterTableStmt,
    old_name: &str,
    new_name: &str,
    type_name: Option<&str>,
    default: Option<&str>,
    generated: Option<&GeneratedColumn>,
    not_null: Option<bool>,
    charset: Option<&str>,
    collate: Option<&str>,
    position: &ColumnPosition,
    current_db: &str,
    storage: &dyn Storage,
) -> Result<()> {
    let (schema, table) = resolve_name(&stmt.table, current_db)?;
    let db = is
        .schema_by_name(&schema)
        .ok_or_else(|| Error::new(ERR_BAD_DB, format!("Unknown database '{schema}'")))?;
    let mut info = is.table_by_name(&schema, &table).cloned().ok_or_else(|| {
        Error::new(
            ERR_NO_SUCH_TABLE,
            format!("Table '{schema}.{table}' doesn't exist"),
        )
    })?;
    let Some(pos) = info
        .columns
        .iter()
        .position(|c| c.name.o().eq_ignore_ascii_case(old_name))
    else {
        return Err(Error::new(
            ERR_BAD_FIELD,
            format!("Unknown column '{old_name}' in '{table}'"),
        ));
    };
    if !old_name.eq_ignore_ascii_case(new_name)
        && info
            .columns
            .iter()
            .any(|c| c.name.o().eq_ignore_ascii_case(new_name))
    {
        return Err(Error::new(
            ERR_DUP_FIELDNAME,
            format!("Duplicate column name '{new_name}'"),
        ));
    }
    let mut new_ft = match type_name {
        Some(name) => field_type_from_name(name)?,
        None => info.columns[pos].field_type.clone(),
    };
    match not_null {
        Some(true) => new_ft.flag |= sealdb_types::field_type::NOT_NULL_FLAG,
        Some(false) => new_ft.flag &= !sealdb_types::field_type::NOT_NULL_FLAG,
        None => {}
    }
    // TiDB `ErrUnsupportedModifyCharset` (8200): a column's charset is fixed
    // once created, so `MODIFY … CHARACTER SET x` on a different charset fails.
    let old_charset = info.columns[pos].field_type.charset.clone();
    if let Some(charset) = charset
        && !charset.eq_ignore_ascii_case(&old_charset)
    {
        return Err(Error::new(
            ERR_UNSUPPORTED_MODIFY_CHARSET,
            format!("Unsupported modify charset from {old_charset} to {charset}"),
        ));
    }
    if let Some(charset) = charset {
        new_ft.charset = charset.to_string();
    }
    if let Some(collate) = collate {
        new_ft.collate = collate.to_string();
    }
    let renaming = !old_name.eq_ignore_ascii_case(new_name);
    if renaming && used_by_partition_function(&info, old_name) {
        return Err(partition_column_dependency(old_name));
    }
    if renaming && has_dependent_generated_column(&info, old_name) {
        return Err(generated_column_dependency(old_name));
    }
    if info
        .ttl
        .as_ref()
        .is_some_and(|ttl| ttl_column_name(&ttl.expr).eq_ignore_ascii_case(old_name))
    {
        if !is_temporal_type(new_ft.tp) {
            return Err(Error::new(
                ERR_TTL_INVALID_COLUMN,
                format!(
                    "Field '{new_name}' is of a not supported type for TTL config, \
                     expect DATETIME, DATE or TIMESTAMP"
                ),
            ));
        }
        if !old_name.eq_ignore_ascii_case(new_name)
            && let Some(ttl) = info.ttl.as_mut()
        {
            ttl.expr = ttl
                .expr
                .replace(&format!("`{old_name}`"), &format!("`{new_name}`"));
        }
    }
    if info.pk_is_handle
        && info
            .handle_column()
            .is_some_and(|handle| handle.id == info.columns[pos].id)
        && new_ft.eval_type() != EvalType::Int
    {
        return Err(Error::unsupported("MODIFY handle column type"));
    }
    let changed = field_type_changed(&info.columns[pos].field_type, &new_ft);
    // TiDB `getModifiableColumnJob` refuses most reshapes of a generated
    // column with 3106 and a reason-specific message.
    let old_generated = info.columns[pos].generated_expr.is_some();
    let old_stored = info.columns[pos].generated_stored;
    // MODIFY / CHANGE always re-specify the type, so a missing `AS (...)` means
    // the column becomes a plain one; RENAME COLUMN leaves it untouched.
    let respecifies_column = type_name.is_some();
    let mut generated_expr_changed = false;
    if old_generated && respecifies_column {
        match generated {
            None => {
                // Turning a stored generated column plain is allowed (the
                // materialised values are kept); a virtual one is not.
                if !old_stored {
                    return Err(unsupported_generated("Changing the STORED status"));
                }
            }
            Some(new_generated) => {
                if new_generated.stored != old_stored {
                    return Err(unsupported_generated("Changing the STORED status"));
                }
                let old_expr = info.columns[pos]
                    .generated_expr
                    .as_deref()
                    .unwrap_or_default();
                let expr_changed = !same_generated_expr(old_expr, &new_generated.expr);
                if expr_changed && old_stored {
                    return Err(unsupported_generated("modifying a stored column"));
                }
                if expr_changed && is_indexed_column(&info, old_name) {
                    return Err(unsupported_generated("modifying an indexed column"));
                }
                generated_expr_changed = expr_changed;
            }
        }
    } else if !old_generated && generated.is_some_and(|g| g.stored) {
        return Err(unsupported_generated("modifying a stored column"));
    }
    if changed && has_dependent_generated_column(&info, old_name) {
        // TiDB runs the MODIFY, hits the 3108 dependency check and reports it
        // wrapped in 3106 (`[ddl:3108]` keeps the inner error's code).
        return Err(Error::new(
            ERR_UNSUPPORTED_ON_GENERATED_COLUMN,
            format!(
                "'[ddl:3108]Column '{old_name}' has a generated column dependency.' is not supported for generated columns."
            ),
        ));
    }
    let effective_default = crate::create_table::normalize_lob_default(
        new_name,
        &new_ft,
        default
            .map(|default| default.to_string())
            .or_else(|| info.columns[pos].default_expr.clone())
            .map(|default| normalize_default(&new_ft, &default)),
    )?;
    validate_display_width(new_name, &new_ft)?;
    validate_default_literal(new_name, &new_ft, effective_default.as_deref())?;
    validate_default_fsp(new_name, &new_ft, effective_default.as_deref())?;
    if let Some(default) = effective_default {
        info.columns[pos].default_expr = Some(default);
    }
    if generated.is_some() || respecifies_column {
        match generated {
            Some(generated) => {
                validate_generated_expr(new_name, &generated.expr)?;
                info.columns[pos].generated_expr = Some(generated.expr.clone());
                info.columns[pos].generated_stored = generated.stored;
                validate_generated_auto_increment(&info.columns, Some(new_name))?;
                validate_generated_column_refs(&info.columns, Some(new_name))?;
                // A rewritten expression may only look backwards in the
                // column list (3107).
                let names: Vec<String> = info
                    .columns
                    .iter()
                    .map(|c| c.name.o().to_string())
                    .collect();
                check_generated_order(&names, &info.columns)?;
            }
            None => {
                info.columns[pos].generated_expr = None;
                info.columns[pos].generated_stored = false;
            }
        }
    }
    // A freshly added NOT NULL needs a reorg just like a type change: TiDB
    // fails the job on the first NULL it meets (1265).
    let not_null_added = new_ft.flag & sealdb_types::field_type::NOT_NULL_FLAG != 0
        && info.columns[pos].field_type.flag & sealdb_types::field_type::NOT_NULL_FLAG == 0;
    if changed || not_null_added {
        rewrite_column_rows(
            storage,
            &info,
            info.columns[pos].id,
            new_name,
            &new_ft,
        )?;
    }
    let previous = info.columns[pos].name.o().to_string();
    info.columns[pos].name = CIStr::new(new_name);
    info.columns[pos].field_type = new_ft;
    if generated_expr_changed {
        recompute_generated_rows(storage, &info)?;
    }
    for idx in &mut info.indices {
        for ic in &mut idx.columns {
            if ic.name.o().eq_ignore_ascii_case(&previous) {
                ic.name = CIStr::new(new_name);
            }
        }
    }
    if changed {
        let affected: Vec<i64> = info
            .indices
            .iter()
            .filter(|idx| {
                idx.columns
                    .iter()
                    .any(|c| c.name.o().eq_ignore_ascii_case(new_name))
            })
            .map(|idx| idx.id)
            .collect();
        for idx_id in affected {
            drop_index_keys(storage, &info, idx_id)?;
            if let Some(idx) = info.indices.iter().find(|idx| idx.id == idx_id).cloned() {
                backfill_index(storage, &info, &idx)?;
            }
        }
    }
    if needs_column_move(position, new_name) {
        validate_generated_order_after_move(&info, pos, position)?;
        let col = info.columns.remove(pos);
        let insert_at = column_insert_index(&info, position)?;
        info.columns.insert(insert_at, col);
        recompute_column_offsets(&mut info);
    }
    // The rebuild re-evaluates literal defaults for the whole table.
    for col in &mut info.columns {
        if let Some(default) = col.default_expr.clone() {
            col.default_expr = Some(requote_default(&col.field_type, &default));
        }
    }
    meta.update_table(db.id, &info)?;
    meta.gen_schema_version()?;
    Ok(())
}

fn field_type_changed(old: &FieldType, new: &FieldType) -> bool {
    old.tp != new.tp
        || old.unsigned != new.unsigned
        || old.decimal != new.decimal
        || (new.flen >= 0 && old.flen >= 0 && new.flen < old.flen)
}

fn rewrite_column_rows(
    storage: &dyn Storage,
    info: &TableInfo,
    col_id: i64,
    col_name: &str,
    new_ft: &FieldType,
) -> Result<()> {
    let old_types = info.col_field_types();
    let mut txn = storage.begin()?;
    let mut reorg_warnings: Vec<(i32, String)> = Vec::new();
    let mut row_no = 0usize;
    for physical_id in info.physical_ids() {
        let start = gen_table_record_prefix(physical_id);
        let end = next_prefix(start.as_bytes());
        let pairs = txn.scan(start.as_bytes(), &end)?;
        for (key, raw) in pairs {
            row_no += 1;
            let decoded = decode_row(&raw, &old_types);
            let mut col_ids = Vec::with_capacity(info.columns.len());
            let mut values = Vec::with_capacity(info.columns.len());
            for col in &info.columns {
                let mut value = decoded
                    .iter()
                    .find(|(id, _)| *id == col.id)
                    .map(|(_, v)| v.clone())
                    .unwrap_or(sealdb_types::Datum::Null);
                if col.id == col_id {
                    // A NULL left in a column that just became NOT NULL aborts
                    // the reorg job (TiDB reports 1265 for the first row).
                    if new_ft.flag & sealdb_types::field_type::NOT_NULL_FLAG != 0
                        && value.is_null()
                    {
                        return Err(Error::new(
                            sealdb_errno::ERR_WARN_DATA_TRUNCATED,
                            format!("Data truncated for column '{col_name}' at row {row_no}"),
                        ));
                    }
                    value = convert_column_value(&value, new_ft, &mut reorg_warnings)?;
                }
                col_ids.push(col.id);
                values.push(value);
            }
            txn.set(&key, &encode_row(&col_ids, &values))?;
        }
    }
    txn.commit()?;
    push_reorg_warnings(&reorg_warnings);
    Ok(())
}

/// TiDB collects reorg warnings per error code and hands them to the statement
/// once the job finishes: one occurrence is reported verbatim, several collapse
/// into `<n> warnings with this error code, first warning: <first>`.
fn push_reorg_warnings(warnings: &[(i32, String)]) {
    let mut order: Vec<i32> = Vec::new();
    let mut counts: std::collections::HashMap<i32, (usize, &str)> =
        std::collections::HashMap::new();
    for (code, message) in warnings {
        let entry = counts.entry(*code).or_insert_with(|| {
            order.push(*code);
            (0, message.as_str())
        });
        entry.0 += 1;
    }
    for code in order {
        let (count, first) = counts[&code];
        let message = if count == 1 {
            first.to_string()
        } else {
            format!("{count} warnings with this error code, first warning: {first}")
        };
        sealdb_expression::note_warning(code, message);
    }
}

/// Integer bounds enforced when a `MODIFY`/`CHANGE` narrows the column type.
fn integer_bounds(ft: &FieldType) -> Option<(i128, i128)> {
    let (min, max) = match ft.tp {
        sealdb_types::field_type::TYPE_TINY => (-128i128, 127i128),
        sealdb_types::field_type::TYPE_SHORT => (-32_768, 32_767),
        sealdb_types::field_type::TYPE_INT24 => (-8_388_608, 8_388_607),
        sealdb_types::field_type::TYPE_LONG => {
            (i64::from(i32::MIN) as i128, i64::from(i32::MAX) as i128)
        }
        sealdb_types::field_type::TYPE_LONG_LONG => (i64::MIN as i128, i64::MAX as i128),
        _ => return None,
    };
    if ft.is_unsigned() {
        Some((0, max * 2 + 1))
    } else {
        Some((min, max))
    }
}

fn convert_column_value(
    value: &sealdb_types::Datum,
    ft: &FieldType,
    reorg_warnings: &mut Vec<(i32, String)>,
) -> Result<sealdb_types::Datum> {
    let mut out = value.cast(ft)?;
    // Non-strict reorg clamps an out-of-range constant and records a 1690
    // warning instead of failing the `ALTER`.
    if let Some((min, max)) = integer_bounds(ft) {
        let observed = match &out {
            sealdb_types::Datum::I64(v) => Some(*v as i128),
            sealdb_types::Datum::U64(v) => Some(*v as i128),
            _ => None,
        };
        if let Some(observed) = observed
            && (observed < min || observed > max)
        {
            reorg_warnings.push((
                sealdb_errno::ERR_DATA_OUT_OF_RANGE,
                format!("constant {observed} overflows {}", ft.type_name()),
            ));
            let clamped = observed.clamp(min, max);
            out = if min >= 0 {
                sealdb_types::Datum::U64(clamped as u64)
            } else {
                sealdb_types::Datum::I64(clamped as i64)
            };
        }
    }
    if ft.eval_type() == EvalType::String
        && ft.flen > 0
        && let sealdb_types::Datum::Bytes(bytes) = &mut out
    {
        let max = ft.flen as usize;
        if bytes.len() > max {
            bytes.truncate(max);
        }
    }
    Ok(out)
}

fn rename_table(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &AlterTableStmt,
    new_name: &TableName,
    current_db: &str,
) -> Result<()> {
    let (schema, table) = resolve_name(&stmt.table, current_db)?;
    let db = is
        .schema_by_name(&schema)
        .ok_or_else(|| Error::new(ERR_BAD_DB, format!("Unknown database '{schema}'")))?;
    let mut info = is.table_by_name(&schema, &table).cloned().ok_or_else(|| {
        Error::new(
            ERR_NO_SUCH_TABLE,
            format!("Table '{schema}.{table}' doesn't exist"),
        )
    })?;
    let (new_schema, new_table) = resolve_name(new_name, &schema)?;
    if schema.eq_ignore_ascii_case(&new_schema) && table.eq_ignore_ascii_case(&new_table) {
        return Ok(());
    }
    if is.table_by_name(&new_schema, &new_table).is_some() {
        return Err(Error::new(
            ERR_TABLE_EXISTS,
            format!("Table '{new_schema}.{new_table}' already exists"),
        ));
    }
    let new_db = is
        .schema_by_name(&new_schema)
        .ok_or_else(|| Error::new(ERR_BAD_DB, format!("Unknown database '{new_schema}'")))?;
    info.name = CIStr::new(new_table);
    for idx in &mut info.indices {
        idx.table = info.name.clone();
    }
    if db.id == new_db.id {
        meta.update_table(db.id, &info)?;
    } else {
        meta.drop_table_or_view(db.id, info.id)?;
        meta.create_table_or_view(new_db.id, &info)?;
    }
    meta.gen_schema_version()?;
    Ok(())
}

fn rename_index(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &AlterTableStmt,
    old_name: &str,
    new_name: &str,
    current_db: &str,
) -> Result<()> {
    let (schema, table) = resolve_name(&stmt.table, current_db)?;
    let db = is
        .schema_by_name(&schema)
        .ok_or_else(|| Error::new(ERR_BAD_DB, format!("Unknown database '{schema}'")))?;
    let mut info = is.table_by_name(&schema, &table).cloned().ok_or_else(|| {
        Error::new(
            ERR_NO_SUCH_TABLE,
            format!("Table '{schema}.{table}' doesn't exist"),
        )
    })?;
    let Some(pos) = info
        .indices
        .iter()
        .position(|idx| idx.name.o().eq_ignore_ascii_case(old_name))
    else {
        return Err(Error::new(
            ERR_CANT_DROP_FIELD_OR_KEY,
            format!("Can't DROP '{old_name}'; check that column/key exists"),
        ));
    };
    if !old_name.eq_ignore_ascii_case(new_name)
        && info
            .indices
            .iter()
            .any(|idx| idx.name.o().eq_ignore_ascii_case(new_name))
    {
        return Err(Error::new(
            ERR_DUP_KEYNAME,
            format!("Duplicate key name '{new_name}'"),
        ));
    }
    info.indices[pos].name = CIStr::new(new_name);
    meta.update_table(db.id, &info)?;
    meta.gen_schema_version()?;
    Ok(())
}

fn alter_index_visibility(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &AlterTableStmt,
    name: &str,
    invisible: bool,
    current_db: &str,
) -> Result<()> {
    let (schema, table) = resolve_name(&stmt.table, current_db)?;
    let db = is
        .schema_by_name(&schema)
        .ok_or_else(|| Error::new(ERR_BAD_DB, format!("Unknown database '{schema}'")))?;
    let mut info = is.table_by_name(&schema, &table).cloned().ok_or_else(|| {
        Error::new(
            ERR_NO_SUCH_TABLE,
            format!("Table '{schema}.{table}' doesn't exist"),
        )
    })?;
    let Some(idx) = info
        .indices
        .iter_mut()
        .find(|idx| idx.name.o().eq_ignore_ascii_case(name))
    else {
        return Err(Error::new(
            ERR_CANT_DROP_FIELD_OR_KEY,
            format!("Can't DROP '{name}'; check that column/key exists"),
        ));
    };
    idx.invisible = invisible;
    meta.update_table(db.id, &info)?;
    meta.gen_schema_version()?;
    Ok(())
}

fn column_insert_index(info: &TableInfo, position: &ColumnPosition) -> Result<usize> {
    match position {
        ColumnPosition::None => Ok(info.columns.len()),
        ColumnPosition::First => Ok(0),
        ColumnPosition::After(name) => {
            let pos = info
                .columns
                .iter()
                .position(|c| c.name.o().eq_ignore_ascii_case(name))
                .ok_or_else(|| {
                    Error::new(ERR_BAD_FIELD, format!("Unknown column '{name}' in 'AFTER'"))
                })?;
            Ok(pos + 1)
        }
    }
}

fn needs_column_move(position: &ColumnPosition, col_name: &str) -> bool {
    match position {
        ColumnPosition::None => false,
        ColumnPosition::First => true,
        ColumnPosition::After(name) => !name.eq_ignore_ascii_case(col_name),
    }
}

fn recompute_column_offsets(info: &mut TableInfo) {
    for (offset, col) in info.columns.iter_mut().enumerate() {
        col.offset = offset as i32;
    }
    for idx in &mut info.indices {
        for ic in &mut idx.columns {
            if let Some(col) = info
                .columns
                .iter()
                .find(|c| c.name.o().eq_ignore_ascii_case(ic.name.o()))
            {
                ic.offset = col.offset;
            }
        }
    }
}
