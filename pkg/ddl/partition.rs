// build PartitionInfo for RANGE / HASH / LIST / KEY / RANGE COLUMNS
// and scalar expression keys (`id % 4`, `ABS(id)`), plus ADD/DROP PARTITION.
// SUBPARTITION BY under RANGE/LIST is accepted then stripped (TiDB parity);
// under HASH/KEY it is rejected. Completeness wave: reorganize, job states.
use crate::list_partition::{
    ERR_SAME_NAME_PARTITION, check_reorganize_coverage, convert_list_values,
    reject_add_when_default_exists, validate_list_defs,
};
use crate::table::resolve_name;
use sealdb_errno::{
    ERR_BAD_DB, ERR_BAD_FIELD, ERR_CANT_DROP_FIELD_OR_KEY, ERR_NO_SUCH_TABLE,
    ERR_UNSUPPORTED_DDL_OPERATION, ERR_WRONG_NAME_FOR_INDEX, ERR_WRONG_PARTITION_NAME, Error,
    Result,
};
use sealdb_infoschema::InfoSchema;
use sealdb_kv::Storage;
use sealdb_meta::{
    ListInEntry, Mutator, PartitionDefinition, PartitionInfo, PartitionType, TableInfo,
};
use sealdb_parser::ast::{
    IndexUpdate, PartitionDef, PartitionOption, PartitionType as AstPartitionType, TableName,
};
use sealdb_tablecodec::{
    decode_handle_from_row_key, decode_row, encode_row_key_with_handle, encode_table_prefix,
    gen_table_record_prefix, next_prefix,
};
use sealdb_types::Datum;

/// TiDB evaluates constant expressions in `VALUES LESS THAN (…)` at CREATE
/// time (`unix_timestamp('…')`, `extract(year from '…')`) under the session
/// time zone and stores the resulting number as the bound.
fn evaluate_constant_bounds(defs: &mut [PartitionDef]) {
    for def in defs {
        let Some(bound) = def.less_than.as_mut() else {
            continue;
        };
        let text = bound.trim().to_string();
        if text.is_empty()
            || text.eq_ignore_ascii_case("MAXVALUE")
            || text.starts_with('\'')
            || text.starts_with('"')
            || text.parse::<i64>().is_ok()
            || text.parse::<u64>().is_ok()
        {
            continue;
        }
        let Ok(expr) = sealdb_parser::sqlparser_bridge::lower::parse_expr(&text) else {
            // `EXTRACT(YEAR FROM '…')` is not lowered yet; fold the common
            // constant form used by RANGE bounds (`… LIKE 1998`).
            if let Some(year) = fold_extract_year(&text) {
                *bound = year.to_string();
            }
            continue;
        };
        let Ok(datum) = sealdb_expression::eval_expr(&expr) else {
            continue;
        };
        if datum.is_null() {
            continue;
        }
        if let Ok(value) = datum.as_i64() {
            *bound = value.to_string();
        }
    }
}

/// `EXTRACT(YEAR FROM '1998-11-23')` → `1998` for a constant RANGE bound.
fn fold_extract_year(text: &str) -> Option<i64> {
    let lower = text.trim().to_ascii_lowercase();
    let inner = lower.strip_prefix("extract(")?.strip_suffix(')')?;
    let (unit, operand) = inner.split_once(" from ")?;
    if !unit.trim().eq_ignore_ascii_case("year") {
        return None;
    }
    let operand = operand.trim();
    let value = operand
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .or_else(|| operand.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
        .unwrap_or(operand);
    let digits: String = value.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Evaluate an INTERVAL FIRST/LAST bound that is a constant expression
/// (`unix_timestamp('…')`) into its integer text.
fn evaluate_interval_bound(bound: &str) -> String {
    let text = bound.trim();
    if text.is_empty()
        || text.eq_ignore_ascii_case("NULL")
        || text.parse::<i64>().is_ok()
        || text.starts_with('\'')
        || text.starts_with('"')
    {
        return text.to_string();
    }
    let Ok(expr) = sealdb_parser::sqlparser_bridge::lower::parse_expr(text) else {
        return text.to_string();
    };
    match sealdb_expression::eval_expr(&expr) {
        Ok(datum) if !datum.is_null() => datum
            .as_i64()
            .map(|v| v.to_string())
            .unwrap_or_else(|_| text.to_string()),
        _ => text.to_string(),
    }
}

/// TiDB `util.IsInCorrectIdentifierName` for partition names: an empty name or
/// one whose last byte is a space is rejected with 1567 before any DDL work.
fn validate_partition_names(defs: &[PartitionDef]) -> Result<()> {
    for def in defs {
        if def.name.is_empty() || def.name.ends_with(' ') {
            return Err(Error::new(
                ERR_WRONG_PARTITION_NAME,
                "Incorrect partition name",
            ));
        }
    }
    Ok(())
}

pub fn attach_partitions(
    meta: &mut Mutator,
    info: &mut TableInfo,
    spec: &PartitionOption,
) -> Result<()> {
    validate_partition_names(&spec.defs)?;
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
                format!("Unknown column '{name}' in 'partition function'"),
            ));
        }
    }
    // Constant partition expressions (`RANGE (4)`) have no columns; TiDB 1486
    // rejects them after parse. Non-constant exprs with zero columns stay 1054.
    if let Some(expr) = &spec.expr
        && key_cols.is_empty()
    {
        let compact: String = expr
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect::<String>()
            .to_ascii_lowercase();
        let is_const = compact.parse::<i64>().is_ok() || compact.parse::<f64>().is_ok();
        if !is_const {
            return Err(Error::new(
                ERR_BAD_FIELD,
                format!("Unknown column in partition expression '{expr}'"),
            ));
        }
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
    // TiDB strips SUBPARTITION under RANGE/LIST; HASH/KEY + SUBPARTITION is illegal.
    if spec.subpartition_ignored && matches!(tp, PartitionType::Hash | PartitionType::Key) {
        return Err(Error::new(
            1500,
            "It is only possible to mix RANGE/LIST partitioning with HASH/KEY partitioning for subpartitioning",
        ));
    }
    validate_partition_expr(info, spec)?;
    // TiDB 1659: TIMESTAMP/TIME/FLOAT/DOUBLE/DECIMAL are not allowed as
    // RANGE COLUMNS keys (INT/DATE/DATETIME/VARCHAR are).
    if spec.range_columns {
        use sealdb_types::field_type::{
            TYPE_DECIMAL, TYPE_DOUBLE, TYPE_DURATION, TYPE_FLOAT, TYPE_NEW_DECIMAL, TYPE_TIMESTAMP,
        };
        for name in key_cols.iter().chain(std::iter::once(&spec.column)) {
            if name.is_empty() {
                continue;
            }
            if let Some(col) = info
                .columns
                .iter()
                .find(|c| c.name.o().eq_ignore_ascii_case(name))
            {
                if matches!(
                    col.field_type.tp,
                    TYPE_TIMESTAMP
                        | TYPE_DURATION
                        | TYPE_FLOAT
                        | TYPE_DOUBLE
                        | TYPE_DECIMAL
                        | TYPE_NEW_DECIMAL
                ) {
                    return Err(Error::new(
                        1659,
                        format!(
                            "Field '{}' is of a not allowed type for this type of partitioning",
                            col.name.o()
                        ),
                    ));
                }
            }
        }
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
            let mut range_defs = spec.defs.clone();
            if range_defs.is_empty() && spec.interval_expr.is_some() {
                // FIRST/LAST may be constant expressions (`unix_timestamp(…)`);
                // re-expand once they are evaluated under the session zone.
                let first = evaluate_interval_bound(spec.interval_first.as_deref().unwrap_or(""));
                let last = evaluate_interval_bound(spec.interval_last.as_deref().unwrap_or(""));
                if let Ok(expanded) = sealdb_parser::sqlparser_bridge::lower::expand_interval_defs(
                    spec.interval_expr.as_deref().unwrap_or(""),
                    &first,
                    &last,
                    spec.interval_null,
                    spec.interval_maxvalue,
                ) {
                    range_defs = expanded;
                }
            }
            evaluate_constant_bounds(&mut range_defs);
            rewrite_interval_null_bound(&mut range_defs, info, &key_cols, spec.range_columns);
            if spec.range_columns {
                validate_range_columns_create_bounds(&range_defs, info, &key_cols)?;
            } else {
                validate_range_int_bounds(&range_defs, info, &key_cols)?;
            }
            if range_defs.is_empty() && spec.interval_expr.is_some() {
                return Err(Error::new(
                    8200,
                    spec.interval_error.clone().unwrap_or_else(|| {
                        "Unsupported INTERVAL partitioning: Error when generating partition values"
                            .to_string()
                    }),
                ));
            }
            validate_range_create_increasing(&range_defs, key_columns_ci(info, &key_cols))?;
            let mut defs = Vec::with_capacity(range_defs.len());
            for def in &range_defs {
                defs.push(PartitionDefinition::new(
                    meta.gen_global_id()?,
                    def.name.clone(),
                    range_bounds(def, info, &key_cols, spec.range_columns),
                ));
            }
            defs
        }
        PartitionType::List => {
            let ncols = key_cols.len().max(1);
            validate_list_defs(&spec.defs, info, &key_cols, spec.range_columns, ncols, &[])?;
            let mut defs = Vec::with_capacity(spec.defs.len());
            for def in &spec.defs {
                let values = convert_list_values(def, info, &key_cols, spec.range_columns)?;
                defs.push(
                    PartitionDefinition::new(meta.gen_global_id()?, def.name.clone(), Vec::new())
                        .with_in_values(values)
                        .with_list_default(def.list_default),
                );
            }
            defs
        }
    };
    let label = spec.expr.clone().unwrap_or_else(|| spec.column.clone());
    let mut part = PartitionInfo::new(tp, label, defs);
    // `PARTITION BY HASH/KEY (…) PARTITIONS n`: SHOW CREATE keeps the shorthand.
    part.uniform = matches!(tp, PartitionType::Hash | PartitionType::Key) && spec.defs.is_empty();
    part.range_columns =
        spec.range_columns && matches!(tp, PartitionType::Range | PartitionType::List);
    // Range/LIST bounds and LIST literal matching follow the key column's
    // collation: a `*_ci` collation compares case-insensitively (`A` < `aa` <
    // `AAA` under utf8mb4_general_ci, but not under utf8mb4_bin).
    part.collation_ci = key_columns_ci(info, &key_cols);
    if let Some(ie) = &spec.interval_expr {
        part.interval_expr = ie.clone();
        part.interval_maxvalue = spec.interval_maxvalue;
    }
    if let Some(expr) = &spec.expr {
        // TiDB stores expression keys in `expr` with an empty column list.
        part = part.with_expr_key(expr.clone());
    } else {
        part = part.with_columns(key_cols);
    }
    info.partition = Some(part);
    validate_unique_indexes_cover_partition_key(info)?;
    Ok(())
}

/// TiDB 1486: constants / RAND / timezone-dependent extractors are illegal in
/// a (sub)partitioning function.
fn validate_partition_expr(info: &TableInfo, spec: &PartitionOption) -> Result<()> {
    const ERR_PARTITION_CONST: i32 = 1486;
    let err = || {
        Error::new(
            ERR_PARTITION_CONST,
            "Constant, random or timezone-dependent expressions in (sub)partitioning function are not allowed",
        )
    };
    let Some(expr) = &spec.expr else {
        return Ok(());
    };
    let compact: String = expr
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    // Bare constant (`PARTITION BY RANGE (4)` / `HASH (4)`).
    if compact.parse::<i64>().is_ok() || compact.parse::<f64>().is_ok() {
        return Err(err());
    }
    for banned in [
        "rand(",
        "now(",
        "sysdate(",
        "current_timestamp",
        "current_date",
        "current_time",
        "utc_timestamp",
        "utc_date",
        "utc_time",
        "curdate(",
        "curtime(",
        "localtime",
        "localtimestamp",
    ] {
        if compact.contains(banned) {
            return Err(err());
        }
    }
    // Extractors that are timezone-dependent on TIMESTAMP/TIME, or illegal on
    // non-DATE columns (`DAY(id)`, `TO_SECONDS(id)`).
    for func in [
        "to_days(",
        "to_seconds(",
        "day(",
        "hour(",
        "minute(",
        "second(",
    ] {
        if !compact.contains(func) {
            continue;
        }
        // Identify the column argument when it is a bare identifier.
        let after = &compact[compact.find(func).unwrap() + func.len()..];
        let arg: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '`')
            .collect::<String>()
            .trim_matches('`')
            .to_string();
        let col = info
            .columns
            .iter()
            .find(|c| c.name.o().eq_ignore_ascii_case(&arg));
        let tp = col.map(|c| c.field_type.tp).unwrap_or(0);
        use sealdb_types::field_type::{TYPE_DATE, TYPE_DATETIME};
        // DATE and DATETIME are OK for TO_DAYS/DAY/…; TIMESTAMP/TIME/INT/other
        // are not (TiDB rejects them as timezone-dependent or ill-typed).
        if tp != TYPE_DATE && tp != TYPE_DATETIME {
            return Err(err());
        }
    }
    // `ABS(timestamp)` is timezone-/type-illegal; ABS on integers is fine.
    if let Some(idx) = compact.find("abs(") {
        let after = &compact[idx + 4..];
        let arg: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '`')
            .collect::<String>()
            .trim_matches('`')
            .to_string();
        if let Some(col) = info
            .columns
            .iter()
            .find(|c| c.name.o().eq_ignore_ascii_case(&arg))
        {
            use sealdb_types::field_type::{
                TYPE_DATE, TYPE_DATETIME, TYPE_DURATION, TYPE_TIMESTAMP,
            };
            if matches!(
                col.field_type.tp,
                TYPE_TIMESTAMP | TYPE_DATETIME | TYPE_DATE | TYPE_DURATION
            ) {
                return Err(err());
            }
        }
    }
    Ok(())
}

/// TiDB 8264: a UNIQUE/PRIMARY index on a partitioned table must include every
/// partitioning column as a full (non-prefix) key part, or be declared GLOBAL.
pub fn validate_unique_indexes_cover_partition_key(info: &TableInfo) -> Result<()> {
    let Some(part) = &info.partition else {
        return Ok(());
    };
    let part_cols: Vec<String> = if !part.columns.is_empty() {
        part.columns
            .iter()
            .map(|c| c.o().to_ascii_lowercase())
            .collect()
    } else {
        // Expression key: every table column referenced by the expression.
        let expr = part.expr.to_ascii_lowercase();
        info.columns
            .iter()
            .filter(|c| !c.hidden)
            .filter(|c| {
                let name = c.name.o().to_ascii_lowercase();
                expr.split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
                    .any(|tok| tok == name)
            })
            .map(|c| c.name.o().to_ascii_lowercase())
            .collect()
    };
    if part_cols.is_empty() {
        return Ok(());
    }
    for idx in &info.indices {
        if !idx.unique && !idx.primary {
            continue;
        }
        if idx.global {
            continue;
        }
        // Prefix key parts (`a(5)`) do not count as covering a partition column.
        let covers = part_cols.iter().all(|pc| {
            idx.columns
                .iter()
                .any(|ic| ic.length < 0 && ic.name.o().eq_ignore_ascii_case(pc))
        });
        if !covers {
            let name = if idx.primary {
                "PRIMARY".to_string()
            } else {
                idx.name.o().to_string()
            };
            return Err(Error::new(
                8264,
                format!(
                    "Global Index is needed for index '{name}', since the unique index is not including all partitioning columns, and GLOBAL is not given as IndexOption"
                ),
            ));
        }
    }
    Ok(())
}

/// `ALTER TABLE t ADD PARTITION (...)` / `ADD PARTITION PARTITIONS n`.
pub fn add_partitions(
    meta: &mut Mutator,
    is: &InfoSchema,
    table: &TableName,
    defs: &[PartitionDef],
    count: Option<u64>,
    current_db: &str,
    storage: &dyn Storage,
) -> Result<()> {
    // TiDB preprocess `checkAlterTableGrammar` rejects bad partition names
    // before the table is even resolved (1567).
    validate_partition_names(defs)?;
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
    let part_tp = info.partition.as_ref().map(|p| p.tp).ok_or_else(|| {
        Error::new(
            1505,
            "Partition management on a not partitioned table is not possible",
        )
    })?;
    // Wrong VALUES clause for the table type → 1480 (before HASH/KEY add).
    for def in defs {
        if def.values_history {
            return Err(Error::new(
                1480,
                "Only SYSTEM_TIME PARTITIONING can use VALUES HISTORY in partition definition",
            ));
        }
        if (!def.in_values.is_empty() || def.list_default) && part_tp != PartitionType::List {
            return Err(Error::new(
                1480,
                "Only LIST PARTITIONING can use VALUES IN in partition definition",
            ));
        }
        // VALUES LESS THAN: missing_bound=false and not a LIST/HISTORY form.
        let is_less_than = !def.missing_bound
            && !def.values_history
            && !def.list_default
            && def.in_values.is_empty();
        if is_less_than && part_tp != PartitionType::Range {
            return Err(Error::new(
                1480,
                "Only RANGE PARTITIONING can use VALUES LESS THAN in partition definition",
            ));
        }
    }
    if matches!(part_tp, PartitionType::Hash | PartitionType::Key) {
        return add_hash_key_partitions(meta, db.id, &mut info, defs, count, storage);
    }
    match part_tp {
        PartitionType::Range | PartitionType::List => {}
        other => {
            return Err(Error::unsupported(format!("ADD PARTITION for {other:?}")));
        }
    }
    if defs.is_empty() {
        let kind = if part_tp == PartitionType::List {
            "LIST"
        } else {
            "RANGE"
        };
        return Err(Error::new(
            1492,
            format!("For {kind} partitions each partition must be defined"),
        ));
    }
    let (key_cols, range_columns, existing_defs) = {
        let part = info.partition.as_ref().unwrap();
        (
            part.columns
                .iter()
                .map(|c| c.o().to_string())
                .collect::<Vec<_>>(),
            part.range_columns,
            part.definitions.clone(),
        )
    };
    // Name clashes before bound/value validation (TiDB 1517 before 1493/1495).
    for def in defs {
        if existing_defs
            .iter()
            .any(|d| d.name.o().eq_ignore_ascii_case(&def.name))
            || defs
                .iter()
                .filter(|d| d.name.eq_ignore_ascii_case(&def.name))
                .count()
                > 1
        {
            return Err(Error::new(
                ERR_SAME_NAME_PARTITION,
                format!("Duplicate partition name {}", def.name),
            ));
        }
    }
    if part_tp == PartitionType::Range {
        if existing_defs.last().is_some_and(|d| d.is_maxvalue()) {
            return Err(Error::new(
                1481,
                "MAXVALUE can only be used in last partition definition",
            ));
        }
        if range_columns {
            validate_range_columns_add_bounds(defs, &info, &key_cols)?;
        }
        // New RANGE bounds must be strictly increasing vs the current last bound.
        if let Some(last) = existing_defs.last() {
            let strip = |s: &str| -> String {
                let b = s.as_bytes();
                if b.len() >= 2
                    && ((b[0] == b'\'' && b[b.len() - 1] == b'\'')
                        || (b[0] == b'"' && b[b.len() - 1] == b'"'))
                {
                    s[1..s.len() - 1].to_string()
                } else {
                    s.to_string()
                }
            };
            let mut prev_num = last.bound();
            let mut prev_str = last.less_than.first().map(|s| strip(s));
            for def in defs {
                if def
                    .less_than
                    .as_ref()
                    .is_some_and(|v| v.eq_ignore_ascii_case("MAXVALUE"))
                    || def.less_than.is_none()
                {
                    prev_num = None;
                    prev_str = None;
                    continue;
                }
                let Some(cur_raw) = def.less_than.as_ref() else {
                    continue;
                };
                let cur_s = strip(cur_raw);
                if let Ok(cur) = cur_s.parse::<i64>() {
                    if prev_num.is_some_and(|p| cur <= p) {
                        return Err(Error::new(
                            1493,
                            "VALUES LESS THAN value must be strictly increasing for each partition",
                        ));
                    }
                    prev_num = Some(cur);
                    prev_str = Some(cur_s);
                } else if prev_str.as_ref().is_some_and(|p| cur_s <= *p) {
                    // RANGE COLUMNS string / datetime lexicographic order.
                    return Err(Error::new(
                        1493,
                        "VALUES LESS THAN value must be strictly increasing for each partition",
                    ));
                } else {
                    prev_str = Some(cur_s);
                    prev_num = None;
                }
            }
        }
    }
    if part_tp == PartitionType::List {
        reject_add_when_default_exists(&existing_defs)?;
        let ncols = key_cols.len().max(1);
        validate_list_defs(defs, &info, &key_cols, range_columns, ncols, &existing_defs)?;
    }
    let mut built_defs = Vec::with_capacity(defs.len());
    for def in defs {
        let built = match part_tp {
            PartitionType::Range => {
                if !def.in_values.is_empty() || def.list_default {
                    return Err(Error::new(
                        1480,
                        "Only LIST PARTITIONING can use VALUES IN in partition definition",
                    ));
                }
                if def.missing_bound {
                    return Err(Error::new(
                        1479,
                        "Syntax : RANGE PARTITIONING requires definition of VALUES LESS THAN for each partition",
                    ));
                }
                PartitionDefinition::new(
                    meta.gen_global_id()?,
                    def.name.clone(),
                    range_bounds(def, &info, &key_cols, range_columns),
                )
            }
            PartitionType::List => {
                let values = convert_list_values(def, &info, &key_cols, range_columns)?;
                PartitionDefinition::new(meta.gen_global_id()?, def.name.clone(), Vec::new())
                    .with_in_values(values)
                    .with_list_default(def.list_default)
            }
            _ => unreachable!(),
        };
        built_defs.push(built);
    }
    let part = info.partition.as_mut().unwrap();
    for built in built_defs {
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
        return Err(Error::new(
            1505,
            "Partition management on a not partitioned table is not possible",
        ));
    };
    if !matches!(part.tp, PartitionType::Range | PartitionType::List) {
        return Err(Error::new(
            1512,
            "DROP PARTITION can only be used on RANGE/LIST partitions",
        ));
    }
    // TiDB: a DROP list at least as long as the partition count is 1508
    // even when names repeat or include unknowns (`p1,p1,p1` / `p1,p9,p1`).
    if !if_exists && names.len() >= part.definitions.len() {
        return Err(Error::new(
            1508,
            "Cannot remove all partitions, use DROP TABLE instead",
        ));
    }
    // Duplicate names in a shorter list are 1507 (TiDB `p1,p1` with 3 parts).
    let mut unique = Vec::new();
    for pname in names {
        if unique
            .iter()
            .any(|u: &String| u.eq_ignore_ascii_case(pname))
        {
            if !if_exists {
                return Err(Error::new(1507, "Error in list of partitions to DROP"));
            }
            continue;
        }
        unique.push(pname.clone());
    }
    if !if_exists && unique.len() >= part.definitions.len() {
        return Err(Error::new(
            1508,
            "Cannot remove all partitions, use DROP TABLE instead",
        ));
    }
    let mut dropped_ids = Vec::new();
    for pname in &unique {
        let Some(pos) = part
            .definitions
            .iter()
            .position(|d| d.name.o().eq_ignore_ascii_case(pname))
        else {
            if if_exists {
                continue;
            }
            // MySQL ER_DROP_PARTITION_NON_EXISTENT (1507).
            return Err(Error::new(1507, "Error in list of partitions to DROP"));
        };
        if part.definitions.len() == 1 {
            return Err(Error::new(
                1508,
                "Cannot remove all partitions, use DROP TABLE instead",
            ));
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

/// `ALTER TABLE t TRUNCATE PARTITION p[, ...]`: drop every row owned by the
/// named partitions while keeping the partition definitions and metadata.
pub fn truncate_partitions(
    is: &InfoSchema,
    table: &TableName,
    names: &[String],
    current_db: &str,
    storage: &dyn Storage,
) -> Result<()> {
    let (schema, name) = resolve_name(table, current_db)?;
    let info = is.table_by_name(&schema, &name).cloned().ok_or_else(|| {
        Error::new(
            ERR_NO_SUCH_TABLE,
            format!("Table '{schema}.{name}' doesn't exist"),
        )
    })?;
    let Some(part) = info.partition.as_ref() else {
        return Err(Error::unsupported(
            "TRUNCATE PARTITION on non-partitioned table",
        ));
    };
    let mut ids = Vec::new();
    // `TRUNCATE PARTITION ALL` empties every partition of the table.
    let all = names.len() == 1 && names[0].eq_ignore_ascii_case("all");
    for pname in names {
        if all {
            ids.extend(part.definitions.iter().map(|d| d.id));
            break;
        }
        let Some(def) = part
            .definitions
            .iter()
            .find(|d| d.name.o().eq_ignore_ascii_case(pname))
        else {
            return Err(Error::new(
                ERR_CANT_DROP_FIELD_OR_KEY,
                format!("Unknown partition '{pname}'"),
            ));
        };
        ids.push(def.id);
    }
    drop_partition_keys(storage, &ids)
}

/// `ALTER TABLE t EXCHANGE PARTITION p WITH TABLE t1`. Each partition and
/// non-partitioned table owns a physical table id, so exchanging the two ids
/// swaps their KV ranges without rewriting any row.
pub fn exchange_partition(
    meta: &mut Mutator,
    is: &InfoSchema,
    table: &TableName,
    partition: &str,
    with_table: &TableName,
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
    let part_ref = info
        .partition
        .as_ref()
        .ok_or_else(|| Error::unsupported("EXCHANGE PARTITION on non-partitioned table"))?;
    let Some(pos) = part_ref
        .definitions
        .iter()
        .position(|d| d.name.o().eq_ignore_ascii_case(partition))
    else {
        return Err(Error::new(
            ERR_CANT_DROP_FIELD_OR_KEY,
            format!("Unknown partition '{partition}'"),
        ));
    };
    let target_id = part_ref.definitions[pos].id;

    let (other_schema, other_name) = resolve_name(with_table, current_db)?;
    let other_db = is
        .schema_by_name(&other_schema)
        .ok_or_else(|| Error::new(ERR_BAD_DB, format!("Unknown database '{other_schema}'")))?;
    let mut other = is
        .table_by_name(&other_schema, &other_name)
        .cloned()
        .ok_or_else(|| {
            Error::new(
                ERR_NO_SUCH_TABLE,
                format!("Table '{other_schema}.{other_name}' doesn't exist"),
            )
        })?;
    if other.partition.is_some() {
        return Err(Error::new(
            1731,
            format!("Table to exchange with partition is partitioned: '{other_name}'"),
        ));
    }
    if other.is_view() || info.is_view() {
        return Err(Error::new(1177, "Can't open table"));
    }
    if other.temporary || info.temporary {
        return Err(Error::new(
            1733,
            format!("Table to exchange with partition is temporary: '{other_name}'"),
        ));
    }
    // Virtual generated ↔ non-generated mismatch → 3106 (before 1736).
    if let Some(msg) = exchange_generated_mismatch(&info, &other) {
        return Err(Error::new(3106, msg));
    }
    if let Some(attr) = exchange_global_index_mismatch(&info, &other) {
        return Err(Error::new(
            1731,
            format!("Non matching attribute '{attr}' between partition and table"),
        ));
    }
    // TiDB 1736 covers column layout, placement, indexes, and stored generated.
    if info.placement_policy != other.placement_policy {
        return Err(Error::new(1736, "Tables have different definitions"));
    }
    if !exchange_schemas_compatible(&info, &other) {
        return Err(Error::new(1736, "Tables have different definitions"));
    }

    validate_exchange_rows(storage, &info, &other, target_id)?;

    let part = info.partition.as_mut().unwrap();
    let other_old_id = other.id;
    std::mem::swap(&mut part.definitions[pos].id, &mut other.id);
    meta.update_table(db.id, &info)?;
    meta.update_table_with_new_id(other_db.id, other_old_id, &other)?;
    meta.gen_schema_version()?;
    Ok(())
}

fn validate_exchange_rows(
    storage: &dyn Storage,
    part_info: &TableInfo,
    other: &TableInfo,
    target_partition_id: i64,
) -> Result<()> {
    let check_on = sealdb_meta::enable_check_constraint();
    let types_other = other.col_field_types();
    let types_part = part_info.col_field_types();
    let mut txn = storage.begin()?;
    // Rows moving into the partition must belong there and pass its ENFORCED checks.
    let start = gen_table_record_prefix(other.id);
    let end = next_prefix(start.as_bytes());
    let pairs = txn.scan(start.as_bytes(), &end)?;
    for (_, raw) in pairs {
        let values = row_values(&decode_row(&raw, &types_other), other);
        let located = part_info.locate_partition_row(&values);
        let ok = matches!(located, Ok(id) if id == target_partition_id);
        if !ok || (check_on && !row_passes_checks(part_info, &values)) {
            return Err(Error::new(
                1737,
                "Found a row that does not match the partition",
            ));
        }
    }
    // Rows leaving the partition must pass the non-partitioned table's ENFORCED checks.
    if check_on && !other.checks.is_empty() {
        let part = part_info.partition.as_ref().unwrap();
        let Some(def) = part
            .definitions
            .iter()
            .find(|d| d.id == target_partition_id)
        else {
            return Ok(());
        };
        let start = gen_table_record_prefix(def.id);
        let end = next_prefix(start.as_bytes());
        let pairs = txn.scan(start.as_bytes(), &end)?;
        for (_, raw) in pairs {
            let values = row_values(&decode_row(&raw, &types_part), part_info);
            if !row_passes_checks(other, &values) {
                return Err(Error::new(
                    1737,
                    "Found a row that does not match the partition",
                ));
            }
        }
    }
    Ok(())
}

/// Virtual generated on one side only → TiDB 3106.
fn exchange_generated_mismatch(a: &TableInfo, b: &TableInfo) -> Option<&'static str> {
    if a.columns.len() != b.columns.len() {
        return None;
    }
    for (ca, cb) in a.columns.iter().zip(b.columns.iter()) {
        let a_virt = ca.generated_expr.is_some() && !ca.generated_stored;
        let b_virt = cb.generated_expr.is_some() && !cb.generated_stored;
        if a_virt != b_virt {
            return Some(
                "'Exchanging partitions for non-generated columns' is not supported for generated columns.",
            );
        }
    }
    None
}

fn exchange_global_index_mismatch(a: &TableInfo, b: &TableInfo) -> Option<String> {
    for ia in &a.indices {
        if !ia.global {
            continue;
        }
        let Some(ib) = b
            .indices
            .iter()
            .find(|i| i.name.o().eq_ignore_ascii_case(ia.name.o()))
        else {
            return Some(format!("global index: {}", ia.name.o()));
        };
        if !ib.global {
            return Some(format!("global index: {}", ia.name.o()));
        }
    }
    for ib in &b.indices {
        if !ib.global {
            continue;
        }
        if !a
            .indices
            .iter()
            .any(|i| i.name.o().eq_ignore_ascii_case(ib.name.o()) && i.global)
        {
            return Some(format!("global index: {}", ib.name.o()));
        }
    }
    None
}

fn exchange_schemas_compatible(a: &TableInfo, b: &TableInfo) -> bool {
    if a.columns.len() != b.columns.len() {
        return false;
    }
    use sealdb_types::field_type::{
        TYPE_INT24, TYPE_LONG, TYPE_LONG_LONG, TYPE_SHORT, TYPE_TINY, TYPE_YEAR,
    };
    for (ca, cb) in a.columns.iter().zip(b.columns.iter()) {
        if !ca.name.o().eq_ignore_ascii_case(cb.name.o()) {
            return false;
        }
        let int_like = matches!(
            ca.field_type.tp,
            TYPE_TINY | TYPE_SHORT | TYPE_INT24 | TYPE_LONG | TYPE_LONG_LONG | TYPE_YEAR
        ) && ca.field_type.tp == cb.field_type.tp;
        // INT display width (`int` vs `int(1)`) is ignored for EXCHANGE.
        let flen_ok = int_like || ca.field_type.flen == cb.field_type.flen;
        if ca.field_type.tp != cb.field_type.tp
            || !flen_ok
            || ca.field_type.decimal != cb.field_type.decimal
            || (ca.field_type.flag & 32) != (cb.field_type.flag & 32) // unsigned
            || (ca.field_type.flag & 1) != (cb.field_type.flag & 1) // not null
            || ca.auto_increment != cb.auto_increment
            || ca.auto_random != cb.auto_random
            || ca.generated_expr != cb.generated_expr
            || ca.generated_stored != cb.generated_stored
            // DEFAULT / ON UPDATE CURRENT_TIMESTAMP differences are allowed.
            || ca.field_type.charset != cb.field_type.charset
            // ZEROFILL (MySQL flag bit 64).
            || (ca.field_type.flag & 64) != (cb.field_type.flag & 64)
        {
            return false;
        }
    }
    // Index sets: names, uniqueness, column offsets. Visibility is ignored
    // (invisible ↔ visible EXCHANGE is allowed).
    let norm = |info: &TableInfo| {
        let mut idxs: Vec<_> = info
            .indices
            .iter()
            .filter(|i| !i.primary || !info.pk_is_handle)
            .map(|i| {
                (
                    i.name.o().to_ascii_lowercase(),
                    i.unique,
                    i.primary,
                    i.global,
                    i.columns.iter().map(|c| c.offset).collect::<Vec<_>>(),
                )
            })
            .collect();
        idxs.sort();
        idxs
    };
    if norm(a) != norm(b) {
        return false;
    }
    if a.pk_is_handle != b.pk_is_handle {
        return false;
    }
    true
}

fn row_values(
    decoded: &[(i64, sealdb_types::Datum)],
    info: &TableInfo,
) -> Vec<sealdb_types::Datum> {
    let mut by_id: std::collections::HashMap<i64, sealdb_types::Datum> =
        decoded.iter().cloned().collect();
    info.columns
        .iter()
        .map(|c| by_id.remove(&c.id).unwrap_or(sealdb_types::Datum::Null))
        .collect()
}

/// Best-effort eval of ENFORCED CHECK exprs (`a < 75`, `b < 75 or b > 100`).
fn row_passes_checks(info: &TableInfo, values: &[sealdb_types::Datum]) -> bool {
    for check in &info.checks {
        if !check.enforced {
            continue;
        }
        if !eval_check_expr(&check.expr, info, values) {
            return false;
        }
    }
    true
}

fn eval_check_expr(expr: &str, info: &TableInfo, values: &[sealdb_types::Datum]) -> bool {
    let lower = expr.to_ascii_lowercase();
    if let Some(pos) = lower.find(" or ") {
        return eval_check_expr(&expr[..pos], info, values)
            || eval_check_expr(&expr[pos + 4..], info, values);
    }
    if let Some(pos) = lower.find(" and ") {
        return eval_check_expr(&expr[..pos], info, values)
            && eval_check_expr(&expr[pos + 5..], info, values);
    }
    for op in ["<=", ">=", "<>", "!=", "<", ">", "="] {
        if let Some(pos) = expr.find(op) {
            let left = expr[..pos].trim();
            let right = expr[pos + op.len()..].trim();
            let lv = resolve_check_operand(left, info, values);
            let rv = resolve_check_operand(right, info, values);
            return match (lv, rv, op) {
                (Some(l), Some(r), "<") => l < r,
                (Some(l), Some(r), ">") => l > r,
                (Some(l), Some(r), "<=") => l <= r,
                (Some(l), Some(r), ">=") => l >= r,
                (Some(l), Some(r), "=") => l == r,
                (Some(l), Some(r), "<>" | "!=") => l != r,
                _ => true,
            };
        }
    }
    true
}

fn resolve_check_operand(
    tok: &str,
    info: &TableInfo,
    values: &[sealdb_types::Datum],
) -> Option<i64> {
    if let Ok(n) = tok.parse::<i64>() {
        return Some(n);
    }
    let idx = info
        .columns
        .iter()
        .position(|c| c.name.o().eq_ignore_ascii_case(tok))?;
    match values.get(idx)? {
        sealdb_types::Datum::I64(v) => Some(*v),
        sealdb_types::Datum::U64(v) => i64::try_from(*v).ok(),
        _ => None,
    }
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

/// `ADD PARTITION` for HASH/KEY: named defs and/or `PARTITIONS n`.
fn add_hash_key_partitions(
    meta: &mut Mutator,
    db_id: i64,
    info: &mut TableInfo,
    defs: &[PartitionDef],
    count: Option<u64>,
    storage: &dyn Storage,
) -> Result<()> {
    let part = info.partition.as_ref().unwrap();
    let existing = part.definitions.clone();
    let mut new_defs = existing.clone();
    if let Some(n) = count {
        if n == 0 {
            return Err(Error::new(1515, "At least one partition must be added"));
        }
        for _ in 0..n {
            // TiDB names the next HASH partition `p{N}` where N is the current
            // count; a colliding user-named partition yields 1517.
            let name = format!("p{}", new_defs.len());
            if new_defs
                .iter()
                .any(|d| d.name.o().eq_ignore_ascii_case(&name))
            {
                return Err(Error::new(
                    ERR_SAME_NAME_PARTITION,
                    format!("Duplicate partition name {name}"),
                ));
            }
            new_defs.push(PartitionDefinition::new(
                meta.gen_global_id()?,
                name,
                Vec::new(),
            ));
        }
    } else {
        if defs.is_empty() {
            return Err(Error::new(
                1492,
                "For HASH partitions each partition must be defined",
            ));
        }
        for def in defs {
            if new_defs
                .iter()
                .any(|d| d.name.o().eq_ignore_ascii_case(&def.name))
                || defs
                    .iter()
                    .filter(|d| d.name.eq_ignore_ascii_case(&def.name))
                    .count()
                    > 1
            {
                return Err(Error::new(
                    ERR_SAME_NAME_PARTITION,
                    format!("Duplicate partition name {}", def.name),
                ));
            }
            let mut built =
                PartitionDefinition::new(meta.gen_global_id()?, def.name.clone(), Vec::new());
            if let Some(c) = &def.comment {
                built = built.with_comment(c.clone());
            }
            built = built.with_placement_policy(def.placement_policy.clone());
            new_defs.push(built);
        }
    }
    redistribute_hash_key(storage, info, &existing, &new_defs)?;
    let part = info.partition.as_mut().unwrap();
    part.definitions = new_defs;
    part.num = part.definitions.len() as u64;
    part.uniform = hash_key_is_uniform(&part.definitions);
    meta.update_table(db_id, info)?;
    meta.gen_schema_version()?;
    Ok(())
}

/// TiDB `getPartitionIntervalFromTable`: a RANGE table is INTERVAL-managed only
/// when there are ≥2 non-NULL/non-MAXVALUE partitions with a constant numeric
/// step (or a stored numeric `interval_expr` that matches). A single partition
/// (CREATE with FIRST == LAST) is not INTERVAL for ALTER FIRST/LAST.
fn resolve_numeric_interval_step(part: &PartitionInfo) -> Option<i64> {
    let bounds: Vec<i64> = part
        .definitions
        .iter()
        .filter(|d| !d.is_maxvalue() && !d.name.o().eq_ignore_ascii_case("P_NULL"))
        .filter_map(|d| d.less_than.first()?.trim_matches('\'').parse::<i64>().ok())
        .collect();
    if bounds.len() < 2 {
        return None;
    }
    let step = bounds[1] - bounds[0];
    if step <= 0 {
        return None;
    }
    if bounds.windows(2).any(|w| w[1] - w[0] != step) {
        return None;
    }
    if !part.interval_expr.is_empty() {
        let stored = parse_stored_interval_step(&part.interval_expr)?;
        if stored != step {
            return None;
        }
    }
    Some(step)
}

fn parse_stored_interval_step(expr: &str) -> Option<i64> {
    let t = expr.trim();
    if t.contains('*') {
        let mut prod: i64 = 1;
        for part in t.split('*') {
            prod = prod.saturating_mul(part.trim().parse::<i64>().ok()?);
        }
        return (prod > 0).then_some(prod);
    }
    let num: String = t
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-' || *c == '+')
        .collect();
    let rest = t[num.len()..].trim();
    // Bare number or `N` with a time unit — unit intervals are not numeric ALTER.
    if !rest.is_empty() && rest.chars().any(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let n = num.parse::<i64>().ok()?;
    (n > 0).then_some(n)
}

/// `ALTER TABLE t FIRST PARTITION LESS THAN (bound)` for INTERVAL tables.
pub fn first_partition(
    meta: &mut Mutator,
    is: &InfoSchema,
    table: &TableName,
    bound: &str,
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
    let part = info.partition.as_ref().ok_or_else(|| {
        Error::new(
            1505,
            "Partition management on a not partitioned table is not possible",
        )
    })?;
    // Prefer TiDB-style inference; fall back to stored expr only when partitions
    // are still a consistent numeric INTERVAL sequence (≥2 defs).
    let inferred = resolve_numeric_interval_step(part);
    if inferred.is_none() && (part.interval_expr.is_empty() || !date_interval_scheme_ok(part)) {
        return Err(Error::new(
            ERR_UNSUPPORTED_DDL_OPERATION,
            "Unsupported FIRST PARTITION, does not seem like an INTERVAL partitioned table",
        ));
    }
    let target = bound.trim().trim_matches('\'').to_string();
    let mut drop_ids = Vec::new();
    let mut keep = Vec::new();
    let mut found = false;
    for def in &part.definitions {
        if def.is_maxvalue() || def.name.o().eq_ignore_ascii_case("P_NULL") {
            keep.push(def.clone());
            continue;
        }
        let b = def.less_than.first().cloned().unwrap_or_default();
        let b = b.trim_matches('\'').to_string();
        if !found {
            if b == target {
                found = true;
                keep.push(def.clone());
            } else {
                drop_ids.push(def.id);
            }
        } else {
            keep.push(def.clone());
        }
    }
    if !found {
        // A numeric scheme with a valid step reports the interpolated message;
        // a date/unit scheme that no longer regenerates from its FIRST bound is
        // the "stuck" TiDB error.
        if inferred.is_none() {
            return Err(Error::new(
                ERR_UNSUPPORTED_DDL_OPERATION,
                "Unsupported FIRST PARTITION, does not seem like an INTERVAL partitioned table",
            ));
        }
        let step = part.interval_expr.split_whitespace().next().unwrap_or("?");
        let cur_first = part
            .definitions
            .iter()
            .find(|d| !d.is_maxvalue() && !d.name.o().eq_ignore_ascii_case("P_NULL"))
            .and_then(|d| d.less_than.first())
            .map(|s| s.trim_matches('\'').to_string())
            .unwrap_or_default();
        return Err(Error::new(
            ERR_UNSUPPORTED_DDL_OPERATION,
            format!(
                "Unsupported INTERVAL: expr ({bound}) not matching FIRST + n INTERVALs ({cur_first} + n * {step})"
            ),
        ));
    }
    if drop_ids.is_empty() {
        return Err(Error::new(
            ERR_UNSUPPORTED_DDL_OPERATION,
            "Unsupported FIRST PARTITION, given value does not generate a list of partition names to be dropped",
        ));
    }
    let part = info.partition.as_mut().unwrap();
    part.definitions = keep;
    part.num = part.definitions.len() as u64;
    meta.update_table(db.id, &info)?;
    meta.gen_schema_version()?;
    drop_partition_keys(storage, &drop_ids)?;
    Ok(())
}

/// Date/MONTH/WEEK INTERVAL scheme still matches regenerating from the current
/// FIRST bound (TiDB gets stuck once FIRST is day 28/29/30 of a short month).
fn date_interval_scheme_ok(part: &PartitionInfo) -> bool {
    let expr = part.interval_expr.trim();
    if expr.is_empty() {
        return false;
    }
    let parts: Vec<&str> = expr.split_whitespace().collect();
    let (n, unit) = match parts.as_slice() {
        [n, u] => match (n.parse::<i64>(), u.to_ascii_uppercase()) {
            (Ok(n), u) if n > 0 => (n, u),
            _ => return false,
        },
        _ => return false,
    };
    let bounds: Vec<String> = part
        .definitions
        .iter()
        .filter(|d| !d.is_maxvalue() && !d.name.o().eq_ignore_ascii_case("P_NULL"))
        .filter_map(|d| {
            d.less_than
                .first()
                .map(|s| s.trim().trim_matches('\'').to_string())
        })
        .collect();
    if bounds.len() < 2 {
        return false;
    }
    // TiDB always adds the interval to the CURRENT FIRST bound, so each bound
    // must equal `first + i * step` (this is exactly why a FIRST on day 28/29/30
    // of a month gets stuck).
    let mut index = 1i64;
    for expect in bounds.iter().skip(1) {
        let next = match unit.as_str() {
            "WEEK" | "WEEKS" => add_days_ymd_str(&bounds[0], 7 * n * index),
            "DAY" | "DAYS" => add_days_ymd_str(&bounds[0], n * index),
            "MONTH" | "MONTHS" => add_months_ymd_str(&bounds[0], n * index),
            "YEAR" | "YEARS" => add_months_ymd_str(&bounds[0], 12 * n * index),
            "QUARTER" | "QUARTERS" => add_months_ymd_str(&bounds[0], 3 * n * index),
            _ => return false,
        };
        let Some(next) = next else {
            return false;
        };
        if &next != expect {
            return false;
        }
        index += 1;
    }
    true
}

fn add_days_ymd_str(date: &str, days: i64) -> Option<String> {
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() < 3 {
        return None;
    }
    let y: i32 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    let d: u32 = parts[2]
        .split_whitespace()
        .next()
        .unwrap_or(parts[2])
        .parse()
        .ok()?;
    let days_from = ymd_to_days_local(y, m, d);
    let (ny, nm, nd) = days_to_ymd_local(days_from + days);
    if date.len() > 10 {
        Some(format!("{ny:04}-{nm:02}-{nd:02}{}", &date[10..]))
    } else {
        Some(format!("{ny:04}-{nm:02}-{nd:02}"))
    }
}

fn add_months_ymd_str(date: &str, months: i64) -> Option<String> {
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() < 3 {
        return None;
    }
    let y: i32 = parts[0].parse().ok()?;
    let m: i64 = parts[1].parse().ok()?;
    let d: u32 = parts[2]
        .split_whitespace()
        .next()
        .unwrap_or(parts[2])
        .parse()
        .ok()?;
    let total = (y as i64) * 12 + (m - 1) + months;
    let ny = (total.div_euclid(12)) as i32;
    let nm = (total.rem_euclid(12) + 1) as u32;
    let max_d = match nm {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if ny % 4 == 0 && (ny % 100 != 0 || ny % 400 == 0) {
                29
            } else {
                28
            }
        }
        _ => 28,
    };
    let nd = d.min(max_d);
    if date.len() > 10 {
        Some(format!("{ny:04}-{nm:02}-{nd:02}{}", &date[10..]))
    } else {
        Some(format!("{ny:04}-{nm:02}-{nd:02}"))
    }
}

fn ymd_to_days_local(y: i32, m: u32, d: u32) -> i64 {
    // Howard Hinnant civil_from_days / days_from_civil.
    let y = y as i64 - if m <= 2 { 1 } else { 0 };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = m as i64 + if m > 2 { -3 } else { 9 };
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + (153 * mp + 2) / 5 + d as i64 - 1;
    era * 146097 + doe - 719468
}

fn days_to_ymd_local(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = (yoe + era * 400) as i32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (mp + if mp < 10 { 3 } else { -9 }) as u32;
    let y = y + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

/// `ALTER TABLE t LAST PARTITION LESS THAN (bound)` for INTERVAL tables.
pub fn last_partition(
    meta: &mut Mutator,
    is: &InfoSchema,
    table: &TableName,
    bound: &str,
    current_db: &str,
    storage: &dyn Storage,
) -> Result<()> {
    let _ = storage;
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
    let part = info.partition.as_ref().ok_or_else(|| {
        Error::new(
            1505,
            "Partition management on a not partitioned table is not possible",
        )
    })?;
    if matches!(part.tp, PartitionType::Hash | PartitionType::Key) {
        return Err(Error::new(
            ERR_UNSUPPORTED_DDL_OPERATION,
            "Unsupported LAST PARTITION of HASH/KEY partitioned table",
        ));
    }
    if matches!(part.tp, PartitionType::List) {
        // TiDB routes LAST PARTITION on LIST through ADD PARTITION which needs
        // explicit defs → 1492.
        return Err(Error::new(
            1492,
            "For LIST partitions each partition must be defined",
        ));
    }
    let step = match resolve_numeric_interval_step(part) {
        Some(s) => s,
        None => {
            return Err(Error::new(
                ERR_UNSUPPORTED_DDL_OPERATION,
                "Unsupported LAST PARTITION, does not seem like an INTERVAL partitioned table",
            ));
        }
    };
    if part.interval_maxvalue || part.definitions.iter().any(|d| d.is_maxvalue()) {
        return Err(Error::new(
            ERR_UNSUPPORTED_DDL_OPERATION,
            "Unsupported LAST PARTITION when MAXVALUE partition exists",
        ));
    }
    let target = bound
        .trim()
        .trim_matches('\'')
        .parse::<i64>()
        .map_err(|_| {
            Error::new(
                ERR_UNSUPPORTED_DDL_OPERATION,
                format!("Unsupported INTERVAL: expr ({bound}) not matching FIRST + n INTERVALs"),
            )
        })?;
    let last_bound = part
        .definitions
        .iter()
        .rev()
        .find(|d| !d.is_maxvalue() && !d.name.o().eq_ignore_ascii_case("P_NULL"))
        .and_then(|d| d.less_than.first())
        .and_then(|s| s.trim_matches('\'').parse::<i64>().ok())
        .unwrap_or(0);
    if target <= last_bound {
        return Err(Error::new(
            ERR_UNSUPPORTED_DDL_OPERATION,
            format!(
                "Unsupported INTERVAL: expr ({bound}) not matching FIRST + n INTERVALs ({last_bound} + n * {step})"
            ),
        ));
    }
    if (target - last_bound) % step != 0 {
        return Err(Error::new(
            ERR_UNSUPPORTED_DDL_OPERATION,
            format!(
                "Unsupported INTERVAL: expr ({bound}) not matching FIRST + n INTERVALs ({last_bound} + n * {step})"
            ),
        ));
    }
    let mut cur = last_bound + step;
    let part = info.partition.as_mut().unwrap();
    while cur <= target {
        let new_name = format!("P_LT_{cur}");
        if part
            .definitions
            .iter()
            .any(|d| d.name.o().eq_ignore_ascii_case(&new_name))
        {
            return Err(Error::new(
                ERR_SAME_NAME_PARTITION,
                format!("Duplicate partition name {new_name}"),
            ));
        }
        part.definitions.push(PartitionDefinition::new(
            meta.gen_global_id()?,
            new_name,
            vec![cur.to_string()],
        ));
        cur += step;
    }
    part.num = part.definitions.len() as u64;
    meta.update_table(db.id, &info)?;
    meta.gen_schema_version()?;
    Ok(())
}

/// `ALTER TABLE t COALESCE PARTITION n` for HASH/KEY.
pub fn coalesce_partitions(
    meta: &mut Mutator,
    is: &InfoSchema,
    table: &TableName,
    num: u64,
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
    let part = info.partition.as_ref().ok_or_else(|| {
        Error::new(
            1505,
            "Partition management on a not partitioned table is not possible",
        )
    })?;
    if !matches!(part.tp, PartitionType::Hash | PartitionType::Key) {
        return Err(Error::new(
            1509,
            "COALESCE PARTITION can only be used on HASH/KEY partitions",
        ));
    }
    if num == 0 {
        return Err(Error::new(1515, "At least one partition must be coalesced"));
    }
    let total = part.definitions.len() as u64;
    if num >= total {
        return Err(Error::new(
            1508,
            "Cannot remove all partitions, use DROP TABLE instead",
        ));
    }
    let keep = (total - num) as usize;
    let existing = part.definitions.clone();
    let new_defs = existing[..keep].to_vec();
    let dropped: Vec<i64> = existing[keep..].iter().map(|d| d.id).collect();
    redistribute_hash_key(storage, &info, &existing, &new_defs)?;
    drop_partition_keys(storage, &dropped)?;
    let part = info.partition.as_mut().unwrap();
    part.definitions = new_defs;
    part.num = part.definitions.len() as u64;
    part.uniform = hash_key_is_uniform(&part.definitions);
    meta.update_table(db.id, &info)?;
    meta.gen_schema_version()?;
    Ok(())
}

/// `ALTER TABLE t REMOVE PARTITIONING` — fold every partition into the table id.
pub fn remove_partitioning(
    meta: &mut Mutator,
    is: &InfoSchema,
    table: &TableName,
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
    let Some(part) = info.partition.take() else {
        return Err(Error::new(
            1505,
            "Partition management on a not partitioned table is not possible",
        ));
    };
    // TiDB `onReorgPartition` (ActionRemovePartitioning): every GLOBAL index is
    // re-created with a fresh index id and therefore moves to the end of the
    // index list; the table becomes unpartitioned, so all indexes turn LOCAL.
    move_recreated_indexes_to_end(&mut info, |_| false);
    let old_ids: Vec<i64> = part.definitions.iter().map(|d| d.id).collect();
    let mut txn = storage.begin()?;
    use std::collections::HashSet;
    let mut occupied: HashSet<i64> = HashSet::new();
    {
        let start = gen_table_record_prefix(info.id);
        let end = next_prefix(start.as_bytes());
        for (key, _) in txn.scan(start.as_bytes(), &end)? {
            if let Ok(handle) = decode_handle_from_row_key(&key) {
                occupied.insert(handle);
            }
        }
    }
    let mut next_handle = 30001i64;
    let mut max_new = 0i64;
    let mut moves: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> = Vec::new();
    for id in &old_ids {
        if *id == info.id {
            continue;
        }
        let start = gen_table_record_prefix(*id);
        let end = next_prefix(start.as_bytes());
        let pairs = txn.scan(start.as_bytes(), &end)?;
        for (key, raw) in pairs {
            let handle = decode_handle_from_row_key(&key)?;
            // Merging partitions into one physical table can collide handles
            // (each partition allocated its own `_tidb_rowid` sequence).
            let final_handle = if occupied.insert(handle) {
                handle
            } else {
                while occupied.contains(&next_handle) {
                    next_handle += 1;
                }
                let h = next_handle;
                next_handle += 1;
                occupied.insert(h);
                max_new = max_new.max(h);
                h
            };
            let dest = encode_row_key_with_handle(info.id, final_handle);
            moves.push((key, dest.as_bytes().to_vec(), raw));
        }
        // Wipe the rest of the physical table prefix (indexes, etc.).
        let start = encode_table_prefix(*id);
        let end = next_prefix(start.as_bytes());
        let pairs = txn.scan(start.as_bytes(), &end)?;
        for (key, _) in pairs {
            txn.delete(&key)?;
        }
    }
    for (old_key, new_key, raw) in moves {
        txn.set(&new_key, &raw)?;
        txn.delete(&old_key)?;
    }
    txn.commit()?;
    if max_new > 0 {
        meta.bump_handle(info.id, max_new)?;
    }
    meta.update_table(db.id, &info)?;
    meta.gen_schema_version()?;
    Ok(())
}

fn hash_key_is_uniform(defs: &[PartitionDefinition]) -> bool {
    defs.iter().enumerate().all(|(i, d)| {
        d.name.o().eq_ignore_ascii_case(&format!("p{i}"))
            && d.comment.is_empty()
            && d.placement_policy.is_none()
            && d.attributes.is_empty()
    })
}

/// Move HASH/KEY rows so each lands in its new modulo partition.
fn redistribute_hash_key(
    storage: &dyn Storage,
    info: &TableInfo,
    old_defs: &[PartitionDefinition],
    new_defs: &[PartitionDefinition],
) -> Result<()> {
    if old_defs.is_empty() || new_defs.is_empty() || old_defs.len() == new_defs.len() {
        return Ok(());
    }
    // Build a temporary TableInfo with the new partition set for locate.
    let mut located_info = info.clone();
    if let Some(part) = located_info.partition.as_mut() {
        part.definitions = new_defs.to_vec();
        part.num = new_defs.len() as u64;
    }
    let types = info.col_field_types();
    let mut txn = storage.begin()?;
    let mut moves: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> = Vec::new();
    for def in old_defs {
        let start = gen_table_record_prefix(def.id);
        let end = next_prefix(start.as_bytes());
        let pairs = txn.scan(start.as_bytes(), &end)?;
        for (key, raw) in pairs {
            let handle = decode_handle_from_row_key(&key)?;
            let decoded = decode_row(&raw, &types);
            let values: Vec<Datum> = info
                .columns
                .iter()
                .map(|col| {
                    decoded
                        .iter()
                        .find(|(id, _)| *id == col.id)
                        .map(|(_, v)| v.clone())
                        .unwrap_or(Datum::Null)
                })
                .collect();
            let dest_id = sealdb_expression::locate_partition_for_row(&located_info, &values)?;
            if dest_id == def.id {
                continue;
            }
            let dest_key = encode_row_key_with_handle(dest_id, handle);
            moves.push((key, dest_key.as_bytes().to_vec(), raw));
        }
    }
    for (old_key, new_key, raw) in moves {
        txn.set(&new_key, &raw)?;
        txn.delete(&old_key)?;
    }
    txn.commit()?;
    Ok(())
}

fn redistribute_reorganized_rows(
    meta: &mut Mutator,
    storage: &dyn Storage,
    info: &TableInfo,
    old_defs: &[PartitionDefinition],
    new_defs: &[PartitionDefinition],
    drop_ids: &[i64],
) -> Result<()> {
    use std::collections::{HashMap, HashSet};
    let types = info.col_field_types();
    let mut txn = storage.begin()?;
    // `_tidb_rowid` is per physical table: when rows from several partitions
    // are merged into one, colliding handles get fresh ids from TiDB's heap
    // rowid base (30001).
    let mut occupied: HashMap<i64, HashSet<i64>> = HashMap::new();
    for def in new_defs {
        let start = gen_table_record_prefix(def.id);
        let end = next_prefix(start.as_bytes());
        for (key, _) in txn.scan(start.as_bytes(), &end)? {
            if let Ok(handle) = decode_handle_from_row_key(&key) {
                occupied.entry(def.id).or_default().insert(handle);
            }
        }
    }
    let mut next_handle = 30001i64;
    let mut max_new = 0i64;
    let mut moves: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> = Vec::new();
    for def in old_defs {
        let start = gen_table_record_prefix(def.id);
        let end = next_prefix(start.as_bytes());
        let pairs = txn.scan(start.as_bytes(), &end)?;
        for (key, raw) in pairs {
            let handle = decode_handle_from_row_key(&key)?;
            let decoded = decode_row(&raw, &types);
            let values: Vec<Datum> = info
                .columns
                .iter()
                .map(|col| {
                    decoded
                        .iter()
                        .find(|(id, _)| *id == col.id)
                        .map(|(_, v)| v.clone())
                        .unwrap_or(Datum::Null)
                })
                .collect();
            let dest_id = sealdb_expression::locate_partition_for_row(info, &values)?;
            if dest_id == def.id {
                continue;
            }
            let taken = occupied.entry(dest_id).or_default();
            let final_handle = if taken.insert(handle) {
                handle
            } else {
                while taken.contains(&next_handle) {
                    next_handle += 1;
                }
                let h = next_handle;
                next_handle += 1;
                taken.insert(h);
                max_new = max_new.max(h);
                h
            };
            let dest_key = encode_row_key_with_handle(dest_id, final_handle);
            moves.push((key, dest_key.as_bytes().to_vec(), raw));
        }
    }
    for (old_key, new_key, raw) in moves {
        txn.set(&new_key, &raw)?;
        txn.delete(&old_key)?;
    }
    // Remove any leftover records (secondary indexes) under the replaced
    // physical ids; all rows have already been rewritten above.
    for id in drop_ids {
        if new_defs.iter().any(|d| d.id == *id) {
            continue;
        }
        let start = encode_table_prefix(*id);
        let end = next_prefix(start.as_bytes());
        let pairs = txn.scan(start.as_bytes(), &end)?;
        for (key, _) in pairs {
            txn.delete(&key)?;
        }
    }
    txn.commit()?;
    if max_new > 0 {
        meta.bump_handle(info.id, max_new)?;
    }
    Ok(())
}

fn range_bounds(
    def: &PartitionDef,
    info: &TableInfo,
    key_cols: &[String],
    range_columns: bool,
) -> Vec<String> {
    let mut out = vec![match &def.less_than {
        Some(v) => v.clone(),
        None => "MAXVALUE".to_string(),
    }];
    for bound in &def.extra_bounds {
        out.push(match bound {
            Some(v) => v.clone(),
            None => "MAXVALUE".to_string(),
        });
    }
    if range_columns {
        // `RANGE COLUMNS(a,b) … (MAXVALUE, 1)` on string `b` renders as
        // `(MAXVALUE,'1')`: trailing bounds are coerced to the column type.
        for (i, bound) in out.iter_mut().enumerate().skip(1) {
            if bound.eq_ignore_ascii_case("MAXVALUE") || is_quoted_literal(bound) {
                continue;
            }
            let string_col = key_cols
                .get(i)
                .and_then(|name| {
                    info.columns
                        .iter()
                        .find(|c| c.name.o().eq_ignore_ascii_case(name))
                })
                .is_some_and(|c| {
                    matches!(
                        c.field_type.tp,
                        sealdb_types::field_type::TYPE_STRING
                            | sealdb_types::field_type::TYPE_VAR_STRING
                            | sealdb_types::field_type::TYPE_VARCHAR
                    )
                });
            if string_col && bound.parse::<i64>().is_ok() {
                *bound = format!("'{bound}'");
            }
        }
    }
    out
}

fn is_quoted_literal(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2
        && ((b[0] == b'\'' && b[b.len() - 1] == b'\'') || (b[0] == b'"' && b[b.len() - 1] == b'"'))
}

/// TiDB INTERVAL `NULL PARTITION`: bound is the type minimum. Unsigned INT uses
/// `0`, which collides with `FIRST … LESS THAN (0)` → 1493. RANGE COLUMNS INT
/// uses `i32::MIN`; plain RANGE uses `i64::MIN` even for INT columns.
fn rewrite_interval_null_bound(
    defs: &mut [PartitionDef],
    info: &TableInfo,
    key_cols: &[String],
    range_columns: bool,
) {
    let Some(null_def) = defs
        .iter_mut()
        .find(|d| d.name.eq_ignore_ascii_case("P_NULL"))
    else {
        return;
    };
    let col = key_cols.first().and_then(|name| {
        info.columns
            .iter()
            .find(|c| c.name.o().eq_ignore_ascii_case(name))
    });
    let Some(col) = col else {
        return;
    };
    use sealdb_types::field_type::{
        TYPE_DATE, TYPE_DATETIME, TYPE_INT24, TYPE_LONG, TYPE_LONG_LONG, TYPE_SHORT, TYPE_TINY,
        TYPE_YEAR,
    };
    let bound = if col.field_type.is_unsigned()
        && matches!(
            col.field_type.tp,
            TYPE_TINY | TYPE_SHORT | TYPE_INT24 | TYPE_LONG | TYPE_LONG_LONG | TYPE_YEAR
        ) {
        "0".to_string()
    } else if matches!(col.field_type.tp, TYPE_DATE | TYPE_DATETIME) {
        "'0000-01-01'".to_string()
    } else if range_columns {
        match col.field_type.tp {
            TYPE_TINY => i8::MIN.to_string(),
            TYPE_SHORT => i16::MIN.to_string(),
            TYPE_INT24 | TYPE_LONG | TYPE_YEAR => i32::MIN.to_string(),
            _ => i64::MIN.to_string(),
        }
    } else {
        i64::MIN.to_string()
    };
    null_def.less_than = Some(bound);
}

/// CREATE RANGE: consecutive LESS THAN bounds must strictly increase (1493).
fn validate_range_create_increasing(defs: &[PartitionDef], ci: bool) -> Result<()> {
    let mut prev: Option<&str> = None;
    for def in defs {
        let Some(bound) = def.less_than.as_deref() else {
            // MAXVALUE may only be last; earlier MAXVALUE already rejected elsewhere.
            prev = Some("MAXVALUE");
            continue;
        };
        if bound.eq_ignore_ascii_case("MAXVALUE") {
            prev = Some(bound);
            continue;
        }
        if let Some(p) = prev {
            if p.eq_ignore_ascii_case("MAXVALUE") {
                return Err(Error::new(
                    1493,
                    "VALUES LESS THAN value must be strictly increasing for each partition",
                ));
            }
            let prev_plain = strip_bound_literal(p);
            let bound_plain = strip_bound_literal(bound);
            let increasing = match (prev_plain.parse::<i64>(), bound_plain.parse::<i64>()) {
                (Ok(a), Ok(b)) => b > a,
                _ => {
                    let cmp = sealdb_meta::model::compare_partition_strings(
                        &bound_plain,
                        &prev_plain,
                        ci,
                    );
                    cmp == std::cmp::Ordering::Greater
                }
            };
            if !increasing {
                return Err(Error::new(
                    1493,
                    "VALUES LESS THAN value must be strictly increasing for each partition",
                ));
            }
        }
        prev = Some(bound);
    }
    Ok(())
}

/// Bound literal without its surrounding quotes/escapes, for collation compare.
fn strip_bound_literal(bound: &str) -> String {
    let trimmed = bound.trim();
    let unquoted = if trimmed.len() >= 2
        && ((trimmed.starts_with('\'') && trimmed.ends_with('\''))
            || (trimmed.starts_with('"') && trimmed.ends_with('"')))
    {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    sealdb_meta::model::unescape_mysql_string(unquoted)
}

/// Partition-key column uses a `*_ci` collation (see `PartitionInfo::collation_ci`).
fn key_columns_ci(info: &TableInfo, key_cols: &[String]) -> bool {
    key_cols.iter().any(|name| {
        info.columns
            .iter()
            .find(|c| c.name.o().eq_ignore_ascii_case(name))
            .is_some_and(|c| {
                let collate = c.field_type.collate.to_ascii_lowercase();
                collate.ends_with("_ci") && !collate.starts_with("binary")
            })
    })
}

/// Plain `RANGE (int_col)` rejects non-integer LESS THAN bounds (TiDB 1697).
fn validate_range_int_bounds(
    defs: &[PartitionDef],
    _info: &TableInfo,
    _key_cols: &[String],
) -> Result<()> {
    const ERR_WRONG_TYPE_COLUMN_VALUE: i32 = 1697;
    for def in defs {
        for bound in std::iter::once(def.less_than.as_ref())
            .chain(def.extra_bounds.iter().map(|b| b.as_ref()))
            .flatten()
        {
            if bound.eq_ignore_ascii_case("MAXVALUE") {
                continue;
            }
            let ok = bound.parse::<i64>().is_ok()
                || bound.parse::<u64>().is_ok()
                || (bound.starts_with('-') && bound[1..].parse::<u64>().is_ok());
            if !ok {
                return Err(Error::new(
                    ERR_WRONG_TYPE_COLUMN_VALUE,
                    format!(
                        "VALUES value for partition '{}' must have type INT",
                        def.name
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// RANGE COLUMNS: reject mistyped LESS THAN bounds (TiDB 1654).
fn validate_range_columns_create_bounds(
    defs: &[PartitionDef],
    info: &TableInfo,
    key_cols: &[String],
) -> Result<()> {
    validate_range_columns_bounds(defs, info, key_cols)
}

/// RANGE COLUMNS ADD PARTITION bound checks (same 1654 rules as CREATE).
fn validate_range_columns_add_bounds(
    defs: &[PartitionDef],
    info: &TableInfo,
    key_cols: &[String],
) -> Result<()> {
    validate_range_columns_bounds(defs, info, key_cols)
}

fn validate_range_columns_bounds(
    defs: &[PartitionDef],
    info: &TableInfo,
    key_cols: &[String],
) -> Result<()> {
    use sealdb_types::field_type::{
        TYPE_DATE, TYPE_DATETIME, TYPE_DECIMAL, TYPE_DOUBLE, TYPE_FLOAT, TYPE_INT24, TYPE_LONG,
        TYPE_LONG_LONG, TYPE_NEW_DECIMAL, TYPE_SHORT, TYPE_STRING, TYPE_TIMESTAMP, TYPE_TINY,
        TYPE_VAR_STRING, TYPE_VARCHAR, TYPE_YEAR,
    };
    let Some(col) = key_cols.first().and_then(|name| {
        info.columns
            .iter()
            .find(|c| c.name.o().eq_ignore_ascii_case(name))
    }) else {
        return Ok(());
    };
    let tp = col.field_type.tp;
    let is_int = matches!(
        tp,
        TYPE_TINY
            | TYPE_SHORT
            | TYPE_INT24
            | TYPE_LONG
            | TYPE_LONG_LONG
            | TYPE_YEAR
            | TYPE_FLOAT
            | TYPE_DOUBLE
            | TYPE_DECIMAL
            | TYPE_NEW_DECIMAL
    );
    let is_temporal = matches!(tp, TYPE_DATE | TYPE_DATETIME | TYPE_TIMESTAMP);
    let is_string = matches!(tp, TYPE_STRING | TYPE_VAR_STRING | TYPE_VARCHAR);
    for def in defs {
        // TiDB only type-checks the leading key column of each bound tuple;
        // trailing bounds are coerced to the column type (`(MAXVALUE, 1)` on
        // `RANGE COLUMNS(a,b)` with string `b` becomes `(MAXVALUE,'1')`).
        for bound in std::iter::once(def.less_than.as_ref()).flatten() {
            if bound.eq_ignore_ascii_case("MAXVALUE") {
                continue;
            }
            let bytes = bound.as_bytes();
            let quoted = bytes.len() >= 2
                && ((bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\'')
                    || (bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"'));
            let hex = bound.len() > 2
                && (bound.starts_with("0x") || bound.starts_with("0X"))
                && bound[2..].bytes().all(|b| b.is_ascii_hexdigit());
            let numeric =
                !quoted && !hex && (bound.parse::<i64>().is_ok() || bound.parse::<u64>().is_ok());
            if is_int {
                // INT RANGE COLUMNS rejects hex (`0x10`) and quoted strings (`'g'`).
                if hex || quoted {
                    return Err(Error::new(
                        1654,
                        "Partition column values of incorrect type",
                    ));
                }
            } else if is_temporal {
                // Unquoted numeric / hex → wrong type for temporal RANGE COLUMNS.
                if !quoted && (numeric || hex || bound.parse::<i64>().is_ok()) {
                    return Err(Error::new(
                        1654,
                        "Partition column values of incorrect type",
                    ));
                }
                // Quoted but non-temporal garbage for datetime (`'g'`).
                // Compact dates like `'20190905'` are valid (TiDB accepts them).
                if quoted {
                    let inner = &bound[1..bound.len() - 1];
                    let ok = sealdb_types::time::parse_timestamp_literal(inner).is_ok()
                        || sealdb_types::time::parse_date_literal(inner).is_ok()
                        || (inner.len() == 8 && inner.bytes().all(|b| b.is_ascii_digit()));
                    if !ok {
                        return Err(Error::new(
                            1654,
                            "Partition column values of incorrect type",
                        ));
                    }
                }
            } else if is_string {
                // Unquoted decimal numbers are wrong for string RANGE COLUMNS;
                // hex (`0x20`) is accepted then fails increasing-order (1493).
                if !quoted && !hex && (numeric || bound.parse::<i64>().is_ok()) {
                    return Err(Error::new(
                        1654,
                        "Partition column values of incorrect type",
                    ));
                }
            }
        }
    }
    Ok(())
}

/// `ALTER TABLE t REORGANIZE PARTITION p0 [, p1] INTO (...)`.
pub fn reorganize_partitions(
    meta: &mut Mutator,
    is: &InfoSchema,
    table: &TableName,
    names: &[String],
    defs: &[PartitionDef],
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
    let part_ref = info
        .partition
        .as_ref()
        .ok_or_else(|| Error::unsupported("REORGANIZE PARTITION on non-partitioned table"))?;
    if !matches!(part_ref.tp, PartitionType::List | PartitionType::Range) {
        return Err(Error::unsupported(format!(
            "REORGANIZE PARTITION for {:?}",
            part_ref.tp
        )));
    }
    let mut remove_idxs = Vec::new();
    for n in names {
        let Some(idx) = part_ref
            .definitions
            .iter()
            .position(|d| d.name.o().eq_ignore_ascii_case(n))
        else {
            return Err(Error::new(
                ERR_CANT_DROP_FIELD_OR_KEY,
                format!("Unknown partition '{n}'"),
            ));
        };
        if !remove_idxs.contains(&idx) {
            remove_idxs.push(idx);
        }
    }
    remove_idxs.sort_unstable();

    // TiDB `getReplacedPartitionIDs`: RANGE partitions must be reorganized as
    // one contiguous run (8200 otherwise).
    if part_ref.tp == PartitionType::Range
        && remove_idxs.windows(2).any(|pair| pair[1] != pair[0] + 1)
    {
        return Err(Error::new(
            ERR_UNSUPPORTED_DDL_OPERATION,
            "Unsupported REORGANIZE PARTITION of RANGE; not adjacent partitions",
        ));
    }

    if part_ref.tp == PartitionType::Range {
        let key_cols: Vec<String> = part_ref.columns.iter().map(|c| c.o().to_string()).collect();
        let range_columns = part_ref.range_columns;
        let old_defs = part_ref.definitions.clone();
        let drop_ids: Vec<i64> = remove_idxs.iter().map(|&i| old_defs[i].id).collect();
        let mut seen = Vec::new();
        for def in defs {
            if seen
                .iter()
                .any(|n: &String| n.eq_ignore_ascii_case(&def.name))
            {
                return Err(Error::new(
                    ERR_SAME_NAME_PARTITION,
                    format!("Duplicate partition name {}", def.name),
                ));
            }
            seen.push(def.name.clone());
            if def.missing_bound || def.list_default || !def.in_values.is_empty() {
                return Err(Error::new(
                    1479,
                    "Syntax : RANGE PARTITIONING requires definition of VALUES LESS THAN for each partition",
                ));
            }
        }
        if defs.is_empty() {
            return Err(Error::new(
                1479,
                "Syntax : RANGE PARTITIONING requires definition of VALUES LESS THAN for each partition",
            ));
        }
        if range_columns {
            validate_range_columns_add_bounds(defs, &info, &key_cols)?;
        } else {
            validate_range_int_bounds(defs, &info, &key_cols)?;
        }
        validate_range_create_increasing(defs, key_columns_ci(&info, &key_cols))?;

        let mut new_defs = Vec::with_capacity(defs.len());
        for def in defs {
            new_defs.push(PartitionDefinition::new(
                meta.gen_global_id()?,
                def.name.clone(),
                range_bounds(def, &info, &key_cols, range_columns),
            ));
        }
        let mut rebuilt = Vec::new();
        let mut new_iter = new_defs.into_iter();
        let mut inserted = false;
        for (i, d) in old_defs.iter().enumerate() {
            if remove_idxs.contains(&i) {
                if !inserted {
                    rebuilt.extend(new_iter.by_ref());
                    inserted = true;
                }
                continue;
            }
            rebuilt.push(d.clone());
        }
        if !inserted {
            rebuilt.extend(new_iter);
        }
        let part = info.partition.as_mut().unwrap();
        part.definitions = rebuilt.clone();
        part.num = rebuilt.len() as u64;
        redistribute_reorganized_rows(meta, storage, &info, &old_defs, &rebuilt, &drop_ids)?;
        meta.update_table(db.id, &info)?;
        meta.gen_schema_version()?;
        return Ok(());
    }

    let mut covered: Vec<ListInEntry> = Vec::new();
    for &i in &remove_idxs {
        for e in &part_ref.definitions[i].in_values {
            if !matches!(e, ListInEntry::Default) {
                covered.push(e.clone());
            }
        }
    }
    let remaining: Vec<PartitionDefinition> = part_ref
        .definitions
        .iter()
        .enumerate()
        .filter(|(i, _)| !remove_idxs.contains(i))
        .map(|(_, d)| d.clone())
        .collect();
    let key_cols: Vec<String> = part_ref.columns.iter().map(|c| c.o().to_string()).collect();
    let range_columns = part_ref.range_columns;
    let old_defs = part_ref.definitions.clone();
    let drop_ids: Vec<i64> = remove_idxs.iter().map(|&i| old_defs[i].id).collect();
    let ncols = key_cols.len().max(1);
    validate_list_defs(defs, &info, &key_cols, range_columns, ncols, &remaining)?;

    let mut new_defs = Vec::new();
    for def in defs {
        let values = convert_list_values(def, &info, &key_cols, range_columns)?;
        new_defs.push(
            PartitionDefinition::new(meta.gen_global_id()?, def.name.clone(), Vec::new())
                .with_in_values(values)
                .with_list_default(def.list_default),
        );
    }
    check_reorganize_coverage(&covered, &new_defs, &remaining, range_columns)?;

    let mut rebuilt = Vec::new();
    let mut new_iter = new_defs.into_iter();
    let mut inserted = false;
    for (i, d) in old_defs.iter().enumerate() {
        if remove_idxs.contains(&i) {
            if !inserted {
                rebuilt.extend(new_iter.by_ref());
                inserted = true;
            }
            continue;
        }
        rebuilt.push(d.clone());
    }
    if !inserted {
        rebuilt.extend(new_iter);
    }
    let part = info.partition.as_mut().unwrap();
    part.definitions = rebuilt;
    part.num = part.definitions.len() as u64;
    meta.update_table(db.id, &info)?;
    meta.gen_schema_version()?;
    drop_partition_keys(storage, &drop_ids)
}

/// `ALTER TABLE t PARTITION BY ...` — rebuild the partition metadata.
///
/// TiDB redistributes existing rows into the new partitions; SealDB currently
/// only supports the metadata step, so a table that already holds rows is
/// rejected rather than silently dropping them (REORGANIZE lands later).
pub fn repartition(
    meta: &mut Mutator,
    is: &InfoSchema,
    table: &TableName,
    spec: &PartitionOption,
    update_indexes: &[IndexUpdate],
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
    let physical_ids = match &info.partition {
        Some(part) => part.definitions.iter().map(|d| d.id).collect::<Vec<_>>(),
        None => vec![info.id],
    };
    let mut txn = storage.begin()?;
    for id in physical_ids {
        let start = encode_table_prefix(id);
        let end = next_prefix(start.as_bytes());
        if !txn.scan(start.as_bytes(), &end)?.is_empty() {
            return Err(Error::unsupported(
                "ALTER TABLE PARTITION BY on a table that contains data",
            ));
        }
    }
    drop(txn);
    // TiDB `... UPDATE INDEXES (idx GLOBAL, idx2 LOCAL)`: the explicit option
    // overrides the current locality; unknown or repeated names are 1280.
    let mut seen = std::collections::HashSet::new();
    for update in update_indexes {
        let lower = update.name.to_ascii_lowercase();
        if !seen.insert(lower.clone()) {
            return Err(Error::new(
                ERR_WRONG_NAME_FOR_INDEX,
                format!("Incorrect index name '{}'", update.name),
            ));
        }
        if !info
            .indices
            .iter()
            .any(|idx| idx.name.o().eq_ignore_ascii_case(&update.name))
        {
            return Err(Error::new(
                ERR_WRONG_NAME_FOR_INDEX,
                format!("Incorrect index name '{}'", update.name),
            ));
        }
    }
    let new_global = |idx: &sealdb_meta::IndexInfo| -> bool {
        update_indexes
            .iter()
            .find(|u| u.name.eq_ignore_ascii_case(idx.name.o()))
            .map(|u| u.global)
            .unwrap_or(idx.global)
    };
    // TiDB re-creates every index that is (or becomes) GLOBAL with a fresh
    // index id, which appends it to the end of the index list while applying
    // the new locality.
    move_recreated_indexes_to_end(&mut info, new_global);
    attach_partitions(meta, &mut info, spec)?;
    meta.update_table(db.id, &info)?;
    meta.gen_schema_version()?;
    Ok(())
}
/// TiDB `onReorgPartition`: while (re)partitioning, each index that is GLOBAL
/// before or after the statement is rebuilt with a new id, so it moves to the
/// end of `TableInfo::indices`; its locality becomes `new_global`.
fn move_recreated_indexes_to_end(
    info: &mut TableInfo,
    new_global: impl Fn(&sealdb_meta::IndexInfo) -> bool,
) {
    let mut kept = Vec::with_capacity(info.indices.len());
    let mut moved = Vec::new();
    for mut idx in std::mem::take(&mut info.indices) {
        let target = new_global(&idx);
        if idx.global || target {
            idx.global = target;
            moved.push(idx);
        } else {
            kept.push(idx);
        }
    }
    kept.extend(moved);
    info.indices = kept;
}
