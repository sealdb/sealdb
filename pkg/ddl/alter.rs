// /ALTER TABLE ADD/DROP COLUMN|INDEX|PARTITION, CHANGE/MODIFY/RENAME,
// AFTER/FIRST, ALGORITHM/LOCK (accepted, in-place).
// Completeness wave / not this feature: generated columns, nested subpartition storage,
// multi-schema-state jobs.
use crate::create_table::field_type_from_name;
use crate::index::{add_index, backfill_index, drop_index_keys, remove_index};
use crate::partition::{add_partitions, drop_partitions, exchange_partition};
use crate::table::resolve_name;
use sealdb_errno::{
    ERR_BAD_DB, ERR_BAD_FIELD, ERR_CANT_DROP_FIELD_OR_KEY, ERR_DUP_FIELDNAME, ERR_DUP_KEYNAME,
    ERR_NO_SUCH_TABLE, ERR_TABLE_EXISTS, Error, Result,
};
use sealdb_infoschema::InfoSchema;
use sealdb_kv::Storage;
use sealdb_meta::{ColumnInfo, Mutator, TableInfo};
use sealdb_parser::ast::{AlterTableSpec, AlterTableStmt, ColumnPosition, TableName};
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
                position,
            } => change_column(
                meta,
                &snapshot,
                stmt,
                old_name,
                new_name,
                Some(type_name),
                position,
                current_db,
                storage,
            )?,
            AlterTableSpec::ModifyColumn {
                name,
                type_name,
                position,
            } => change_column(
                meta,
                &snapshot,
                stmt,
                name,
                name,
                Some(type_name),
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
            // ALGORITHM/LOCK are accepted and ignored; SealDB DDL is in-place.
            AlterTableSpec::Algorithm(_) | AlterTableSpec::Lock(_) => {}
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
        }
        snapshot = InfoSchema::from_meta(meta)?;
    }
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
    new_col.default_expr = column.default.clone();
    if column.not_null {
        new_col.field_type.flag |= sealdb_types::field_type::NOT_NULL_FLAG;
    }
    let new_id = new_col.id;
    let default_value = column_default_datum(&new_col)?;
    // Existing rows were written before this column existed, so decode them
    // with the pre-ALTER layout (offsets in `info` already include the new
    // column and would mis-map the stored values).
    let old_types = info.col_field_types();
    info.columns.insert(insert_at, new_col);
    recompute_column_offsets(&mut info);
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
    if info
        .handle_column()
        .is_some_and(|handle| handle.id == info.columns[pos].id)
    {
        return Err(Error::unsupported("DROP handle column"));
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
fn change_column(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &AlterTableStmt,
    old_name: &str,
    new_name: &str,
    type_name: Option<&str>,
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
    let new_ft = match type_name {
        Some(name) => field_type_from_name(name)?,
        None => info.columns[pos].field_type.clone(),
    };
    if info
        .handle_column()
        .is_some_and(|handle| handle.id == info.columns[pos].id)
        && new_ft.eval_type() != EvalType::Int
    {
        return Err(Error::unsupported("MODIFY handle column type"));
    }
    let changed = field_type_changed(&info.columns[pos].field_type, &new_ft);
    if changed {
        rewrite_column_rows(storage, &info, info.columns[pos].id, &new_ft)?;
    }
    let previous = info.columns[pos].name.o().to_string();
    info.columns[pos].name = CIStr::new(new_name);
    info.columns[pos].field_type = new_ft;
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
        let col = info.columns.remove(pos);
        let insert_at = column_insert_index(&info, position)?;
        info.columns.insert(insert_at, col);
        recompute_column_offsets(&mut info);
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
    new_ft: &FieldType,
) -> Result<()> {
    let old_types = info.col_field_types();
    let mut txn = storage.begin()?;
    for physical_id in info.physical_ids() {
        let start = gen_table_record_prefix(physical_id);
        let end = next_prefix(start.as_bytes());
        let pairs = txn.scan(start.as_bytes(), &end)?;
        for (key, raw) in pairs {
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
                    value = convert_column_value(&value, new_ft)?;
                }
                col_ids.push(col.id);
                values.push(value);
            }
            txn.set(&key, &encode_row(&col_ids, &values))?;
        }
    }
    txn.commit()
}

fn convert_column_value(
    value: &sealdb_types::Datum,
    ft: &FieldType,
) -> Result<sealdb_types::Datum> {
    let mut out = value.cast(ft)?;
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
            format!("Table '{new_table}' already exists"),
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

fn column_insert_index(info: &TableInfo, position: &ColumnPosition) -> Result<usize> {    match position {
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
