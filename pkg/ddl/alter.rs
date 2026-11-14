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
    ERR_BAD_DB, ERR_BAD_FIELD, ERR_BLOB_USED_AS_KEY, ERR_CANT_DROP_FIELD_OR_KEY,
    ERR_COLUMN_WITH_GENERATED_DEPENDENCIES,
    ERR_DEPENDENT_BY_GENERATED_COLUMN, ERR_DEPENDENT_BY_PARTITION_FUNCTIONAL, ERR_DUP_FIELDNAME,
    ERR_DUP_KEYNAME, ERR_FK_INCOMPATIBLE_COLUMNS, ERR_NO_SUCH_TABLE,
    ERR_PRIMARY_CANT_HAVE_NULL, ERR_TABLE_EXISTS, ERR_TTL_DROP_COLUMN,
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
use sealdb_types::field_type::{TYPE_ENUM, TYPE_SET};
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
                comment,
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
                comment.as_deref(),
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
                comment,
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
                comment.as_deref(),
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

/// TiDB `checkTypeChangeSupported` in `pkg/types/field_type.go`: the
/// conversions that are still unimplemented (and therefore report 8200).
fn check_type_change_supported(old_ft: &FieldType, new_ft: &FieldType) -> bool {
    use sealdb_types::field_type as ft;
    let time = |tp: u8| matches!(tp, ft::TYPE_DATE | ft::TYPE_DATETIME | ft::TYPE_TIMESTAMP);
    let stringy = |tp: u8| {
        matches!(
            tp,
            ft::TYPE_STRING
                | ft::TYPE_VARCHAR
                | ft::TYPE_VAR_STRING
                | ft::TYPE_BLOB
                | ft::TYPE_TINY_BLOB
                | ft::TYPE_MEDIUM_BLOB
                | ft::TYPE_LONG_BLOB
        )
    };
    let decimalish = |tp: u8| {
        matches!(
            tp,
            ft::TYPE_NEW_DECIMAL | ft::TYPE_FLOAT | ft::TYPE_DOUBLE
        )
    };
    let old = old_ft.tp;
    let new = new_ft.tp;
    // date/datetime/timestamp/time/year/string/json -> bit
    if (time(old)
        || old == ft::TYPE_DURATION
        || old == ft::TYPE_YEAR
        || stringy(old)
        || old == ft::TYPE_JSON)
        && new == ft::TYPE_BIT
    {
        return false;
    }
    // date/datetime/timestamp/time/year/decimal/float/double/json/bit -> enum/set
    if (time(old)
        || old == ft::TYPE_DURATION
        || old == ft::TYPE_YEAR
        || decimalish(old)
        || old == ft::TYPE_JSON
        || old == ft::TYPE_BIT)
        && matches!(new, ft::TYPE_ENUM | ft::TYPE_SET)
    {
        return false;
    }
    // enum/set/bit/decimal/float/double -> date/datetime/timestamp
    if (matches!(old, ft::TYPE_ENUM | ft::TYPE_SET | ft::TYPE_BIT) || decimalish(old)) && time(new) {
        return false;
    }
    // enum/set/bit -> time
    if matches!(old, ft::TYPE_ENUM | ft::TYPE_SET | ft::TYPE_BIT) && new == ft::TYPE_DURATION {
        return false;
    }
    true
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
    comment: Option<&str>,
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
    // TiDB `checkPriKeyConstraint`: an explicit `NULL` on a PRIMARY KEY
    // column is refused even when the column was already NOT NULL (1171).
    if not_null == Some(false) && is_primary_key_column(&info, old_name) {
        return Err(Error::new(
            ERR_PRIMARY_CANT_HAVE_NULL,
            "All parts of a PRIMARY KEY must be NOT NULL; if you need NULL in a key, \
             use UNIQUE instead"
                .to_string(),
        ));
    }
    // TiDB `ErrUnsupportedModifyCharset` (8200): only a fixed set of type
    // conversions is implemented yet (`checkTypeChangeSupported`).
    let old_ft = &info.columns[pos].field_type;
    if old_ft.tp != new_ft.tp && !check_type_change_supported(old_ft, &new_ft) {
        return Err(Error::new(
            ERR_UNSUPPORTED_MODIFY_CHARSET,
            format!(
                "Unsupported modify column: [ddl:8200]Unsupported modify column: change from original \
                 type {} to {} is currently unsupported yet",
                old_ft.type_name(),
                new_ft.type_name()
            ),
        ));
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
    if changed {
        // TiDB `checkModifyColumnWithForeignKeyConstraint`: a type change that
        // makes a FK's referencing/referenced columns disagree is 3780.
        check_foreign_key_column_change(
            is,
            &schema,
            &info,
            &info.columns[pos],
            new_name,
            &new_ft,
        )?;
        // A BLOB/TEXT target cannot carry a full-column index key (1170).
        check_blob_index_length(&info, old_name, &new_ft, new_name)?;
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
    // TiDB `getModifiableColumnJob` builds the new column from the spec alone
    // (`DefaultValue` stays nil unless the statement spells DEFAULT), so a
    // MODIFY/CHANGE that omits DEFAULT drops the old default instead of
    // revalidating it against the new type.
    let effective_default = crate::create_table::normalize_lob_default(
        new_name,
        &new_ft,
        default
            .map(|default| default.to_string())
            .map(|default| normalize_default(&new_ft, &default)),
    )?;
    validate_display_width(new_name, &new_ft)?;
    validate_default_literal(new_name, &new_ft, effective_default.as_deref())?;
    validate_default_fsp(new_name, &new_ft, effective_default.as_deref())?;
    match effective_default {
        Some(default) => {
            info.columns[pos].origin_default_expr =
                crate::create_table::origin_default_for(&new_ft, &default);
            info.columns[pos].default_expr = Some(default);
        }
        None if respecifies_column => {
            info.columns[pos].default_expr = None;
            info.columns[pos].origin_default_expr = None;
        }
        None => {}
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
    if let Some(comment) = comment {
        info.columns[pos].comment = Some(comment.to_string());
    }
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
    // A FOREIGN KEY follows its column through a rename (TiDB rebuilds the
    // constraint with the new name).
    for fk in &mut info.foreign_keys {
        for col in &mut fk.cols {
            if col.eq_ignore_ascii_case(&previous) {
                *col = new_name.to_string();
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
                    // TiDB substitutes `CURRENT_TIMESTAMP` when a NULL from a
                    // non-TIMESTAMP column reaches a NOT NULL TIMESTAMP column;
                    // its later null scan skips this shape entirely.
                    if value.is_null()
                        && new_ft.flag & sealdb_types::field_type::NOT_NULL_FLAG != 0
                        && new_ft.tp == sealdb_types::field_type::TYPE_TIMESTAMP
                        && col.field_type.tp != sealdb_types::field_type::TYPE_TIMESTAMP
                    {
                        value = sealdb_types::Datum::Time(current_timestamp_value(new_ft));
                    }
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
                    value = convert_column_value(
                        &value,
                        &col.field_type,
                        new_ft,
                        col_name,
                        &mut reorg_warnings,
                    )?;
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
    old_ft: &FieldType,
    ft: &FieldType,
    col_name: &str,
    reorg_warnings: &mut Vec<(i32, String)>,
) -> Result<sealdb_types::Datum> {
    // A JSON source contributes its scalar form: a string is unquoted and a
    // number keeps its text (a boolean/object/array/`null` keeps the raw text
    // so the target's own error reports it).
    let json_temporal;
    let value = if old_ft.tp == sealdb_types::field_type::TYPE_JSON
        && ft.eval_type() == EvalType::Datetime
        && let sealdb_types::Datum::Bytes(bytes) = value
    {
        json_temporal = json_temporal_source(bytes, ft)?;
        &json_temporal
    } else {
        value
    };
    // TiDB's reorg row decoder shifts a TIMESTAMP column's UTC value into the
    // session zone before converting it to any other type.
    let localized_timestamp;
    let value = if old_ft.tp == sealdb_types::field_type::TYPE_TIMESTAMP
        && ft.tp != sealdb_types::field_type::TYPE_TIMESTAMP
        && let sealdb_types::Datum::Time(t) = value
        && t.year != 0
    {
        localized_timestamp =
            sealdb_types::Datum::Time(sealdb_expression::utc_to_session_local(*t)?);
        &localized_timestamp
    } else {
        value
    };
    // ENUM/SET and BIT sources convert to a numeric target by value (member
    // index / bitmask / big-endian bits), not by their text form; a JSON source
    // contributes its scalar value (`true` → 1, a string its unquoted text).
    let numeric_source;
    let json_source;
    let value = if matches!(
        ft.eval_type(),
        EvalType::Int | EvalType::Real | EvalType::Decimal
    ) {
        match value {
            sealdb_types::Datum::Enum(e) => {
                numeric_source = sealdb_types::Datum::I64(e.number as i64);
                &numeric_source
            }
            sealdb_types::Datum::Bit(bytes) => {
                numeric_source = sealdb_types::Datum::U64(
                    bytes.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b)),
                );
                &numeric_source
            }
            sealdb_types::Datum::Bytes(bytes)
                if old_ft.tp == sealdb_types::field_type::TYPE_JSON
                    && ft.eval_type() != EvalType::Int =>
            {
                json_source = json_numeric_source(bytes, ft)?;
                &json_source
            }
            other => other,
        }
    } else {
        value
    };
    // A BIT value cast to JSON becomes a JSON string of its raw bytes
    // (`b'10101'` → `"\u0015"`).
    if ft.tp == sealdb_types::field_type::TYPE_JSON
        && let sealdb_types::Datum::Bit(bytes) = value
    {
        let text = String::from_utf8_lossy(bytes).into_owned();
        let json = serde_json::to_string(&text)
            .unwrap_or_else(|_| "\"\"".to_string());
        return Ok(sealdb_types::Datum::Bytes(json.into_bytes()));
    }
    // A numeric value cast to JSON keeps full DOUBLE precision (a FLOAT column
    // prints its float64 expansion) and drops DECIMAL scale padding.
    if ft.tp == sealdb_types::field_type::TYPE_JSON {
        let text = match value {
            sealdb_types::Datum::F64(v) => Some(sealdb_types::shortest_f64(*v)),
            sealdb_types::Datum::Decimal(d) => Some(trim_decimal_zeros(&d.to_string())),
            // TiDB `Datum.convertToMysqlJSON`: a temporal value becomes a JSON
            // *string* of its display form (`"2020-10-30 08:21:33.455555"`),
            // always with six fractional digits.
            sealdb_types::Datum::Time(t) => {
                Some(serde_json::to_string(&json_temporal_text(t)).unwrap_or_default())
            }
            _ => None,
        };
        if let Some(text) = text {
            return Ok(sealdb_types::Datum::Bytes(text.into_bytes()));
        }
    }
    // Narrowing an integer column to ENUM/SET reinterprets the number as a
    // member index (ENUM) / bitmask (SET); a number outside the member range
    // truncates the value: 1265 in strict mode, a warning otherwise.
    if matches!(ft.tp, TYPE_ENUM | TYPE_SET)
        && let Some(n) = integer_datum(value)
    {
        let members = ft.elems.as_ref().map(|e| e.len()).unwrap_or(0);
        let in_range = n >= 0
            && match ft.tp {
                TYPE_ENUM => (n as u128) <= members as u128,
                _ => members >= 63 || (n as u128) < (1u128 << members),
            };
        let text = datum_text(value);
        let truncated = format!("Data truncated for column '{col_name}', value is '{text}'");
        if !in_range {
            if strict_sql_mode() {
                return Err(Error::new(sealdb_errno::ERR_WARN_DATA_TRUNCATED, truncated));
            }
            reorg_warnings.push((sealdb_errno::ERR_WARN_DATA_TRUNCATED, truncated));
            return Ok(sealdb_types::Datum::Bytes(Vec::new()));
        }
        let elems = ft.elems.as_deref().unwrap_or_default();
        let label = if ft.tp == TYPE_ENUM {
            if n == 0 {
                String::new()
            } else {
                elems.get((n - 1) as usize).cloned().unwrap_or_default()
            }
        } else {
            let mut parts = Vec::new();
            for (i, elem) in elems.iter().enumerate() {
                if n as u128 & (1u128 << i) != 0 {
                    parts.push(elem.as_str());
                }
            }
            parts.join(",")
        };
        return Ok(sealdb_types::Datum::Bytes(label.into_bytes()));
    }
    // A numeric zero read as a date/datetime: strict mode reports 1292, while a
    // non-strict reorg stores the zero temporal value (TiDB treats the
    // `NO_ZERO_DATE` / `NO_ZERO_IN_DATE` pair as advisory outside strict mode).
    if matches!(
        ft.tp,
        sealdb_types::field_type::TYPE_DATE
            | sealdb_types::field_type::TYPE_DATETIME
            | sealdb_types::field_type::TYPE_TIMESTAMP
    ) && zero_number(value)
        && !strict_sql_mode()
    {
        let kind = match ft.tp {
            sealdb_types::field_type::TYPE_DATE => "date",
            sealdb_types::field_type::TYPE_TIMESTAMP => "timestamp",
            _ => "datetime",
        };
        reorg_warnings.push((
            sealdb_errno::ERR_TRUNCATED_WRONG_VALUE,
            format!("Incorrect {kind} value: '{}'", datum_text(value)),
        ));
        return Ok(zero_temporal(ft));
    }
    // TIMESTAMP only holds 1970-01-01 00:00:01 .. 2038-01-19 03:14:07 UTC
    // (TiDB converts the stored wall clock through the session zone first).
    if ft.tp == sealdb_types::field_type::TYPE_TIMESTAMP
        && let sealdb_types::Datum::Time(t) = value
    {
        let utc = sealdb_expression::session_local_to_utc(*t).unwrap_or(*t);
        let stamp = (utc.year, utc.month, utc.day, utc.hour, utc.minute, utc.second);
        if stamp <= (1970, 1, 1, 0, 0, 0) || stamp > (2038, 1, 19, 3, 14, 7) {
            let msg = format!("Incorrect timestamp value: '{}'", t);
            // Outside strict mode an out-of-range TIMESTAMP becomes the zero
            // timestamp with a warning instead of failing the reorg.
            if !strict_sql_mode() {
                reorg_warnings.push((sealdb_errno::ERR_TRUNCATED_WRONG_VALUE, msg));
                return Ok(zero_temporal(ft));
            }
            return Err(Error::new(sealdb_errno::ERR_TRUNCATED_WRONG_VALUE, msg));
        }
    }
    // Converting a string column to JSON validates every value (TiDB
    // `ParseBinaryJSONFromString`, 3140).
    if ft.tp == sealdb_types::field_type::TYPE_JSON
        && let Some(bytes) = match value {
            sealdb_types::Datum::Bytes(bytes) | sealdb_types::Datum::Bit(bytes) => {
                Some(bytes.as_slice())
            }
            sealdb_types::Datum::Enum(e) => Some(e.label.as_slice()),
            _ => None,
        }
    {
        let reason = if bytes.is_empty() {
            Some("The document is empty")
        } else if serde_json::from_slice::<serde_json::Value>(bytes).is_err() {
            Some("The document root must not be followed by other values.")
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(Error::new(
                sealdb_errno::ERR_INVALID_JSON_TEXT,
                format!("Invalid JSON text: {reason}"),
            ));
        }
    }
    // A value wider than `BINARY(N)` truncates: 1265 in strict mode, a warning
    // otherwise (TiDB reports the reorg, not the INSERT 1406).
    if ft.tp == sealdb_types::field_type::TYPE_STRING
        && ft.charset == "binary"
        && ft.flen > 0
    {
        let bytes = value.as_bytes();
        if bytes.len() > ft.flen as usize {
            let msg = format!(
                "Data truncated for column '{col_name}', value is '{}'",
                datum_text(value)
            );
            if strict_sql_mode() {
                return Err(Error::new(sealdb_errno::ERR_WARN_DATA_TRUNCATED, msg));
            }
            reorg_warnings.push((sealdb_errno::ERR_WARN_DATA_TRUNCATED, msg));
        }
    }
    // A character column narrowed below the stored text fails the reorg with
    // 1265 (TiDB raises this even outside strict mode).
    if ft.tp == sealdb_types::field_type::TYPE_STRING
        && ft.charset != "binary"
        && ft.flen > 0
        && String::from_utf8_lossy(&value.as_bytes())
            .chars()
            .count()
            > ft.flen as usize
    {
        return Err(Error::new(
            sealdb_errno::ERR_WARN_DATA_TRUNCATED,
            format!(
                "Data truncated for column '{col_name}', value is '{}'",
                datum_text(value)
            ),
        ));
    }
    // A decimal target that cannot hold the source's integer digits reports
    // TiDB `ErrDataOutOfRange` (1690) `DECIMAL value is out of range in '(M, D)'`.
    let decimal_target = matches!(
        ft.tp,
        sealdb_types::field_type::TYPE_NEW_DECIMAL | sealdb_types::field_type::TYPE_DECIMAL
    );
    if decimal_target
        && ft.flen > 0
        && let Some(digits) = integer_digit_count(value)
    {
        let scale = ft.decimal.max(0) as i64;
        let allowed = (i64::from(ft.flen) - scale).max(0) as u32;
        if digits > allowed {
            let msg = format!(
                "DECIMAL value is out of range in '({}, {})'",
                ft.flen,
                ft.decimal.max(0)
            );
            if strict_sql_mode() {
                return Err(Error::new(sealdb_errno::ERR_DATA_OUT_OF_RANGE, msg));
            }
            reorg_warnings.push((sealdb_errno::ERR_DATA_OUT_OF_RANGE, msg));
            let mut max = "9".repeat(allowed as usize);
            if scale > 0 {
                max.push('.');
                max.push_str(&"9".repeat(scale as usize));
            }
            return Ok(sealdb_types::Datum::Decimal(
                sealdb_types::mydecimal::MyDecimal::parse(&max)
                    .map_err(|err| Error::new(sealdb_errno::ERR_DATA_OUT_OF_RANGE, err.to_string()))?,
            ));
        }
    }
    // YEAR keeps the 2-digit mapping (1-69 → 20xx, 70-99 → 19xx) after
    // rounding the source; anything else must be 0 or 1901-2155 (1264).
    if ft.tp == sealdb_types::field_type::TYPE_YEAR
        && let Some(n) = year_source_value(value)
    {
        return year_column_value(n, col_name, datum_text(value), reorg_warnings);
    }
    // DECIMAL rejects a BINARY(N) value byte for byte (the NUL padding is not a
    // terminator here): 1292. Text with a numeric prefix reports the target
    // precision (`decimal(7,4)`); text without one reports `DECIMAL`.
    if decimal_target && let sealdb_types::Datum::Bytes(bytes) = value {
        let text = String::from_utf8_lossy(bytes);
        if sealdb_types::mydecimal::MyDecimal::parse(&text).is_err() {
            let digits = text.trim_start_matches(['+', '-']);
            let has_prefix = digits.starts_with(|c: char| c.is_ascii_digit());
            let message = if has_prefix {
                format!(
                    "Truncated incorrect decimal({},{}) value: '{text}'",
                    ft.flen,
                    ft.decimal.max(0)
                )
            } else {
                format!("Truncated incorrect DECIMAL value: '{text}'")
            };
            return Err(Error::new(
                sealdb_errno::ERR_TRUNCATED_WRONG_VALUE,
                message,
            ));
        }
    }
    // A DOUBLE target reads a BINARY(N) value as text up to the NUL padding.
    if ft.eval_type() == EvalType::Real
        && let sealdb_types::Datum::Bytes(bytes) = value
        && let Some(end) = bytes.iter().position(|b| *b == 0)
    {
        let text = String::from_utf8_lossy(&bytes[..end]);
        let number: f64 = text.trim().parse().map_err(|_| {
            Error::new(
                sealdb_errno::ERR_TRUNCATED_WRONG_VALUE,
                format!(
                    "Truncated incorrect DOUBLE value: '{}'",
                    sealdb_errno::error_value(&String::from_utf8_lossy(bytes))
                ),
            )
        })?;
        return Ok(sealdb_types::Datum::F64(
            if ft.tp == sealdb_types::field_type::TYPE_FLOAT {
                f64::from(number as f32)
            } else {
                number
            },
        ));
    }
    // Text/binary → integer runs through MySQL's string-to-number conversion.
    // The reorg is strict about the text: anything that is not a whole number
    // fails with 1292 `Truncated incorrect DOUBLE value` (TiDB parses the text
    // as one float, so trailing junk is an error, not a truncation). A JSON
    // source contributes the document's scalar instead of its raw text.
    if ft.eval_type() == EvalType::Int
        && matches!(
            value,
            sealdb_types::Datum::Bytes(_) | sealdb_types::Datum::Bit(_)
        )
    {
        let raw_text = datum_text(value);
        // BINARY(N) is zero padded: the NUL bytes terminate the text (TiDB reads
        // `b'123\0…'` as 123, no truncation warning).
        let text = match raw_text.find('\0') {
            Some(end) => raw_text[..end].to_string(),
            None => raw_text.clone(),
        };
        let number: Option<f64> = if old_ft.tp == sealdb_types::field_type::TYPE_JSON {
            match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(serde_json::Value::Bool(b)) => Some(if b { 1.0 } else { 0.0 }),
                Ok(serde_json::Value::Number(n)) => n.as_f64().filter(|v| v.is_finite()),
                Ok(serde_json::Value::String(s)) => match number_text(&s) {
                    Some(v) => Some(v),
                    None => {
                        let msg = format!(
                            "Truncated incorrect DOUBLE value: '{}'",
                            sealdb_errno::error_value(&s)
                        );
                        if strict_sql_mode() {
                            return Err(Error::new(
                                sealdb_errno::ERR_TRUNCATED_WRONG_VALUE,
                                msg,
                            ));
                        }
                        note_number_warning(reorg_warnings, msg);
                        return Ok(zero_integer_datum(ft));
                    }
                },
                _ => {
                    let msg = format!(
                        "Truncated incorrect INTEGER value: '{}'",
                        sealdb_errno::error_value(&text)
                    );
                    if strict_sql_mode() {
                        return Err(Error::new(sealdb_errno::ERR_TRUNCATED_WRONG_VALUE, msg));
                    }
                    note_number_warning(reorg_warnings, msg);
                    return Ok(zero_integer_datum(ft));
                }
            }
        } else {
            number_text(&text)
        };
        let Some(number) = number else {
            // TiDB quotes the raw column value (BINARY padding included).
            let msg = format!(
                "Truncated incorrect DOUBLE value: '{}'",
                sealdb_errno::error_value(&raw_text)
            );
            if strict_sql_mode() {
                return Err(Error::new(sealdb_errno::ERR_TRUNCATED_WRONG_VALUE, msg));
            }
            note_number_warning(reorg_warnings, msg);
            return Ok(zero_integer_datum(ft));
        };
        // MySQL truncates the fractional part toward zero.
        let truncated = number.trunc();
        if ft.tp == sealdb_types::field_type::TYPE_YEAR {
            return year_column_value(truncated as i128, col_name, text, reorg_warnings);
        }
        if let Some((min, max)) = integer_bounds(ft) {
            let n = clamp_i128(truncated);
            if n < min || n > max || (ft.is_unsigned() && truncated < 0.0) {
                let shown = if old_ft.tp == sealdb_types::field_type::TYPE_JSON {
                    go_float_string(n as f64)
                } else {
                    n.to_string()
                };
                let msg = format!("constant {shown} overflows {}", integer_target_name(ft));
                if strict_sql_mode() {
                    return Err(Error::new(sealdb_errno::ERR_DATA_OUT_OF_RANGE, msg));
                }
                reorg_warnings.push((sealdb_errno::ERR_DATA_OUT_OF_RANGE, msg));
                return Ok(integer_datum_clamped(n.clamp(min, max), min));
            }
            return Ok(integer_datum_clamped(n, min));
        }
    }
    // A non-integer source that does not fit the integer target reports TiDB
    // `ErrDataOutOfRange` (1690) `constant <x> overflows <type>` — as an error
    // in strict mode, as a clamped warning otherwise. Integer→integer keeps
    // the 1265 pre-check form below.
    let old_is_integer = matches!(
        old_ft.tp,
        sealdb_types::field_type::TYPE_TINY
            | sealdb_types::field_type::TYPE_SHORT
            | sealdb_types::field_type::TYPE_INT24
            | sealdb_types::field_type::TYPE_LONG
            | sealdb_types::field_type::TYPE_LONG_LONG
    );
    if let Some((min, max)) = integer_bounds(ft)
        && !old_is_integer
        && let Some(text) = integer_overflow_text(value, old_ft, ft, min, max)
    {
        let msg = format!("constant {text} overflows {}", integer_target_name(ft));
        if strict_sql_mode() {
            return Err(Error::new(sealdb_errno::ERR_DATA_OUT_OF_RANGE, msg));
        }
        reorg_warnings.push((sealdb_errno::ERR_DATA_OUT_OF_RANGE, msg));
        let clamped = match value {
            sealdb_types::Datum::F64(v) => (*v).round() as i128,
            sealdb_types::Datum::Time(t) => t.to_number() as i128,
            sealdb_types::Datum::I64(v) => *v as i128,
            sealdb_types::Datum::U64(v) => *v as i128,
            sealdb_types::Datum::Enum(e) => e.number as i128,
            sealdb_types::Datum::Bytes(bytes) | sealdb_types::Datum::Bit(bytes) => {
                String::from_utf8_lossy(bytes).trim().parse().unwrap_or(0)
            }
            _ => decimal_int_i128(value).unwrap_or(0),
        }
        .clamp(min, max);
        return Ok(if min >= 0 {
            sealdb_types::Datum::U64(clamped as u64)
        } else {
            sealdb_types::Datum::I64(clamped as i64)
        });
    }
    let mut out = value.cast(ft)?;
    // FLOAT(M,D) / DOUBLE(M,D) keep D decimal digits: the reorg rounds the
    // stored value (`36.43` into FLOAT(6,1) is `36.4`).
    if matches!(
        ft.tp,
        sealdb_types::field_type::TYPE_FLOAT | sealdb_types::field_type::TYPE_DOUBLE
    ) && ft.decimal > 0
        && let sealdb_types::Datum::F64(v) = out
    {
        let scale = 10f64.powi(ft.decimal.min(30));
        let rounded = (v * scale).round() / scale;
        out = sealdb_types::Datum::F64(if ft.tp == sealdb_types::field_type::TYPE_FLOAT {
            f64::from(rounded as f32)
        } else {
            rounded
        });
    }
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
            // Strict mode fails the reorg on the first out-of-range value
            // (1265); outside strict mode TiDB clamps and warns (1690).
            if strict_sql_mode() {
                return Err(Error::new(
                    sealdb_errno::ERR_WARN_DATA_TRUNCATED,
                    format!(
                        "Data truncated for column '{col_name}', value is '{}'",
                        datum_text(value)
                    ),
                ));
            }
            reorg_warnings.push((
                sealdb_errno::ERR_DATA_OUT_OF_RANGE,
                format!("constant {observed} overflows {}", integer_target_name(ft)),
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
    // TIMESTAMP columns store UTC: a wall clock coming from another type is
    // read in the session zone and shifted (a TIMESTAMP source is already UTC).
    if ft.tp == sealdb_types::field_type::TYPE_TIMESTAMP
        && old_ft.tp != sealdb_types::field_type::TYPE_TIMESTAMP
        && let sealdb_types::Datum::Time(t) = out
    {
        let mut t = t;
        t.tp = sealdb_types::TimeType::Timestamp;
        t = sealdb_expression::session_local_to_utc(t)?;
        t.tp = sealdb_types::TimeType::Timestamp;
        out = sealdb_types::Datum::Time(t);
    }
    Ok(out)
}

/// TiDB's JSON text for a temporal value: the display form with the stored
/// microseconds always rendered as six digits (`19:38:25.001` → `19:38:25.001000`).
fn json_temporal_text(t: &sealdb_types::Time) -> String {
    let text = t.to_string();
    if t.micro == 0 {
        return text;
    }
    match text.split_once('.') {
        Some((_, frac)) if frac.len() >= 6 => text,
        Some((base, frac)) => format!("{base}.{frac:0<6}"),
        None => format!("{text}.{:06}", t.micro),
    }
}

/// A JSON value's text for a temporal target: strings are unquoted, numbers keep
/// their text, and everything else keeps the raw document text so the target's
/// `Incorrect <type> value` error reports it verbatim. A TIME target reports the
/// numeric form of the error (`Truncated incorrect time value`).
fn json_temporal_source(bytes: &[u8], ft: &FieldType) -> Result<sealdb_types::Datum> {
    let raw = String::from_utf8_lossy(bytes).into_owned();
    match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(serde_json::Value::String(s)) => Ok(sealdb_types::Datum::Bytes(s.into_bytes())),
        Ok(serde_json::Value::Number(n)) => {
            Ok(sealdb_types::Datum::Bytes(n.to_string().into_bytes()))
        }
        _ => {
            if ft.tp == sealdb_types::field_type::TYPE_DURATION {
                return Err(Error::new(
                    sealdb_errno::ERR_TRUNCATED_WRONG_VALUE,
                    format!(
                        "Truncated incorrect time value: '{}'",
                        sealdb_errno::error_value(&raw)
                    ),
                ));
            }
            Ok(sealdb_types::Datum::Bytes(raw.into_bytes()))
        }
    }
}

/// TiDB's JSON → numeric conversion: a boolean is 1/0, a number keeps its text,
/// a string contributes its unquoted content (1292 when it is not a number) and
/// an object/array/`null` document fails with its raw text.
fn json_numeric_source(
    bytes: &[u8],
    ft: &FieldType,
) -> Result<sealdb_types::Datum> {
    let raw = String::from_utf8_lossy(bytes).into_owned();
    let decimal_target = ft.eval_type() == EvalType::Decimal;
    let invalid_text = |shown: &str| {
        let message = if decimal_target {
            let digits = shown.trim_start_matches(['+', '-']);
            if digits.starts_with(|c: char| c.is_ascii_digit()) {
                format!(
                    "Truncated incorrect decimal({},{}) value: '{shown}'",
                    ft.flen,
                    ft.decimal.max(0)
                )
            } else {
                format!("Truncated incorrect DECIMAL value: '{shown}'")
            }
        } else {
            format!("Truncated incorrect DOUBLE value: '{shown}'")
        };
        Error::new(sealdb_errno::ERR_TRUNCATED_WRONG_VALUE, message)
    };
    match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(serde_json::Value::Bool(b)) => {
            Ok(sealdb_types::Datum::I64(if b { 1 } else { 0 }))
        }
        Ok(serde_json::Value::Number(n)) => {
            Ok(sealdb_types::Datum::Bytes(n.to_string().into_bytes()))
        }
        Ok(serde_json::Value::String(s)) => {
            let numeric = if decimal_target {
                sealdb_types::mydecimal::MyDecimal::parse(&s).is_ok()
            } else {
                s.trim().parse::<f64>().is_ok()
            };
            if numeric {
                Ok(sealdb_types::Datum::Bytes(s.into_bytes()))
            } else {
                Err(invalid_text(&s))
            }
        }
        // A JSON object/array/`null` has no scalar form; TiDB reports the raw
        // document text (and says FLOAT rather than DOUBLE for a real target).
        _ => Err(Error::new(
            sealdb_errno::ERR_TRUNCATED_WRONG_VALUE,
            if decimal_target {
                format!(
                    "Truncated incorrect DECIMAL value: '{}'",
                    sealdb_errno::error_value(&raw)
                )
            } else {
                format!(
                    "Truncated incorrect FLOAT value: '{}'",
                    sealdb_errno::error_value(&raw)
                )
            },
        )),
    }
}

/// `-258.1234500` → `-258.12345` (JSON numbers carry no scale padding).
fn trim_decimal_zeros(text: &str) -> String {
    if !text.contains('.') {
        return text.to_string();
    }
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    trimmed.to_string()
}

/// Number of significant digits in the integer part of a numeric value/// (`0.5` → 0, `200805.11` → 6). `None` for values that convert through text.
fn integer_digit_count(value: &sealdb_types::Datum) -> Option<u32> {
    fn digits(magnitude: u128) -> u32 {
        if magnitude == 0 {
            0
        } else {
            magnitude.ilog10() + 1
        }
    }
    match value {
        sealdb_types::Datum::I64(v) => Some(digits(v.unsigned_abs() as u128)),
        sealdb_types::Datum::U64(v) => Some(digits(*v as u128)),
        sealdb_types::Datum::Bit(bytes) => {
            let magnitude = bytes
                .iter()
                .fold(0u64, |acc, b| (acc << 8) | u64::from(*b));
            Some(digits(magnitude as u128))
        }
        sealdb_types::Datum::F64(v) => {
            if !v.is_finite() {
                return None;
            }
            let magnitude = v.trunc().abs();
            if magnitude < 1.0 {
                Some(0)
            } else if magnitude < u128::MAX as f64 {
                Some(digits(magnitude as u128))
            } else {
                Some(40)
            }
        }
        sealdb_types::Datum::Decimal(d) => {
            let text = d.to_string();
            let unsigned = text.trim_start_matches('-');
            let integer = unsigned.split('.').next().unwrap_or("0");
            let trimmed = integer.trim_start_matches('0');
            Some(trimmed.len() as u32)
        }
        _ => None,
    }
}

/// Rounded numeric value used for a YEAR conversion (TiDB rounds first, then
/// applies the 2-digit year mapping).
fn year_source_value(value: &sealdb_types::Datum) -> Option<i128> {
    match value {
        sealdb_types::Datum::I64(v) => Some(*v as i128),
        sealdb_types::Datum::Year(v) => Some(*v as i128),
        sealdb_types::Datum::U64(v) => Some(*v as i128),
        sealdb_types::Datum::F64(v) => Some(v.round() as i128),
        sealdb_types::Datum::Decimal(d) => decimal_round_i128(&d.to_string()),
        sealdb_types::Datum::Time(t) => Some(t.to_number() as i128),
        sealdb_types::Datum::Bytes(bytes) | sealdb_types::Datum::Bit(bytes) => {
            String::from_utf8_lossy(bytes).trim().parse().ok()
        }
        sealdb_types::Datum::Enum(e) => Some(e.number as i128),
        sealdb_types::Datum::Null => None,
    }
}

/// Map a rounded value onto the YEAR column's 2-digit rule.
fn year_column_value(
    n: i128,
    col_name: &str,
    value_text: String,
    reorg_warnings: &mut Vec<(i32, String)>,
) -> Result<sealdb_types::Datum> {
    let year = match n {
        0 => 0,
        1..=69 => 2000 + n,
        70..=99 => 1900 + n,
        1901..=2155 => n,
        _ => {
            let msg =
                format!("Out of range value for column '{col_name}', the value is '{value_text}'");
            // Strict mode rejects the reorg; otherwise the year becomes 0 with
            // a 1264 warning (suite: TIME '12:13:14' under `sql_mode=''`).
            if strict_sql_mode() {
                return Err(Error::new(sealdb_errno::ERR_WARN_DATA_OUT_OF_RANGE, msg));
            }
            reorg_warnings.push((sealdb_errno::ERR_WARN_DATA_OUT_OF_RANGE, msg));
            return Ok(sealdb_types::Datum::Year(0));
        }
    };
    Ok(sealdb_types::Datum::Year(year as i64))
}

/// A numeric datum that reads as the temporal zero value.
fn zero_number(d: &sealdb_types::Datum) -> bool {
    match d {
        sealdb_types::Datum::I64(v) => *v == 0,
        sealdb_types::Datum::U64(v) => *v == 0,
        sealdb_types::Datum::F64(v) => *v == 0.0,
        sealdb_types::Datum::Decimal(d) => d.to_f64().map(|v| v == 0.0).unwrap_or(false),
        _ => false,
    }
}

/// `CURRENT_TIMESTAMP` as a stored TIMESTAMP value (UTC wall clock). TiDB uses
/// it when a NULL of another type lands in a NOT NULL TIMESTAMP column.
fn current_timestamp_value(ft: &FieldType) -> sealdb_types::time::Time {
    let (secs, micro) = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_secs() as i64, d.subsec_micros()))
        .unwrap_or((0, 0));
    let mut t = sealdb_types::time::Time::from_unix_secs(secs, sealdb_types::TimeType::Timestamp);
    let fsp = ft.decimal.clamp(0, 6) as i8;
    t.fsp = fsp;
    if fsp > 0 {
        t.micro = micro;
    }
    t
}

/// The zero value of a temporal target type.
fn zero_temporal(ft: &FieldType) -> sealdb_types::Datum {    let mut t = sealdb_types::time::Time::new(0, 0, 0, 0, 0, 0);
    t.tp = match ft.tp {
        sealdb_types::field_type::TYPE_DATE => sealdb_types::TimeType::Date,
        sealdb_types::field_type::TYPE_TIMESTAMP => sealdb_types::TimeType::Timestamp,
        _ => sealdb_types::TimeType::Datetime,
    };
    t.fsp = ft.decimal.clamp(0, 6) as i8;
    sealdb_types::Datum::Time(t)
}

/// Parse a full numeric spelling the way MySQL's string-to-number conversion
/// does: the whole text (after trimming) has to be one finite float.
fn number_text(text: &str) -> Option<f64> {
    text.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

fn note_number_warning(reorg_warnings: &mut Vec<(i32, String)>, message: String) {
    reorg_warnings.push((sealdb_errno::ERR_TRUNCATED_WRONG_VALUE, message));
}

fn zero_integer_datum(ft: &FieldType) -> sealdb_types::Datum {
    if ft.is_unsigned() {
        sealdb_types::Datum::U64(0)
    } else {
        sealdb_types::Datum::I64(0)
    }
}

/// Saturating float→i128 so huge spellings still report an overflow (1690)
/// instead of wrapping.
fn clamp_i128(v: f64) -> i128 {
    if v >= i128::MAX as f64 {
        i128::MAX
    } else if v <= i128::MIN as f64 {
        i128::MIN
    } else {
        v as i128
    }
}

fn integer_datum_clamped(n: i128, min: i128) -> sealdb_types::Datum {
    if min >= 0 {
        sealdb_types::Datum::U64(n as u64)
    } else {
        sealdb_types::Datum::I64(n as i64)
    }
}

/// Round a decimal spelling (`-258.1234500`) to its nearest integer.
fn decimal_round_i128(text: &str) -> Option<i128> {
    let (negative, rest) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (int_part, frac) = rest.split_once('.').unwrap_or((rest, ""));
    let magnitude: i128 = int_part.parse().ok()?;
    let round_up = frac
        .bytes()
        .next()
        .is_some_and(|digit| digit >= b'5');
    let rounded = if round_up { magnitude + 1 } else { magnitude };
    Some(if negative { -rounded } else { rounded })
}

/// Integer target spelling inside `constant <x> overflows <type>`: TiDB keeps
/// the signed family name even for `BIGINT UNSIGNED`.
fn integer_target_name(ft: &FieldType) -> String {
    ft.type_name()
        .trim_end_matches(" unsigned")
        .to_string()
}

/// TiDB `constant <x> overflows <type>` payload for a value whose *source* is
/// not an integer (the integer-source overflow keeps the 1265 form below).
/// Returns `None` when the value fits (or cannot be judged here).
fn integer_overflow_text(
    value: &sealdb_types::Datum,
    old_ft: &FieldType,
    ft: &FieldType,
    min: i128,
    max: i128,
) -> Option<String> {
    let fits = |n: i128| n >= min && n <= max;
    match value {
        sealdb_types::Datum::Decimal(d) => {
            let text = d.to_string();
            // TiDB converts a negative decimal to an unsigned target before
            // rounding, so the original spelling is what it reports.
            if ft.is_unsigned() && text.starts_with('-') {
                return Some(text);
            }
            let n = decimal_int_i128(value)?;
            (!fits(n)).then(|| n.to_string())
        }
        sealdb_types::Datum::F64(v) => {
            let n = v.round() as i128;
            if (ft.is_unsigned() && *v < 0.0) || !fits(n) {
                Some(go_float_string(n as f64))
            } else {
                None
            }
        }
        sealdb_types::Datum::Time(t) => {
            let n = t.to_number() as i128;
            (!fits(n)).then(|| n.to_string())
        }
        sealdb_types::Datum::Bytes(bytes) | sealdb_types::Datum::Bit(bytes) => {
            let text = String::from_utf8_lossy(bytes);
            let text = text.trim();
            if let Ok(n) = text.parse::<i128>() {
                return (!fits(n)).then(|| n.to_string());
            }
            // A JSON numeric value converts through float64, so TiDB reports
            // its integer with Go's float spelling.
            let v: f64 = text.parse().ok()?;
            if !v.is_finite() {
                return None;
            }
            let n = v.round() as i128;
            if fits(n) && !(ft.is_unsigned() && v < 0.0) {
                return None;
            }
            Some(if old_ft.tp == sealdb_types::field_type::TYPE_JSON {
                go_float_string(n as f64)
            } else {
                n.to_string()
            })
        }
        sealdb_types::Datum::Enum(e) => {
            let n = e.number as i128;
            (!fits(n)).then(|| n.to_string())
        }
        sealdb_types::Datum::I64(v) => (!fits(*v as i128)).then(|| v.to_string()),
        sealdb_types::Datum::Year(v) => (!fits(*v as i128)).then(|| v.to_string()),
        sealdb_types::Datum::U64(v) => (!fits(*v as i128)).then(|| v.to_string()),
        sealdb_types::Datum::Null => None,
    }
}

/// Integer part of a decimal value, truncated toward zero.
fn decimal_int_i128(value: &sealdb_types::Datum) -> Option<i128> {
    let sealdb_types::Datum::Decimal(d) = value else {
        return None;
    };
    let text = d.to_string();
    let (negative, rest) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.as_str()),
    };
    let int_part = rest.split('.').next()?;
    let magnitude: i128 = int_part.parse().ok()?;
    Some(if negative { -magnitude } else { magnitude })
}

/// Go's `%v` spelling of a float (`strconv.FormatFloat(f, 'g', -1, 64)`), which
/// TiDB uses in `constant <x> overflows <type>` messages: exponent form when
/// the decimal exponent is below -4 or at least 6, two-digit exponent.
fn go_float_string(v: f64) -> String {
    if v == 0.0 {
        return "0".to_string();
    }
    let formatted = format!("{v:e}");
    let (mantissa, exponent) = formatted
        .split_once('e')
        .expect("LowerExp always writes an exponent");
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let body = if exponent < -4 || exponent >= 6 {
        let (head, tail) = digits.split_at(1);
        let mantissa = if tail.is_empty() {
            head.to_string()
        } else {
            format!("{head}.{tail}")
        };
        format!(
            "{mantissa}e{}{:02}",
            if exponent < 0 { '-' } else { '+' },
            exponent.abs()
        )
    } else if exponent >= 0 {
        let int_len = exponent as usize + 1;
        if digits.len() <= int_len {
            format!("{digits}{}", "0".repeat(int_len - digits.len()))
        } else {
            format!("{}.{}", &digits[..int_len], &digits[int_len..])
        }
    } else {
        format!("0.{}{}", "0".repeat((-exponent - 1) as usize), digits)
    };
    if negative { format!("-{body}") } else { body }
}

/// Numeric value of a decoded integer datum.
fn integer_datum(value: &sealdb_types::Datum) -> Option<i128> {
    match value {
        sealdb_types::Datum::I64(v) => Some(*v as i128),
        sealdb_types::Datum::U64(v) => Some(*v as i128),
        _ => None,
    }
}

/// TiDB `sql_mode` strictness for reorg conversions.
fn strict_sql_mode() -> bool {
    sealdb_expression::sql_mode_flag("STRICT_TRANS_TABLES")
        || sealdb_expression::sql_mode_flag("STRICT_ALL_TABLES")
}

/// Row value as the reorg error message spells it.
fn datum_text(value: &sealdb_types::Datum) -> String {
    match value {
        sealdb_types::Datum::Null => "NULL".to_string(),
        sealdb_types::Datum::I64(v) => v.to_string(),
        sealdb_types::Datum::Year(v) => v.to_string(),
        sealdb_types::Datum::U64(v) => v.to_string(),
        sealdb_types::Datum::F64(v) => sealdb_types::Datum::F64(*v).display(),
        sealdb_types::Datum::Decimal(d) => d.to_string(),
        sealdb_types::Datum::Bit(b) | sealdb_types::Datum::Bytes(b) => {
            String::from_utf8_lossy(b).into_owned()
        }
        sealdb_types::Datum::Enum(e) => String::from_utf8_lossy(&e.label).into_owned(),
        sealdb_types::Datum::Time(t) => t.to_string(),
    }
}

/// Whether `name` is part of the table's PRIMARY KEY (including the clustered
/// handle column).
fn is_primary_key_column(info: &TableInfo, name: &str) -> bool {
    info.indices.iter().any(|index| {
        index.primary
            && index
                .columns
                .iter()
                .any(|col| col.name.o().eq_ignore_ascii_case(name))
    })
}

/// TiDB `checkIndexColumn`: after changing a column to BLOB/TEXT/JSON, every
/// index on it needs a prefix shorter than the column, else 1170.
fn check_blob_index_length(
    info: &TableInfo,
    col_name: &str,
    new_ft: &FieldType,
    new_name: &str,
) -> Result<()> {
    if !crate::create_table::is_lob_type(new_ft) {
        return Ok(());
    }
    let max_prefix = lob_max_bytes(new_ft);
    for index in &info.indices {
        for ic in &index.columns {
            if !ic.name.o().eq_ignore_ascii_case(col_name) {
                continue;
            }
            let full_column = match max_prefix {
                Some(limit) => ic.length < 0 || i64::from(ic.length) > limit,
                None => ic.length < 0,
            };
            if full_column {
                return Err(Error::new(
                    ERR_BLOB_USED_AS_KEY,
                    format!(
                        "BLOB/TEXT column '{new_name}' used in key specification without a key length"
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Maximum indexable byte length of a BLOB/TEXT family column.
fn lob_max_bytes(ft: &FieldType) -> Option<i64> {
    match ft.tp {
        sealdb_types::field_type::TYPE_TINY_BLOB => Some(255),
        sealdb_types::field_type::TYPE_BLOB => Some(65_535),
        sealdb_types::field_type::TYPE_MEDIUM_BLOB => Some(16_777_215),
        sealdb_types::field_type::TYPE_LONG_BLOB => Some(4_294_967_295),
        _ => None,
    }
}

/// TiDB `checkModifyColumnWithForeignKeyConstraint`: changing a column's type
/// must keep every foreign key's referencing/referenced pair on the same type
/// (3780). Column length/decimals may change without this check.
fn check_foreign_key_column_change(
    is: &InfoSchema,
    schema: &str,
    info: &TableInfo,
    old_col: &ColumnInfo,
    new_name: &str,
    new_ft: &FieldType,
) -> Result<()> {
    let old_ft = &old_col.field_type;
    if new_ft.tp == old_ft.tp && new_ft.flen == old_ft.flen && new_ft.decimal == old_ft.decimal {
        return Ok(());
    }
    let incompatible = |child: &str, parent: &str, name: &str| {
        Error::new(
            ERR_FK_INCOMPATIBLE_COLUMNS,
            format!(
                "Referencing column '{child}' and referenced column '{parent}' in foreign key \
                 constraint '{name}' are incompatible."
            ),
        )
    };
    // Child side: this table references another table.
    for fk in &info.foreign_keys {
        let Some(index) = fk
            .cols
            .iter()
            .position(|col| col.eq_ignore_ascii_case(old_col.name.o()))
        else {
            continue;
        };
        let Some(parent) = is.table_by_name(schema, &fk.ref_table) else {
            continue;
        };
        let ref_cols = referenced_columns(fk, parent);
        let Some(ref_name) = ref_cols.get(index) else {
            continue;
        };
        let Some(ref_col) = parent
            .columns
            .iter()
            .find(|col| col.name.o().eq_ignore_ascii_case(ref_name))
        else {
            continue;
        };
        if new_ft.tp != ref_col.field_type.tp {
            return Err(incompatible(new_name, ref_col.name.o(), &fk.name));
        }
    }
    // Parent side: other tables reference this column.
    for db in is.all_schemas() {
        let db_name = db.name.o().to_string();
        if !db_name.eq_ignore_ascii_case(schema) {
            continue;
        }
        for child in is.schema_tables(&db_name) {
            for fk in &child.foreign_keys {
                if !fk.ref_table.eq_ignore_ascii_case(info.name.o()) {
                    continue;
                }
                let ref_cols = referenced_columns(fk, info);
                let Some(index) = ref_cols
                    .iter()
                    .position(|col| col.eq_ignore_ascii_case(old_col.name.o()))
                else {
                    continue;
                };
                let Some(child_name) = fk.cols.get(index) else {
                    continue;
                };
                let Some(child_col) = child
                    .columns
                    .iter()
                    .find(|col| col.name.o().eq_ignore_ascii_case(child_name))
                else {
                    continue;
                };
                if new_ft.tp != child_col.field_type.tp {
                    return Err(incompatible(child_col.name.o(), old_col.name.o(), &fk.name));
                }
            }
        }
    }
    Ok(())
}

/// Columns a foreign key references: its explicit list, or the parent's
/// PRIMARY KEY columns when the DDL omitted them.
fn referenced_columns(fk: &sealdb_meta::ForeignKeyInfo, parent: &TableInfo) -> Vec<String> {
    if !fk.ref_cols.is_empty() {
        return fk.ref_cols.clone();
    }
    parent
        .indices
        .iter()
        .find(|index| index.primary)
        .map(|index| {
            index
                .columns
                .iter()
                .map(|col| col.name.o().to_string())
                .collect()
        })
        .unwrap_or_default()
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
