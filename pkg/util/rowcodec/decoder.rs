use super::common::{decode_int, decode_uint};
use super::row::{FindCol, Row};
use crate::codec::decode_float;
use sealdb_types::field_type::{
    FieldType, TYPE_BIT, TYPE_BLOB, TYPE_DATE, TYPE_DATETIME, TYPE_DOUBLE, TYPE_DURATION,
    TYPE_ENUM, TYPE_FLOAT, TYPE_INT24, TYPE_LONG, TYPE_LONG_BLOB, TYPE_LONG_LONG, TYPE_MEDIUM_BLOB,
    TYPE_NEW_DECIMAL, TYPE_SET, TYPE_SHORT, TYPE_STRING, TYPE_TIMESTAMP, TYPE_TINY, TYPE_TINY_BLOB,
    TYPE_VAR_STRING, TYPE_VARCHAR, TYPE_YEAR,
};
use sealdb_types::{Datum, EnumValue, MyDecimal, Time, TimeType};

/// Decode a v128 row. The new format has no type tags, so each requested column
/// needs a `FieldType`. An empty `columns` list returns every stored value as
/// `Bytes` / `Null` (used when the caller has no schema).
pub fn decode(raw: &[u8], columns: &[(i64, FieldType)]) -> Vec<(i64, Datum)> {
    let Some(row) = Row::from_bytes(raw) else {
        return Vec::new();
    };
    if columns.is_empty() {
        return decode_all(&row);
    }
    let mut out = Vec::with_capacity(columns.len());
    for (id, ft) in columns {
        let value = match row.find_col_id(*id) {
            FindCol::Null => Datum::Null,
            // A column the stored row predates reads as its implicit default
            // (TiDB returns the type's zero for a NOT NULL column).
            FindCol::NotFound => default_datum(ft),
            FindCol::NotNull(idx) => row
                .col_data(idx)
                .map(|data| decode_value(data, ft))
                .unwrap_or(Datum::Null),
        };
        out.push((*id, value));
    }
    out
}

/// Implicit default TiDB returns for a column that a stored row has no value
/// for: NULL when the column is nullable, else the type's zero value.
fn default_datum(ft: &FieldType) -> Datum {
    if ft.flag & sealdb_types::field_type::NOT_NULL_FLAG == 0 {
        return Datum::Null;
    }
    match ft.tp {
        // YEAR keeps its display type (text protocol prints the zero year as
        // `0000`).
        TYPE_YEAR => Datum::Year(0),
        TYPE_TINY | TYPE_SHORT | TYPE_LONG | TYPE_INT24 | TYPE_LONG_LONG => {
            if ft.is_unsigned() {
                Datum::U64(0)
            } else {
                Datum::I64(0)
            }
        }
        TYPE_FLOAT | TYPE_DOUBLE => Datum::F64(0.0),
        TYPE_NEW_DECIMAL => Datum::Decimal(
            MyDecimal::parse("0")
                .map(|d| d.with_frac(ft.decimal))
                .unwrap_or_else(|_| MyDecimal::from_i64(0)),
        ),
        TYPE_DATE | TYPE_DATETIME | TYPE_TIMESTAMP | TYPE_DURATION => {
            let mut t = Time::new(0, 0, 0, 0, 0, 0);
            t.tp = match ft.tp {
                TYPE_DATE => TimeType::Date,
                TYPE_TIMESTAMP => TimeType::Timestamp,
                TYPE_DURATION => TimeType::Time,
                _ => TimeType::Datetime,
            };
            t.fsp = ft.decimal.max(0) as i8;
            Datum::Time(t)
        }
        TYPE_ENUM | TYPE_SET => Datum::Enum(EnumValue {
            label: Vec::new(),
            number: 0,
        }),
        TYPE_BIT => Datum::Bit(Vec::new()),
        _ => Datum::Bytes(Vec::new()),
    }
}

/// Return every stored column without FieldType: not-null as raw `Bytes`.
fn decode_all(row: &Row<'_>) -> Vec<(i64, Datum)> {
    let mut out = Vec::new();
    for i in 0..row.num_not_null as usize {
        let id = i64::from(row.col_ids[i]);
        let data = row.col_data(i).unwrap_or(&[]);
        out.push((id, Datum::Bytes(data.to_vec())));
    }
    for i in row.num_not_null as usize..row.col_ids.len() {
        out.push((i64::from(row.col_ids[i]), Datum::Null));
    }
    out
}

/// Interpret untyped column bytes using MySQL `FieldType.tp` / unsigned.
fn decode_value(data: &[u8], ft: &FieldType) -> Datum {
    match ft.tp {
        TYPE_YEAR => Datum::Year(decode_int(data)),
        TYPE_TINY | TYPE_SHORT | TYPE_LONG | TYPE_INT24 | TYPE_LONG_LONG => {
            if ft.is_unsigned() {
                Datum::U64(decode_uint(data))
            } else {
                Datum::I64(decode_int(data))
            }
        }
        TYPE_FLOAT | TYPE_DOUBLE => decode_float(data)
            .map(|(_, v)| Datum::F64(v))
            .unwrap_or(Datum::Null),
        TYPE_NEW_DECIMAL => Datum::Decimal(
            MyDecimal::parse(&String::from_utf8_lossy(data))
                .unwrap_or_else(|_| MyDecimal::from_i64(0)),
        ),
        TYPE_DATE | TYPE_DATETIME | TYPE_TIMESTAMP | TYPE_DURATION => {
            let mut t = Time::from_packed_u64(decode_uint(data));
            t.tp = match ft.tp {
                TYPE_DATE => TimeType::Date,
                TYPE_TIMESTAMP => TimeType::Timestamp,
                TYPE_DURATION => TimeType::Time,
                _ => TimeType::Datetime,
            };
            // The column's fsp fixes the displayed fraction width
            // (`time(3)` shows `19:38:25.001` even though the packed value
            // carries microseconds).
            if ft.tp != TYPE_DATE && ft.decimal > 0 {
                t.fsp = ft.decimal.clamp(0, 6) as i8;
            }
            Datum::Time(t)
        }
        // ENUM/SET store member labels as text; rebuild the numeric form
        // (enum index, set bitmask) from `FieldType.elems` for comparisons.
        TYPE_ENUM | TYPE_SET => {
            let label = data.to_vec();
            let number = ft.enum_set_number(&label);
            Datum::Enum(EnumValue { label, number })
        }
        // CHAR keeps PAD SPACE semantics: the stored value is right-padded and
        // TiDB trims the padding on retrieval (VARCHAR keeps the spaces).
        TYPE_STRING => {
            let mut bytes = data.to_vec();
            if ft.charset != "binary" {
                while bytes.last() == Some(&b' ') {
                    bytes.pop();
                }
            }
            Datum::Bytes(bytes)
        }
        TYPE_VAR_STRING | TYPE_VARCHAR | TYPE_BLOB | TYPE_TINY_BLOB | TYPE_MEDIUM_BLOB
        | TYPE_LONG_BLOB => Datum::Bytes(data.to_vec()),
        // BIT(n) rows store the raw big-endian bytes (see insert `cast_for_column`).
        TYPE_BIT => Datum::Bit(data.to_vec()),
        _ => Datum::Bytes(data.to_vec()),
    }
}
