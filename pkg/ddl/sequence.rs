use crate::table::resolve_name;
use sealdb_errno::{
    ERR_BAD_DB, ERR_NO_SUCH_TABLE, ERR_TABLE_EXISTS, ERR_WRONG_OBJECT, Error, Result,
};
use sealdb_infoschema::InfoSchema;
use sealdb_meta::{Mutator, SequenceInfo, TableInfo};
use sealdb_parser::ast::{
    AlterSequenceStmt, CreateSequenceStmt, DropSequenceStmt, SequenceOptionSet,
};

pub fn create_sequence(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &CreateSequenceStmt,
    current_db: &str,
) -> Result<TableInfo> {
    let (schema, name) = resolve_name(&stmt.name, current_db)?;
    if let Some(existing) = is.table_by_name(&schema, &name) {
        if stmt.if_not_exists {
            return Ok(existing.clone());
        }
        return Err(Error::new(
            ERR_TABLE_EXISTS,
            format!("Table '{schema}.{name}' already exists"),
        ));
    }
    let db = is
        .schema_by_name(&schema)
        .ok_or_else(|| Error::new(ERR_BAD_DB, format!("Unknown database '{schema}'")))?;
    let table_id = meta.gen_global_id()?;
    let seq = build_sequence_info(&stmt.options)?;
    // Sequences are stored as TableInfo with a synthetic INT column for I_S.
    let mut info = TableInfo::new(
        table_id,
        name,
        vec![sealdb_meta::ColumnInfo::new(
            1,
            0,
            "id",
            sealdb_types::FieldType::new_longlong(),
        )],
    );
    info.sequence = Some(seq);
    meta.create_table_or_view(db.id, &info)?;
    meta.gen_schema_version()?;
    Ok(info)
}

pub fn drop_sequence(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &DropSequenceStmt,
    current_db: &str,
) -> Result<()> {
    for name in &stmt.sequences {
        let (schema, table) = match resolve_name(name, current_db) {
            Ok(pair) => pair,
            Err(err) if stmt.if_exists && err.code == sealdb_errno::ERR_NO_DB => continue,
            Err(err) => return Err(err),
        };
        match (
            is.schema_by_name(&schema),
            is.table_by_name(&schema, &table),
        ) {
            (Some(db), Some(tbl)) => {
                if !tbl.is_sequence() {
                    return Err(Error::new(
                        ERR_WRONG_OBJECT,
                        format!("'{schema}.{table}' is not SEQUENCE"),
                    ));
                }
                meta.drop_table_or_view(db.id, tbl.id)?;
                meta.gen_schema_version()?;
            }
            _ if stmt.if_exists => {}
            (None, _) => {
                return Err(Error::new(
                    ERR_BAD_DB,
                    format!("Unknown database '{schema}'"),
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

pub fn alter_sequence(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &AlterSequenceStmt,
    current_db: &str,
) -> Result<()> {
    let (schema, name) = resolve_name(&stmt.name, current_db)?;
    let Some(db) = is.schema_by_name(&schema) else {
        if stmt.if_exists {
            return Ok(());
        }
        return Err(Error::new(
            ERR_BAD_DB,
            format!("Unknown database '{schema}'"),
        ));
    };
    let Some(existing) = is.table_by_name(&schema, &name) else {
        if stmt.if_exists {
            return Ok(());
        }
        return Err(Error::new(
            ERR_NO_SUCH_TABLE,
            format!("Table '{schema}.{name}' doesn't exist"),
        ));
    };
    if !existing.is_sequence() {
        return Err(Error::new(
            ERR_WRONG_OBJECT,
            format!("'{schema}.{name}' is not SEQUENCE"),
        ));
    }
    let mut info = existing.clone();
    let seq = info.sequence.as_mut().expect("is_sequence");
    apply_options(seq, &stmt.options, true)?;
    meta.update_table(db.id, &info)?;
    meta.gen_schema_version()?;
    Ok(())
}

fn build_sequence_info(opts: &SequenceOptionSet) -> Result<SequenceInfo> {
    let increment = opts.increment.unwrap_or(1);
    let mut seq = SequenceInfo::with_defaults_for_increment(increment);
    apply_options(&mut seq, opts, false)?;
    seq.next_value = seq.start;
    Ok(seq)
}

fn apply_options(seq: &mut SequenceInfo, opts: &SequenceOptionSet, is_alter: bool) -> Result<()> {
    if let Some(v) = opts.increment {
        if v == 0 {
            return Err(Error::unsupported("SEQUENCE INCREMENT cannot be 0"));
        }
        seq.increment = v;
    }
    if opts.no_min {
        seq.min_value = if seq.increment > 0 {
            1
        } else {
            -9223372036854775807
        };
    } else if let Some(v) = opts.min_value {
        seq.min_value = v;
    }
    if opts.no_max {
        seq.max_value = if seq.increment > 0 {
            9223372036854775806
        } else {
            -1
        };
    } else if let Some(v) = opts.max_value {
        seq.max_value = v;
    }
    if let Some(v) = opts.start {
        if v == i64::MIN && is_alter {
            // RESTART without WITH → original start.
            seq.next_value = seq.start;
        } else {
            seq.start = v;
            if is_alter {
                seq.next_value = v;
            }
        }
    }
    if opts.no_cache {
        seq.cache = false;
        seq.cache_value = 0;
    } else if let Some(v) = opts.cache {
        if v <= 0 {
            return Err(Error::unsupported("SEQUENCE CACHE must be positive"));
        }
        seq.cache = true;
        seq.cache_value = v;
    }
    if let Some(cycle) = opts.cycle {
        seq.cycle = cycle;
    }
    Ok(())
}
