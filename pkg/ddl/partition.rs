// F19 core: build PartitionInfo for RANGE / HASH / LIST / KEY / RANGE COLUMNS
// and scalar expression keys (`id % 4`, `ABS(id)`), plus ADD/DROP PARTITION.
// Completeness wave / not this feature: reorganize, job states, subpartition.
use crate::table::resolve_name;
use sealdb_errno::{
    ERR_BAD_DB, ERR_BAD_FIELD, ERR_CANT_DROP_FIELD_OR_KEY, ERR_NO_SUCH_TABLE, Error, Result,
};
use sealdb_infoschema::InfoSchema;
use sealdb_kv::Storage;
use sealdb_meta::{Mutator, PartitionDefinition, PartitionInfo, PartitionType, TableInfo};
use sealdb_parser::ast::{
    PartitionDef, PartitionOption, PartitionType as AstPartitionType, TableName,
};
use sealdb_tablecodec::{encode_table_prefix, next_prefix};

pub fn attach_partitions(
    meta: &mut Mutator,
    info: &mut TableInfo,
    spec: &PartitionOption,
) -> Result<()> {
    let key_cols = if !spec.columns.is_empty() {
        spec.columns.clone()
    } else if !spec.column.is_empty() {
        vec![spec.column.clone()]
    } else {
        Vec::new()
    };
    for name in &key_cols {
        if !info
            .columns
            .iter()
            .any(|c| c.name.o().eq_ignore_ascii_case(name))
        {
            return Err(Error::new(
                ERR_BAD_FIELD,
                format!("Unknown column '{name}' in partition key"),
            ));
        }
    }
    if let Some(expr) = &spec.expr
        && key_cols.is_empty()
    {
        return Err(Error::new(
            ERR_BAD_FIELD,
            format!("Unknown column in partition expression '{expr}'"),
        ));
    }
    let tp = match spec.tp {
        AstPartitionType::Range => PartitionType::Range,
        AstPartitionType::Hash => PartitionType::Hash,
        AstPartitionType::List => PartitionType::List,
        AstPartitionType::Key => PartitionType::Key,
    };
    if matches!(tp, PartitionType::Key) && spec.expr.is_some() {
        return Err(Error::unsupported("KEY partition expression"));
    }
    let defs = match tp {
        PartitionType::Hash | PartitionType::Key => {
            let n = if spec.defs.is_empty() {
                spec.num
            } else {
                spec.defs.len() as u64
            };
            let mut defs = Vec::with_capacity(n as usize);
            for i in 0..n {
                let name = spec
                    .defs
                    .get(i as usize)
                    .map(|d| d.name.clone())
                    .unwrap_or_else(|| format!("p{i}"));
                defs.push(PartitionDefinition::new(
                    meta.gen_global_id()?,
                    name,
                    Vec::new(),
                ));
            }
            defs
        }
        PartitionType::Range => {
            let mut defs = Vec::with_capacity(spec.defs.len());
            for def in &spec.defs {
                defs.push(PartitionDefinition::new(
                    meta.gen_global_id()?,
                    def.name.clone(),
                    range_bounds(def),
                ));
            }
            defs
        }
        PartitionType::List => {
            let mut defs = Vec::with_capacity(spec.defs.len());
            for def in &spec.defs {
                defs.push(
                    PartitionDefinition::new(meta.gen_global_id()?, def.name.clone(), Vec::new())
                        .with_in_values(def.in_values.clone()),
                );
            }
            defs
        }
    };
    let label = spec.expr.clone().unwrap_or_else(|| spec.column.clone());
    let mut part = PartitionInfo::new(tp, label, defs);
    if let Some(expr) = &spec.expr {
        // TiDB stores expression keys in `expr` with an empty column list.
        part = part.with_expr_key(expr.clone());
    } else {
        part = part.with_columns(key_cols);
    }
    info.partition = Some(part);
    Ok(())
}

/// `ALTER TABLE t ADD PARTITION (...)` for RANGE/LIST tables.
pub fn add_partitions(
    meta: &mut Mutator,
    is: &InfoSchema,
    table: &TableName,
    defs: &[PartitionDef],
    current_db: &str,
) -> Result<()> {
    let (schema, name) = resolve_name(table, current_db)?;
    let db = is
        .schema_by_name(&schema)
        .ok_or_else(|| Error::new(ERR_BAD_DB, format!("Unknown database '{schema}'")))?;
    let mut info = is.table_by_name(&schema, &name).cloned().ok_or_else(|| {
        Error::new(
            ERR_NO_SUCH_TABLE,
            format!("Table '{schema}.{name}' doesn't exist"),
        )
    })?;
    let Some(part) = info.partition.as_mut() else {
        return Err(Error::unsupported("ADD PARTITION on non-partitioned table"));
    };
    match part.tp {
        PartitionType::Range | PartitionType::List => {}
        other => {
            return Err(Error::unsupported(format!("ADD PARTITION for {other:?}")));
        }
    }
    for def in defs {
        if part
            .definitions
            .iter()
            .any(|d| d.name.o().eq_ignore_ascii_case(&def.name))
        {
            return Err(Error::new(
                ERR_BAD_FIELD,
                format!("Duplicate partition name '{}'", def.name),
            ));
        }
        let built = match part.tp {
            PartitionType::Range => {
                if !def.in_values.is_empty() {
                    return Err(Error::unsupported("RANGE partition VALUES IN"));
                }
                PartitionDefinition::new(meta.gen_global_id()?, def.name.clone(), range_bounds(def))
            }
            PartitionType::List => {
                if def.less_than.is_some() || !def.extra_bounds.is_empty() {
                    return Err(Error::unsupported("LIST partition VALUES LESS THAN"));
                }
                PartitionDefinition::new(meta.gen_global_id()?, def.name.clone(), Vec::new())
                    .with_in_values(def.in_values.clone())
            }
            _ => unreachable!(),
        };
        // Insert before MAXVALUE for RANGE so new bounds stay ordered.
        if part.tp == PartitionType::Range
            && part.definitions.last().is_some_and(|d| d.is_maxvalue())
        {
            let last = part.definitions.len() - 1;
            part.definitions.insert(last, built);
        } else {
            part.definitions.push(built);
        }
    }
    part.num = part.definitions.len() as u64;
    meta.update_table(db.id, &info)?;
    meta.gen_schema_version()?;
    Ok(())
}

/// `ALTER TABLE t DROP PARTITION p0 [, p1 ...]` — drops meta and KV for those ids.
pub fn drop_partitions(
    meta: &mut Mutator,
    is: &InfoSchema,
    table: &TableName,
    names: &[String],
    if_exists: bool,
    current_db: &str,
    storage: &dyn Storage,
) -> Result<()> {
    let (schema, name) = resolve_name(table, current_db)?;
    let db = is
        .schema_by_name(&schema)
        .ok_or_else(|| Error::new(ERR_BAD_DB, format!("Unknown database '{schema}'")))?;
    let mut info = is.table_by_name(&schema, &name).cloned().ok_or_else(|| {
        Error::new(
            ERR_NO_SUCH_TABLE,
            format!("Table '{schema}.{name}' doesn't exist"),
        )
    })?;
    let Some(part) = info.partition.as_mut() else {
        if if_exists {
            return Ok(());
        }
        return Err(Error::unsupported(
            "DROP PARTITION on non-partitioned table",
        ));
    };
    let mut dropped_ids = Vec::new();
    for pname in names {
        let Some(pos) = part
            .definitions
            .iter()
            .position(|d| d.name.o().eq_ignore_ascii_case(pname))
        else {
            if if_exists {
                continue;
            }
            return Err(Error::new(
                ERR_CANT_DROP_FIELD_OR_KEY,
                format!("Unknown partition '{pname}'"),
            ));
        };
        if part.definitions.len() == 1 {
            return Err(Error::unsupported("DROP last partition"));
        }
        let removed = part.definitions.remove(pos);
        dropped_ids.push(removed.id);
    }
    part.num = part.definitions.len() as u64;
    meta.update_table(db.id, &info)?;
    meta.gen_schema_version()?;
    drop_partition_keys(storage, &dropped_ids)?;
    Ok(())
}

fn drop_partition_keys(storage: &dyn Storage, physical_ids: &[i64]) -> Result<()> {
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

fn range_bounds(def: &PartitionDef) -> Vec<String> {
    let mut out = vec![match def.less_than {
        Some(v) => v.to_string(),
        None => "MAXVALUE".to_string(),
    }];
    for bound in &def.extra_bounds {
        out.push(match bound {
            Some(v) => v.to_string(),
            None => "MAXVALUE".to_string(),
        });
    }
    out
}
