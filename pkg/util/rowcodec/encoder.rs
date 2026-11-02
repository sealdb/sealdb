use super::common::{ROW_FLAG_LARGE, encode_int, encode_uint};
use super::row::to_bytes;
use crate::codec::encode_float;
use sealdb_types::Datum;

/// Encode one table row in CodecVer 128 (TiDB `rowcodec.Encoder.Encode`).
/// Column ids are split into sorted not-null / null runs. Checksum is omitted.
pub fn encode(col_ids: &[i64], values: &[Datum]) -> Vec<u8> {
    let n = col_ids.len().min(values.len());
    let mut not_null: Vec<usize> = (0..n).filter(|&i| !values[i].is_null()).collect();
    let mut nulls: Vec<usize> = (0..n).filter(|&i| values[i].is_null()).collect();
    not_null.sort_by_key(|&i| col_ids[i]);
    nulls.sort_by_key(|&i| col_ids[i]);
    let mut large = col_ids[..n].iter().any(|id| *id > 255);
    let mut data = Vec::new();
    let mut offsets = Vec::with_capacity(not_null.len());
    for &i in &not_null {
        encode_value(&mut data, &values[i]);
        if data.len() > u16::MAX as usize {
            large = true;
        }
        offsets.push(data.len() as u32);
    }
    let flags = if large { ROW_FLAG_LARGE } else { 0 };
    let not_null_ids: Vec<u32> = not_null.iter().map(|&i| col_ids[i] as u32).collect();
    let null_ids: Vec<u32> = nulls.iter().map(|&i| col_ids[i] as u32).collect();
    to_bytes(flags, &not_null_ids, &null_ids, &offsets, &data)
}

/// Append one datum with no type tag. Integers are compact LE; floats are
/// memcomparable; decimals are UTF-8 string bytes (MyDecimal bin later).
fn encode_value(buf: &mut Vec<u8>, value: &Datum) {
    match value {
        Datum::Null => {}
        Datum::I64(v) => encode_int(buf, *v),
        Datum::U64(v) => encode_uint(buf, *v),
        Datum::F64(v) => encode_float(buf, *v),
        Datum::Bytes(b) => buf.extend_from_slice(b),
        Datum::Decimal(d) => buf.extend_from_slice(d.to_string().as_bytes()),
        Datum::Time(t) => encode_uint(buf, t.to_packed_u64()),
    }
}
