use crate::create_table::{
    build_table_info, build_table_info_with_like, columns_from_select, referenced_by_foreign_key,
    validate_ttl,
};
use sealdb_errno::{
    ERR_BAD_DB, ERR_BAD_TABLE, ERR_NO_DB, ERR_NO_SUCH_TABLE, ERR_TABLE_EXISTS,
    ERR_TTL_FK_REFERENCED, ERR_UNKNOWN_ERROR, ERR_WRONG_OBJECT, Error, Result,
};
use sealdb_infoschema::InfoSchema;
use sealdb_kv::Storage;
use sealdb_meta::{Mutator, TableInfo};
use sealdb_parser::ast::{CreateTableStmt, DropTableStmt, TableName, TruncateTableStmt};
use sealdb_tablecodec::{encode_table_prefix, next_prefix};

pub fn create_table(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &CreateTableStmt,
    current_db: &str,
) -> Result<TableInfo> {
    let (schema, table) = resolve_name(&stmt.table, current_db)?;
    if let Some(existing) = is.table_by_name(&schema, &table) {
        if stmt.if_not_exists {
            return Ok(existing.clone());
        }
        return Err(Error::new(
            ERR_TABLE_EXISTS,
            format!("Table '{schema}.{table}' already exists"),
        ));
    }
    let db = is
        .schema_by_name(&schema)
        .ok_or_else(|| Error::new(ERR_BAD_DB, format!("Unknown database '{schema}'")))?;
    let table_id = meta.gen_global_id()?;
    let info = if let Some(refer) = &stmt.refer_table {
        let (ref_schema, ref_table) = resolve_name(refer, current_db)?;
        let refer_info = is.table_by_name(&ref_schema, &ref_table).ok_or_else(|| {
            Error::new(
                ERR_NO_SUCH_TABLE,
                format!("Table '{ref_schema}.{ref_table}' doesn't exist"),
            )
        })?;
        build_table_info_with_like(&table, refer_info, table_id, meta)?
    } else {
        let mut built = stmt.clone();
        if built.cols.is_empty()
            && let Some(select) = built.select.as_deref()
        {
            built.cols = columns_from_select(is, select, current_db)?;
        }
        let mut info = build_table_info(&built, table_id)?;
        if let Some(ttl) = &built.ttl {
            info.ttl = Some(sealdb_meta::TtlInfo {
                expr: ttl.expr.clone(),
                enable: ttl.enable,
                job_interval: ttl.job_interval.clone(),
            });
        }
        validate_ttl(&info)?;
        // A FOREIGN KEY child cannot reference a TTL table (8152).
        for fk in &info.foreign_keys {
            if let Some(parent) = is.table_by_name(&schema, &fk.ref_table)
                && parent.ttl.is_some()
            {
                return Err(Error::new(
                    ERR_TTL_FK_REFERENCED,
                    "Set TTL for a table referenced by foreign key is not allowed",
                ));
            }
        }
        if info.ttl.is_some() && referenced_by_foreign_key(is, &table) {
            return Err(Error::new(
                ERR_TTL_FK_REFERENCED,
                "Set TTL for a table referenced by foreign key is not allowed",
            ));
        }
        if let Some(spec) = &built.partition {
            crate::partition::attach_partitions(meta, &mut info, spec)?;
        }
        info
    };
    meta.create_table_or_view(db.id, &info)?;
    meta.gen_schema_version()?;
    Ok(info)
}

pub fn drop_table(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &DropTableStmt,
    current_db: &str,
) -> Result<()> {
    for name in &stmt.tables {
        let (schema, table) = match resolve_name(name, current_db) {
            Ok(pair) => pair,
            Err(err) if stmt.if_exists && err.code == ERR_NO_DB => continue,
            Err(err) => return Err(err),
        };
        // GC tables are managed by TiDB itself; dropping them is refused with 1105.
        if schema.eq_ignore_ascii_case("mysql") && table.eq_ignore_ascii_case("gc_delete_range") {
            return Err(Error::new(
                ERR_UNKNOWN_ERROR,
                format!("Drop tidb system table 'mysql.{table}' is forbidden"),
            ));
        }
        match (
            is.schema_by_name(&schema),
            is.table_by_name(&schema, &table),
        ) {
            (Some(db), Some(tbl)) => {
                if stmt.is_view && !tbl.is_view() {
                    return Err(Error::new(
                        ERR_WRONG_OBJECT,
                        format!("'{schema}.{table}' is not VIEW"),
                    ));
                }
                if !stmt.is_view && tbl.is_view() {
                    return Err(Error::new(
                        ERR_BAD_TABLE,
                        format!("Unknown table '{schema}.{table}'"),
                    ));
                }
                if !stmt.is_view && tbl.is_sequence() {
                    return Err(Error::new(
                        ERR_WRONG_OBJECT,
                        format!("'{schema}.{table}' is not BASE TABLE"),
                    ));
                }
                meta.drop_table_or_view(db.id, tbl.id)?;
                meta.gen_schema_version()?;
            }
            _ if stmt.if_exists => {
                // MySQL reports a note per missing object for
                // `DROP {TABLE|VIEW} IF EXISTS` (1051).
                if is.schema_by_name(&schema).is_some() {
                    sealdb_expression::note_warning(
                        ERR_BAD_TABLE,
                        format!("Unknown table '{schema}.{table}'"),
                    );
                }
            }
            (None, _) => {
                return Err(Error::new(
                    ERR_BAD_DB,
                    format!("Unknown database '{schema}'"),
                ));
            }
            (_, None) if stmt.is_view => {
                return Err(Error::new(
                    ERR_BAD_TABLE,
                    format!("Unknown table '{schema}.{table}'"),
                ));
            }
            (_, None) => {
                return Err(Error::new(
                    ERR_NO_SUCH_TABLE,
                    format!("Table '{schema}.{table}' doesn't exist"),
                ));
            }
        }
    }
    Ok(())
}

/// Clear all row/index keys for the table and reset next_handle; keep schema.
pub fn truncate_table(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &TruncateTableStmt,
    current_db: &str,
    storage: &dyn Storage,
) -> Result<()> {
    let (schema, table) = resolve_name(&stmt.table, current_db)?;
    let tbl = is
        .table_by_name(&schema, &table)
        // A VIEW or SEQUENCE is not a table: TiDB resolves `TRUNCATE` against
        // the table map only, so those names report `ErrNoSuchTable`.
        .filter(|tbl| tbl.view.is_none() && tbl.sequence.is_none())
        .ok_or_else(|| {
            if is.schema_by_name(&schema).is_none() {
                Error::new(ERR_BAD_DB, format!("Unknown database '{schema}'"))
            } else {
                Error::new(
                    ERR_NO_SUCH_TABLE,
                    format!("Table '{schema}.{table}' doesn't exist"),
                )
            }
        })?;
    clear_table_keys(storage, &tbl.physical_ids())?;
    meta.reset_handle(tbl.id);
    meta.gen_schema_version()?;
    Ok(())
}

/// Remove every row/index key of the given physical table ids.
pub fn clear_table_keys(storage: &dyn Storage, physical_ids: &[i64]) -> Result<()> {
    if physical_ids.is_empty() {
        return Ok(());
    }
    let mut txn = storage.begin()?;
    for id in physical_ids {
        let start = encode_table_prefix(*id);
        let end = next_prefix(start.as_bytes());
        let pairs = txn.scan(start.as_bytes(), &end)?;
        for (key, _) in pairs {
            txn.delete(&key)?;
        }
    }
    txn.commit()
}

pub(crate) fn resolve_name(name: &TableName, current_db: &str) -> Result<(String, String)> {
    let (schema, table) = name.qualified(current_db);
    if schema.is_empty() {
        return Err(Error::new(ERR_NO_DB, "No database selected"));
    }
    Ok((schema, table))
}
