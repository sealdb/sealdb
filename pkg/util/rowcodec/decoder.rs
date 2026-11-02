use super::common::{decode_int, decode_uint};
use super::row::{FindCol, Row};
use crate::codec::decode_float;
use sealdb_types::field_type::{
    FieldType, TYPE_BIT, TYPE_BLOB, TYPE_DATE, TYPE_DATETIME, TYPE_DOUBLE, TYPE_DURATION,
    TYPE_ENUM, TYPE_FLOAT, TYPE_INT24, TYPE_LONG, TYPE_LONG_BLOB, TYPE_LONG_LONG, TYPE_MEDIUM_BLOB,
    TYPE_NEW_DECIMAL, TYPE_SET, TYPE_SHORT, TYPE_STRING, TYPE_TIMESTAMP, TYPE_TINY, TYPE_TINY_BLOB,
    TYPE_VAR_STRING, TYPE_VARCHAR, TYPE_YEAR,
};
use sealdb_types::{Datum, MyDecimal, Time};

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
            FindCol::Null | FindCol::NotFound => Datum::Null,
            FindCol::NotNull(idx) => row
                .col_data(idx)
                .map(|data| decode_value(data, ft))
                .unwrap_or(Datum::Null),
        };
        out.push((*id, value));
    }
    out
}

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

fn decode_value(data: &[u8], ft: &FieldType) -> Datum {
    match ft.tp {
        TYPE_TINY | TYPE_SHORT | TYPE_LONG | TYPE_INT24 | TYPE_LONG_LONG | TYPE_YEAR => {
            if ft.unsigned {
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
            Datum::Time(Time::from_packed_u64(decode_uint(data)))
        }
        TYPE_BIT | TYPE_ENUM | TYPE_SET => Datum::U64(decode_uint(data)),
        TYPE_VAR_STRING | TYPE_VARCHAR | TYPE_STRING | TYPE_BLOB | TYPE_TINY_BLOB
        | TYPE_MEDIUM_BLOB | TYPE_LONG_BLOB => Datum::Bytes(data.to_vec()),
        _ => Datum::Bytes(data.to_vec()),
    }
}
