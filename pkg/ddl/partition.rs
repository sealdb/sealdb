// F19 core: build PartitionInfo for RANGE / HASH / LIST / KEY / RANGE COLUMNS.
// Completeness wave / not this feature: ADD/DROP PARTITION, reorganize, job
// states, expression keys.
use sealdb_errno::{ERR_BAD_FIELD, Error, Result};
use sealdb_meta::{Mutator, PartitionDefinition, PartitionInfo, PartitionType, TableInfo};
use sealdb_parser::ast::{PartitionOption, PartitionType as AstPartitionType};

pub fn attach_partitions(
    meta: &mut Mutator,
    info: &mut TableInfo,
    spec: &PartitionOption,
) -> Result<()> {
    let key_cols = if spec.columns.is_empty() {
        vec![spec.column.clone()]
    } else {
        spec.columns.clone()
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
    let tp = match spec.tp {
        AstPartitionType::Range => PartitionType::Range,
        AstPartitionType::Hash => PartitionType::Hash,
        AstPartitionType::List => PartitionType::List,
        AstPartitionType::Key => PartitionType::Key,
    };
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
    info.partition = Some(PartitionInfo::new(tp, spec.column.clone(), defs).with_columns(key_cols));
    Ok(())
}

fn range_bounds(def: &sealdb_parser::ast::PartitionDef) -> Vec<String> {
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
