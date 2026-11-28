use crate::create_table::{
    affinity_setting, build_table_info_with_defaults, build_table_info_with_like,
    columns_from_select, referenced_by_foreign_key, resolve_qualified_column_name, validate_ttl,
};
use sealdb_errno::{
    ERR_BAD_DB, ERR_BAD_TABLE, ERR_CANNOT_DROP_TABLE_REFERENCED_BY_FK, ERR_NO_DB,
    ERR_NO_SUCH_TABLE, ERR_TABLE_EXISTS, ERR_TTL_FK_REFERENCED, ERR_UNKNOWN_ERROR,
    ERR_WRONG_OBJECT, Error, Result,
};
use sealdb_infoschema::InfoSchema;
use sealdb_kv::{Storage, Transaction};
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
        // TiDB `checkContainDotColumn`: `[db.]table.column` must name this table.
        for col in &mut built.cols {
            col.name = resolve_qualified_column_name(&col.name, &schema, &table)?;
        }
        let mut info = build_table_info_with_defaults(
            &built,
            table_id,
            Some((db.charset.clone(), db.collate.clone())),
        )?;
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
            // TiDB 1562: temporary tables cannot be partitioned.
            if built.temporary || built.on_commit_delete_rows {
                return Err(Error::new(
                    1562,
                    "Cannot create temporary table with partitions",
                ));
            }
            crate::partition::attach_partitions(meta, &mut info, spec)?;
        }
        info.temporary = built.temporary || built.on_commit_delete_rows;
        info.on_commit_delete_rows = built.on_commit_delete_rows;
        if let Some(raw) = &built.affinity {
            info.affinity = affinity_setting(raw, info.temporary, info.partition.is_some())?;
        }
        info
    };
    // TiDB `checkGlobalIndexes`: a GLOBAL index only makes sense on a
    // partitioned table (8200).
    if info.partition.is_none() && info.indices.iter().any(|idx| idx.global) {
        return Err(Error::new(
            sealdb_errno::ERR_UNSUPPORTED_DDL_OPERATION,
            "Unsupported Global Index on non-partitioned table",
        ));
    }
    // TiDB builds the table info (validating generated columns / expression
    // indexes) before the DDL job checks that the name is free, so a bad
    // definition wins over "already exists".
    if let Some(existing) = is.table_by_name(&schema, &table) {
        if stmt.if_not_exists {
            return Ok(existing.clone());
        }
        return Err(Error::new(
            ERR_TABLE_EXISTS,
            format!("Table '{schema}.{table}' already exists"),
        ));
    }
    meta.create_table_or_view(db.id, &info)?;
    meta.gen_schema_version()?;
    Ok(info)
}

pub fn drop_table(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &DropTableStmt,
    current_db: &str,
    fk_checks: bool,
) -> Result<()> {
    // Phase 1 validates the whole statement; a failure must leave every table
    // in place (`DROP TABLE a, b` is all-or-nothing).
    let mut drops: Vec<(String, String)> = Vec::new();
    let mut warnings: Vec<(i32, String)> = Vec::new();
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
            (Some(_), Some(tbl)) => {
                if stmt.is_view && !tbl.is_view() {
                    return Err(Error::new(
                        ERR_WRONG_OBJECT,
                        format!("'{schema}.{table}' is not VIEW"),
                    ));
                }
                // TiDB resolves `DROP TABLE` against base tables only; a view
                // name is an unknown table (1051), suppressed by IF EXISTS.
                if !stmt.is_view && tbl.is_view() {
                    if stmt.if_exists {
                        warnings.push((ERR_BAD_TABLE, format!("Unknown table '{schema}.{table}'")));
                        continue;
                    }
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
                drops.push((schema, table));
            }
            _ if stmt.if_exists => {
                // MySQL reports a note per missing object for
                // `DROP {TABLE|VIEW} IF EXISTS` (1051).
                if is.schema_by_name(&schema).is_some() {
                    warnings.push((ERR_BAD_TABLE, format!("Unknown table '{schema}.{table}'")));
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
    // TiDB `checkTableHasForeignKeyReferred` (3730): a table another table's
    // foreign key points at cannot go unless the child goes in this statement.
    if fk_checks {
        check_drop_tables_referenced_by_fk(is, &drops)?;
    }
    for (code, message) in warnings {
        sealdb_expression::note_warning(code, message);
    }
    // Phase 2 applies the drops (the info schema is unchanged since phase 1).
    for (schema, table) in &drops {
        let Some(db) = is.schema_by_name(schema) else {
            continue;
        };
        let Some(tbl) = is.table_by_name(schema, table) else {
            continue;
        };
        meta.drop_table_or_view(db.id, tbl.id)?;
        meta.gen_schema_version()?;
    }
    Ok(())
}

/// TiDB `checkTableHasForeignKeyReferred` (3730): reject dropping a table that
/// a foreign key of some table outside `dropped` still references.
pub(crate) fn check_drop_tables_referenced_by_fk(
    is: &InfoSchema,
    dropped: &[(String, String)],
) -> Result<()> {
    for (schema, table) in dropped {
        for db in is.all_schemas() {
            let child_schema = db.name.o().to_string();
            for child in is.schema_tables(&child_schema) {
                let child_dropped = dropped.iter().any(|(s, t)| {
                    s.eq_ignore_ascii_case(&child_schema) && t.eq_ignore_ascii_case(child.name.o())
                });
                if child_dropped {
                    continue;
                }
                for fk in &child.foreign_keys {
                    if !fk.ref_table.eq_ignore_ascii_case(table) {
                        continue;
                    }
                    if !crate::create_table::fk_ref_schema(&child_schema, fk)
                        .eq_ignore_ascii_case(schema)
                    {
                        continue;
                    }
                    return Err(Error::new(
                        ERR_CANNOT_DROP_TABLE_REFERENCED_BY_FK,
                        format!(
                            "Cannot drop table '{table}' referenced by a foreign key \
                             constraint '{}' on table '{}'.",
                            fk.name,
                            child.name.o()
                        ),
                    ));
                }
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
    // MySQL `TRUNCATE` restarts `AUTO_INCREMENT` from 1 as well.
    meta.reset_auto_id(tbl.id);
    meta.gen_schema_version()?;
    Ok(())
}

/// Remove every row/index key of the given physical table ids.
pub fn clear_table_keys(storage: &dyn Storage, physical_ids: &[i64]) -> Result<()> {
    if physical_ids.is_empty() {
        return Ok(());
    }
    let mut txn = storage.begin()?;
    clear_table_keys_in_txn(txn.as_mut(), physical_ids)?;
    txn.commit()
}

/// Delete every row/index key of `physical_ids` through `txn`, so a statement
/// running inside an explicit transaction discards the rows it has not
/// committed yet (`TRUNCATE` on a session temporary table).
pub fn clear_table_keys_in_txn(txn: &mut dyn Transaction, physical_ids: &[i64]) -> Result<()> {
    for id in physical_ids {
        let start = encode_table_prefix(*id);
        let end = next_prefix(start.as_bytes());
        let pairs = txn.scan(start.as_bytes(), &end)?;
        for (key, _) in pairs {
            txn.delete(&key)?;
        }
    }
    Ok(())
}

pub(crate) fn resolve_name(name: &TableName, current_db: &str) -> Result<(String, String)> {
    let (schema, table) = name.qualified(current_db);
    if schema.is_empty() {
        return Err(Error::new(ERR_NO_DB, "No database selected"));
    }
    Ok((schema, table))
}
