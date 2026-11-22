//! LIST / LIST COLUMNS partition value conversion and validation (TiDB parity).
use sealdb_errno::{
    ERR_NO_PARTITION_FOR_GIVEN_VALUE, ERR_UNSUPPORTED_DDL_OPERATION, Error, Result,
};
use sealdb_meta::{ListInEntry, ListLiteral, PartitionDefinition, TableInfo};
use sealdb_parser::ast::{
    ListInEntry as AstListInEntry, ListLiteral as AstListLiteral, PartitionDef,
};
use sealdb_types::field_type::{
    TYPE_BLOB, TYPE_DATE, TYPE_DATETIME, TYPE_LONG, TYPE_LONG_LONG, TYPE_STRING, TYPE_TIMESTAMP,
    TYPE_TINY, TYPE_VAR_STRING, TYPE_VARCHAR, TYPE_YEAR,
};

const ERR_MULTIPLE_DEF_CONST_IN_LIST_PART: i32 = 1495;
const ERR_WRONG_TYPE_COLUMN_VALUE: i32 = 1697;
const ERR_WRONG_TYPE_COLUMN_VALUE_ERROR: i32 = 1654;
const ERR_SAME_NAME_PARTITION_FIELD: i32 = 1653;
const ERR_REQUIRES_VALUES_IN: i32 = 1479;
const ERR_PARTITION_WRONG_VALUES: i32 = 1480;
pub const ERR_SAME_NAME_PARTITION: i32 = 1517;

pub fn convert_list_values(
    def: &PartitionDef,
    info: &TableInfo,
    key_cols: &[String],
    list_columns: bool,
) -> Result<Vec<ListInEntry>> {
    if def.list_default && def.in_values.is_empty() {
        return Ok(vec![ListInEntry::Default]);
    }
    let mut out = Vec::with_capacity(def.in_values.len());
    for entry in &def.in_values {
        match entry {
            AstListInEntry::Default => out.push(ListInEntry::Default),
            AstListInEntry::Row(row) => {
                out.push(ListInEntry::Row(coerce_list_row(
                    row,
                    info,
                    key_cols,
                    list_columns,
                )?));
            }
        }
    }
    Ok(out)
}

pub fn validate_list_defs(
    defs: &[PartitionDef],
    info: &TableInfo,
    key_cols: &[String],
    list_columns: bool,
    ncols: usize,
    existing: &[PartitionDefinition],
) -> Result<()> {
    let mut seen: Vec<ListInEntry> = existing
        .iter()
        .flat_map(|d| d.in_values.iter().cloned())
        .filter(|e| !matches!(e, ListInEntry::Default))
        .collect();

    for def in defs {
        if def.missing_bound || (def.in_values.is_empty() && !def.list_default) {
            if def.less_than.is_some() || !def.extra_bounds.is_empty() {
                return Err(Error::new(
                    ERR_PARTITION_WRONG_VALUES,
                    "Only RANGE PARTITIONING can use VALUES LESS THAN in partition definition",
                ));
            }
            return Err(Error::new(
                ERR_REQUIRES_VALUES_IN,
                "Syntax : LIST PARTITIONING requires definition of VALUES IN for each partition",
            ));
        }
        if def.less_than.is_some() || !def.extra_bounds.is_empty() {
            return Err(Error::new(
                ERR_PARTITION_WRONG_VALUES,
                "Only RANGE PARTITIONING can use VALUES LESS THAN in partition definition",
            ));
        }
        if !list_columns {
            for entry in &def.in_values {
                if let AstListInEntry::Row(row) = entry {
                    for lit in row {
                        if matches!(lit, AstListLiteral::String(_)) {
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
            }
        }
        for entry in &def.in_values {
            match entry {
                AstListInEntry::Default => {}
                AstListInEntry::Row(row) => {
                    if list_columns && ncols > 1 && row.len() != ncols {
                        return Err(Error::new(
                            ERR_SAME_NAME_PARTITION_FIELD,
                            "Inconsistency in usage of column lists for partitioning",
                        ));
                    }
                    if list_columns {
                        check_list_columns_type(row, info, key_cols)?;
                    }
                    let coerced = coerce_list_row(row, info, key_cols, list_columns)?;
                    let as_entry = ListInEntry::Row(coerced);
                    if seen
                        .iter()
                        .any(|e| list_entries_equal(e, &as_entry, info, key_cols, list_columns))
                    {
                        return Err(Error::new(
                            ERR_MULTIPLE_DEF_CONST_IN_LIST_PART,
                            "Multiple definition of same constant in list partitioning",
                        ));
                    }
                    seen.push(as_entry);
                }
            }
        }
    }
    let defaults_in_defs = defs
        .iter()
        .filter(|d| {
            d.list_default
                || d.in_values
                    .iter()
                    .any(|e| matches!(e, AstListInEntry::Default))
        })
        .count();
    if existing.is_empty() && defaults_in_defs > 1 {
        return Err(Error::new(
            ERR_MULTIPLE_DEF_CONST_IN_LIST_PART,
            "Multiple definition of same constant in list partitioning",
        ));
    }
    Ok(())
}

pub fn reject_add_when_default_exists(existing: &[PartitionDefinition]) -> Result<()> {
    if existing.iter().any(|d| d.has_list_default()) {
        return Err(Error::new(
            ERR_UNSUPPORTED_DDL_OPERATION,
            "Unsupported ADD List partition, already contains DEFAULT partition. Please use REORGANIZE PARTITION instead",
        ));
    }
    Ok(())
}

pub fn check_reorganize_coverage(
    covered: &[ListInEntry],
    new_defs: &[PartitionDefinition],
    _remaining: &[PartitionDefinition],
    list_columns: bool,
) -> Result<()> {
    // TiDB: an untouched DEFAULT partition does NOT absorb constants that were
    // explicitly listed on the reorganized source. Only a DEFAULT declared in
    // the INTO list covers leftover source constants.
    let dest_has_default = new_defs.iter().any(|d| d.has_list_default());
    if dest_has_default {
        return Ok(());
    }
    for c in covered {
        let ListInEntry::Row(row) = c else { continue };
        let found = new_defs.iter().any(|d| {
            d.in_values
                .iter()
                .any(|e| matches!(e, ListInEntry::Row(r) if r == row))
        });
        if !found {
            let msg = if list_columns {
                // TiDB `LIST COLUMNS` reports the whole key row, not the
                // offending scalar (`ErrNoPartitionForGivenValue` wording).
                "Table has no partition for value from column_list".to_string()
            } else {
                match row.as_slice() {
                    [ListLiteral::Number(n)] => format!("Table has no partition for value {n}"),
                    [ListLiteral::String(s)] => format!("Table has no partition for value '{s}'"),
                    [ListLiteral::Null] => "Table has no partition for value NULL".into(),
                    _ => "Table has no partition for value".into(),
                }
            };
            return Err(Error::new(ERR_NO_PARTITION_FOR_GIVEN_VALUE, msg));
        }
    }
    Ok(())
}

fn list_entries_equal(
    left: &ListInEntry,
    right: &ListInEntry,
    info: &TableInfo,
    key_cols: &[String],
    list_columns: bool,
) -> bool {
    match (left, right) {
        (ListInEntry::Default, ListInEntry::Default) => true,
        (ListInEntry::Row(a), ListInEntry::Row(b)) if a.len() == b.len() => a
            .iter()
            .zip(b.iter())
            .enumerate()
            .all(|(i, (x, y))| list_literal_equal(x, y, info, key_cols.get(i), list_columns)),
        _ => false,
    }
}

fn list_literal_equal(
    left: &ListLiteral,
    right: &ListLiteral,
    info: &TableInfo,
    col_name: Option<&String>,
    _list_columns: bool,
) -> bool {
    match (left, right) {
        (ListLiteral::Null, ListLiteral::Null) => true,
        (ListLiteral::Number(a), ListLiteral::Number(b)) => {
            match (a.parse::<i128>(), b.parse::<i128>()) {
                (Ok(x), Ok(y)) => x == y,
                _ => a == b,
            }
        }
        (ListLiteral::String(a), ListLiteral::String(b)) => {
            let ci = col_name
                .and_then(|name| {
                    info.columns
                        .iter()
                        .find(|c| c.name.o().eq_ignore_ascii_case(name))
                })
                .map(|c| {
                    let col = c.field_type.collate.to_ascii_lowercase();
                    col.contains("_ci")
                        || col.ends_with("_general_ci")
                        || col.ends_with("_unicode_ci")
                })
                .unwrap_or(false);
            if ci {
                a.eq_ignore_ascii_case(b)
            } else {
                a == b
            }
        }
        (ListLiteral::Number(n), ListLiteral::String(s))
        | (ListLiteral::String(s), ListLiteral::Number(n)) => n == s,
        _ => false,
    }
}

fn coerce_list_row(
    row: &[AstListLiteral],
    info: &TableInfo,
    key_cols: &[String],
    list_columns: bool,
) -> Result<Vec<ListLiteral>> {
    let mut out = Vec::with_capacity(row.len());
    for (i, lit) in row.iter().enumerate() {
        let col_name = key_cols.get(i).map(String::as_str).unwrap_or("");
        let col = info
            .columns
            .iter()
            .find(|c| c.name.o().eq_ignore_ascii_case(col_name));
        out.push(coerce_list_literal(
            lit,
            col.map(|c| c.field_type.tp),
            list_columns,
        )?);
    }
    Ok(out)
}

fn coerce_list_literal(
    lit: &AstListLiteral,
    col_tp: Option<u8>,
    list_columns: bool,
) -> Result<ListLiteral> {
    match lit {
        AstListLiteral::Null => Ok(ListLiteral::Null),
        AstListLiteral::Number(n) => {
            if list_columns && col_tp.is_some_and(is_string_tp) {
                return Ok(ListLiteral::String(n.clone()));
            }
            Ok(ListLiteral::Number(n.clone()))
        }
        AstListLiteral::String(s) => {
            if col_tp.is_some_and(|tp| matches!(tp, TYPE_DATE | TYPE_DATETIME | TYPE_TIMESTAMP)) {
                // Store the canonical form so a row value (`Datum::Time`) that
                // TiDB parses from a compact literal (`20180203`) matches.
                return Ok(ListLiteral::String(normalize_datetime_literal(s)));
            }
            Ok(ListLiteral::String(s.clone()))
        }
    }
}

/// Canonical `YYYY-MM-DD[ HH:MM:SS]` form of a compact date/datetime literal.
/// Non-compact text is returned unchanged (the type checker validates it).
fn normalize_datetime_literal(s: &str) -> String {
    let b = s.as_bytes();
    if b.len() == 8 && b.iter().all(u8::is_ascii_digit) {
        return format!("{}-{}-{}", &s[0..4], &s[4..6], &s[6..8]);
    }
    if b.len() == 14 && b.iter().all(u8::is_ascii_digit) {
        return format!(
            "{}-{}-{} {}:{}:{}",
            &s[0..4],
            &s[4..6],
            &s[6..8],
            &s[8..10],
            &s[10..12],
            &s[12..14]
        );
    }
    s.to_string()
}

fn is_string_tp(tp: u8) -> bool {
    matches!(tp, TYPE_STRING | TYPE_VAR_STRING | TYPE_VARCHAR | TYPE_BLOB)
}

fn check_list_columns_type(
    row: &[AstListLiteral],
    info: &TableInfo,
    key_cols: &[String],
) -> Result<()> {
    for (i, lit) in row.iter().enumerate() {
        let Some(col_name) = key_cols.get(i) else {
            continue;
        };
        let Some(col) = info
            .columns
            .iter()
            .find(|c| c.name.o().eq_ignore_ascii_case(col_name))
        else {
            continue;
        };
        let tp = col.field_type.tp;
        let flen = col.field_type.flen;
        let unsigned = col.field_type.flag & 32 != 0;
        match lit {
            AstListLiteral::Null => {}
            AstListLiteral::Number(n) => {
                if is_string_tp(tp) {
                    if flen > 0 && n.len() as i32 > flen {
                        return type_err();
                    }
                    continue;
                }
                if matches!(tp, TYPE_DATE | TYPE_DATETIME | TYPE_TIMESTAMP) {
                    return type_err();
                }
                if unsigned && n.starts_with('-') {
                    return type_err();
                }
                if matches!(tp, TYPE_TINY) {
                    if let Ok(v) = n.parse::<i64>() {
                        let (min, max) = if unsigned { (0, 255) } else { (-128, 127) };
                        if v < min || v > max {
                            return type_err();
                        }
                    }
                }
                if matches!(tp, TYPE_LONG_LONG) && !unsigned {
                    if n.parse::<i64>().is_err() && n.parse::<u64>().is_ok() {
                        return type_err();
                    }
                }
                let _ = (TYPE_LONG, TYPE_YEAR);
            }
            AstListLiteral::String(s) => {
                if matches!(tp, TYPE_TINY | TYPE_LONG | TYPE_LONG_LONG | TYPE_YEAR) {
                    // Non-numeric string into an integer LIST COLUMNS key.
                    if s.parse::<i128>().is_err() && s.parse::<u128>().is_err() {
                        return type_err();
                    }
                }
                if is_string_tp(tp) && flen > 0 && s.len() as i32 > flen {
                    return type_err();
                }
                if matches!(tp, TYPE_DATE | TYPE_DATETIME | TYPE_TIMESTAMP)
                    && !valid_datetime_literal(s)
                {
                    return type_err();
                }
            }
        }
    }
    Ok(())
}

fn type_err() -> Result<()> {
    Err(Error::new(
        ERR_WRONG_TYPE_COLUMN_VALUE_ERROR,
        "Partition column values of incorrect type",
    ))
}

fn valid_datetime_literal(s: &str) -> bool {
    let bytes = s.as_bytes();
    // TiDB's date parser also accepts the compact `YYYYMMDD` form
    // (`'20180203'` in a LIST COLUMNS date key).
    let (y, m, d) = if bytes.len() == 8 && bytes.iter().all(u8::is_ascii_digit) {
        let y = std::str::from_utf8(&bytes[0..4])
            .ok()
            .and_then(|x| x.parse().ok())
            .unwrap_or(-1);
        let m = std::str::from_utf8(&bytes[4..6])
            .ok()
            .and_then(|x| x.parse().ok())
            .unwrap_or(0);
        let d = std::str::from_utf8(&bytes[6..8])
            .ok()
            .and_then(|x| x.parse().ok())
            .unwrap_or(0);
        (y, m, d)
    } else if bytes.len() >= 10 {
        let y = std::str::from_utf8(&bytes[0..4])
            .ok()
            .and_then(|x| x.parse().ok())
            .unwrap_or(-1);
        let m = std::str::from_utf8(&bytes[5..7])
            .ok()
            .and_then(|x| x.parse().ok())
            .unwrap_or(0);
        let d = std::str::from_utf8(&bytes[8..10])
            .ok()
            .and_then(|x| x.parse().ok())
            .unwrap_or(0);
        (y, m, d)
    } else {
        return false;
    };
    if !(1..=12).contains(&m) || d == 0 {
        return false;
    }
    let max_d = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
            if leap { 29 } else { 28 }
        }
        _ => return false,
    };
    d <= max_d
}
