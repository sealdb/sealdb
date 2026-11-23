use sealdb_errno::{ERR_DUP_ENTRY, Error, Result};
use sealdb_kv::Transaction;
use sealdb_meta::{IndexInfo, TableInfo};
use sealdb_tablecodec::{encode_index_key, encode_row_key_with_handle};
use sealdb_types::Datum;

pub(crate) fn index_values(index: &IndexInfo, values: &[Datum]) -> Vec<Datum> {
    index
        .columns
        .iter()
        .map(|col| {
            values
                .get(col.offset as usize)
                .cloned()
                .unwrap_or(Datum::Null)
        })
        .collect()
}

/// Scalar type each element of an MVIndex hidden column is stored as.
#[derive(Clone, Copy, PartialEq)]
enum MvElementType {
    Signed,
    Unsigned,
    Double,
    Decimal,
    Date,
    Datetime,
    Time,
    Char,
}

fn mv_element_type(info: &TableInfo, index: &IndexInfo, pos: usize) -> Option<MvElementType> {
    let col = index.columns.get(pos)?;
    let hidden = info.columns.get(col.offset as usize)?;
    let text = hidden.generated_expr.as_deref()?;
    let expr = sealdb_parser::sqlparser_bridge::lower::parse_expr(text).ok()?;
    let sealdb_parser::ast::expressions::ExprNode::Cast(cast) = expr else {
        return None;
    };
    let ty = cast.target.to_ascii_lowercase();
    let base = ty.strip_suffix("array")?.trim();
    Some(if base == "signed" || base == "bigint" {
        MvElementType::Signed
    } else if base == "unsigned" {
        MvElementType::Unsigned
    } else if matches!(base, "double" | "float" | "real") {
        MvElementType::Double
    } else if base.starts_with("decimal") {
        MvElementType::Decimal
    } else if base == "date" {
        MvElementType::Date
    } else if base.starts_with("datetime") || base.starts_with("timestamp") {
        MvElementType::Datetime
    } else if base.starts_with("time") {
        MvElementType::Time
    } else if base.starts_with("char") || base.starts_with("varchar") || base.starts_with("binary")
    {
        MvElementType::Char
    } else {
        return None;
    })
}

pub(crate) fn multi_valued_elements(value: &Datum) -> Vec<String> {
    let Datum::Bytes(bytes) = value else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    if !text.starts_with('[') || !text.ends_with(']') {
        return Vec::new();
    }
    text[1..text.len() - 1]
        .split(',')
        .map(|part| part.trim().trim_matches('"').to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

fn mv_element_datum(ty: MvElementType, text: &str) -> Option<Datum> {
    Some(match ty {
        MvElementType::Signed => Datum::I64(text.parse().ok()?),
        MvElementType::Unsigned => Datum::U64(text.parse().ok()?),
        MvElementType::Double => Datum::F64(text.parse().ok()?),
        MvElementType::Decimal => Datum::Decimal(sealdb_types::MyDecimal::parse(text).ok()?),
        MvElementType::Char => Datum::Bytes(text.as_bytes().to_vec()),
        MvElementType::Date => Datum::Time(sealdb_types::parse_date_literal(text).ok()?),
        MvElementType::Datetime => Datum::Time(sealdb_types::parse_datetime_literal(text).ok()?),
        MvElementType::Time => {
            let mut time = sealdb_types::parse_datetime_literal(text).ok()?;
            time.tp = sealdb_types::TimeType::Time;
            Datum::Time(time)
        }
    })
}

/// One index entry per MVIndex element combination (`[1, 11]` → `1`, `11`).
/// An empty array writes no key, exactly like TiDB's multi-valued index.
pub(crate) fn index_value_variants(
    info: &TableInfo,
    index: &IndexInfo,
    values: &[Datum],
) -> Vec<Vec<Datum>> {
    let mut variants = vec![index_values(index, values)];
    for (pos, col) in index.columns.iter().enumerate() {
        let Some(ty) = mv_element_type(info, index, pos) else {
            continue;
        };
        let array = values
            .get(col.offset as usize)
            .cloned()
            .unwrap_or(Datum::Null);
        let elements: Vec<Datum> = multi_valued_elements(&array)
            .iter()
            .filter_map(|text| mv_element_datum(ty, text))
            .collect();
        if elements.is_empty() {
            return Vec::new();
        }
        let mut expanded = Vec::with_capacity(variants.len() * elements.len());
        for variant in &variants {
            for element in &elements {
                let mut next = variant.clone();
                next[pos] = element.clone();
                expanded.push(next);
            }
        }
        variants = expanded;
    }
    variants
}

/// TiDB partial index: only rows satisfying the predicate take a key entry.
pub(crate) fn index_applies(info: &TableInfo, index: &IndexInfo, values: &[Datum]) -> bool {
    match &index.where_clause {
        Some(text) => {
            sealdb_expression::eval_where_text(text, &crate::update::col_names(info), values)
        }
        None => true,
    }
}

pub(crate) fn write_index_keys(
    txn: &mut dyn Transaction,
    info: &TableInfo,
    handle: i64,
    values: &[Datum],
    physical_id: i64,
) -> Result<()> {
    for index in &info.indices {
        // A clustered integer PRIMARY KEY is the row handle: TiDB stores no
        // separate index entry for it (`pk_is_handle`).
        if index.primary && info.pk_is_handle {
            continue;
        }
        if !index_applies(info, index, values) {
            continue;
        }
        for entry in index_value_variants(info, index, values) {
            let key = encode_index_key(physical_id, index.id, &entry, handle, index.unique);
            if index.unique && txn.get(key.as_bytes())?.is_some() {
                let value = entry
                    .iter()
                    .map(crate::insert::duplicate_part_text)
                    .collect::<Vec<_>>()
                    .join("-");
                let name = if index.primary {
                    "PRIMARY".to_string()
                } else {
                    index.name.o().to_string()
                };
                return Err(Error::new(
                    ERR_DUP_ENTRY,
                    format!(
                        "Duplicate entry '{value}' for key '{}.{name}'",
                        info.name.o()
                    ),
                ));
            }
            txn.set(
                key.as_bytes(),
                encode_row_key_with_handle(physical_id, handle).as_bytes(),
            )?;
        }
    }
    Ok(())
}

pub(crate) fn delete_index_keys(
    txn: &mut dyn Transaction,
    info: &TableInfo,
    handle: i64,
    values: &[Datum],
    physical_id: i64,
) -> Result<()> {
    for index in &info.indices {
        // Clustered handle PRIMARY KEY rows are removed by the row key.
        if index.primary && info.pk_is_handle {
            continue;
        }
        if !index_applies(info, index, values) {
            continue;
        }
        for entry in index_value_variants(info, index, values) {
            let key = encode_index_key(physical_id, index.id, &entry, handle, index.unique);
            txn.delete(key.as_bytes())?;
        }
    }
    Ok(())
}
