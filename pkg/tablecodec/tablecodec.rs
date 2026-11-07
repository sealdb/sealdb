use sealdb_errno::{Error, Result};
use sealdb_kv::Key;
use sealdb_types::{Datum, FieldType};
use sealdb_util::codec::{
    decode_bytes, decode_float, decode_int, encode_bytes, encode_float, encode_int,
    encode_int_to_cmp_uint,
};
use sealdb_util::rowcodec::{self, CODEC_VER};

pub const TABLE_PREFIX: &[u8] = b"t";
pub const RECORD_PREFIX_SEP: &[u8] = b"_r";
pub const INDEX_PREFIX_SEP: &[u8] = b"_i";
pub const META_PREFIX: &[u8] = b"m";
pub const PREFIX_LEN: usize = 1 + 8 + 2;

pub fn table_prefix() -> &'static [u8] {
    TABLE_PREFIX
}

pub fn meta_prefix() -> &'static [u8] {
    META_PREFIX
}

pub fn encode_table_prefix(table_id: i64) -> Key {
    let mut buf = Vec::with_capacity(9);
    buf.extend_from_slice(TABLE_PREFIX);
    encode_int(&mut buf, table_id);
    Key::from_raw(buf)
}

pub fn encode_row_key(table_id: i64, encoded_handle: &[u8]) -> Key {
    let mut buf = Vec::with_capacity(PREFIX_LEN + encoded_handle.len());
    append_table_record_prefix(&mut buf, table_id);
    buf.extend_from_slice(encoded_handle);
    Key::from_raw(buf)
}

pub fn encode_row_key_with_handle(table_id: i64, handle: i64) -> Key {
    let mut encoded = Vec::with_capacity(8);
    encode_int(&mut encoded, handle);
    encode_row_key(table_id, &encoded)
}

pub fn encode_index_seek_key(table_id: i64, idx_id: i64, encoded_value: &[u8]) -> Key {
    let mut key = Vec::with_capacity(PREFIX_LEN + 8 + encoded_value.len());
    append_table_index_prefix(&mut key, table_id);
    encode_int(&mut key, idx_id);
    key.extend_from_slice(encoded_value);
    Key::from_raw(key)
}

/// Encode one index column value. NULL sorts first.
/// Completeness wave: unique-index NULL handling and prefix truncation.
pub fn encode_index_datum(buf: &mut Vec<u8>, value: &Datum) {
    match value {
        Datum::Null => buf.push(0),
        Datum::I64(v) => {
            buf.push(1);
            encode_int(buf, *v);
        }
        Datum::U64(v) => {
            buf.push(1);
            encode_int(buf, *v as i64);
        }
        Datum::F64(v) => {
            buf.push(2);
            encode_float(buf, *v);
        }
        other => {
            buf.push(3);
            encode_bytes(buf, &other.as_bytes());
        }
    }
}

pub fn encode_index_values(values: &[Datum]) -> Vec<u8> {
    let mut buf = Vec::new();
    for value in values {
        encode_index_datum(&mut buf, value);
    }
    buf
}

/// Non-unique secondary index key: `t{table}_i{idx}{encoded_cols}{handle}`.
pub fn encode_index_key(
    table_id: i64,
    idx_id: i64,
    values: &[Datum],
    handle: i64,
    unique: bool,
) -> Key {
    let mut encoded = encode_index_values(values);
    // Unique indexes allow repeated NULLs (SQL semantics), so those entries
    // keep the handle suffix and never collide with each other.
    if !unique || values.iter().any(|v| v.is_null()) {
        encode_int(&mut encoded, handle);
    }
    encode_index_seek_key(table_id, idx_id, &encoded)
}

pub fn encode_index_seek_prefix(table_id: i64, idx_id: i64, values: &[Datum]) -> Key {
    encode_index_seek_key(table_id, idx_id, &encode_index_values(values))
}

pub fn decode_handle_from_index_key(key: &[u8]) -> Result<i64> {
    if key.len() < PREFIX_LEN + 8 + 8 {
        return Err(Error::new(
            sealdb_errno::ERR_INVALID_KEY,
            "index key too short",
        ));
    }
    let (_, handle) = decode_int(&key[key.len() - 8..])?;
    Ok(handle)
}

pub fn decode_index_datum(buf: &[u8]) -> Result<(&[u8], Datum)> {
    if buf.is_empty() {
        return Err(Error::new(
            sealdb_errno::ERR_INVALID_KEY,
            "index datum too short",
        ));
    }
    match buf[0] {
        0 => Ok((&buf[1..], Datum::Null)),
        1 => {
            let (rest, v) = decode_int(&buf[1..])?;
            Ok((rest, Datum::I64(v)))
        }
        2 => {
            let (rest, v) = decode_float(&buf[1..])?;
            Ok((rest, Datum::F64(v)))
        }
        3 => {
            let (rest, bytes) = decode_bytes(&buf[1..], false)?;
            Ok((rest, Datum::Bytes(bytes)))
        }
        other => Err(Error::new(
            sealdb_errno::ERR_INVALID_KEY,
            format!("unknown index datum tag {other}"),
        )),
    }
}

pub fn decode_index_key_values(
    key: &[u8],
    n_cols: usize,
    unique: bool,
) -> Result<(Vec<Datum>, i64)> {
    if key.len() < PREFIX_LEN + 8 {
        return Err(Error::new(
            sealdb_errno::ERR_INVALID_KEY,
            "index key too short",
        ));
    }
    let mut rest = &key[PREFIX_LEN + 8..];
    let mut values = Vec::with_capacity(n_cols);
    for _ in 0..n_cols {
        let (next, value) = decode_index_datum(rest)?;
        rest = next;
        values.push(value);
    }
    let handle = if unique {
        0
    } else {
        let (_, handle) = decode_int(rest)?;
        handle
    };
    Ok((values, handle))
}

pub fn gen_table_record_prefix(table_id: i64) -> Key {
    let mut buf = Vec::with_capacity(PREFIX_LEN);
    append_table_record_prefix(&mut buf, table_id);
    Key::from_raw(buf)
}

pub fn gen_table_index_prefix(table_id: i64) -> Key {
    let mut buf = Vec::with_capacity(PREFIX_LEN);
    append_table_index_prefix(&mut buf, table_id);
    Key::from_raw(buf)
}

fn append_table_record_prefix(buf: &mut Vec<u8>, table_id: i64) {
    buf.extend_from_slice(TABLE_PREFIX);
    encode_int(buf, table_id);
    buf.extend_from_slice(RECORD_PREFIX_SEP);
}

fn append_table_index_prefix(buf: &mut Vec<u8>, table_id: i64) {
    buf.extend_from_slice(TABLE_PREFIX);
    encode_int(buf, table_id);
    buf.extend_from_slice(INDEX_PREFIX_SEP);
}

pub fn encode_int_handle(handle: i64) -> Vec<u8> {
    encode_int_to_cmp_uint(handle).to_be_bytes().to_vec()
}

pub fn decode_handle_from_row_key(key: &[u8]) -> Result<i64> {
    if key.len() < PREFIX_LEN + 8 {
        return Err(Error::new(
            sealdb_errno::ERR_INVALID_KEY,
            "row key too short",
        ));
    }
    let (_, handle) = decode_int(&key[PREFIX_LEN..])?;
    Ok(handle)
}

pub fn next_prefix(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    for i in (0..end.len()).rev() {
        if end[i] != 0xFF {
            end[i] += 1;
            end.truncate(i + 1);
            return end;
        }
    }
    end.push(0);
    end
}

const TAG_NULL: u8 = 0;
const TAG_I64: u8 = 1;
const TAG_F64: u8 = 2;
const TAG_BYTES: u8 = 3;

/// Encode a table row in TiDB CodecVer 128 (`pkg/util/rowcodec`).
pub fn encode_row(col_ids: &[i64], values: &[Datum]) -> Vec<u8> {
    rowcodec::encode(col_ids, values)
}

/// Decode a table row. v128 needs `columns` FieldTypes; any other first byte is
/// the old tagged layout kept as a read fallback for pre-rowcodec KV.
pub fn decode_row(raw: &[u8], columns: &[(i64, FieldType)]) -> Vec<(i64, Datum)> {
    if raw.first() == Some(&CODEC_VER) {
        rowcodec::decode(raw, columns)
    } else {
        decode_legacy_row(raw)
    }
}

/// Tagged `(col_id, type_tag, payload)*` layout used before CodecVer 128.
fn decode_legacy_row(raw: &[u8]) -> Vec<(i64, Datum)> {
    if raw.len() < 4 {
        return Vec::new();
    }
    let n = u32::from_le_bytes(raw[0..4].try_into().unwrap_or([0; 4])) as usize;
    let mut i = 4;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        if i + 9 > raw.len() {
            break;
        }
        let id = i64::from_le_bytes(raw[i..i + 8].try_into().unwrap_or([0; 8]));
        i += 8;
        let tag = raw[i];
        i += 1;
        let value = match tag {
            TAG_NULL => Datum::Null,
            TAG_I64 if i + 8 <= raw.len() => {
                let v = i64::from_le_bytes(raw[i..i + 8].try_into().unwrap_or([0; 8]));
                i += 8;
                Datum::I64(v)
            }
            TAG_F64 if i + 8 <= raw.len() => {
                let v = f64::from_le_bytes(raw[i..i + 8].try_into().unwrap_or([0; 8]));
                i += 8;
                Datum::F64(v)
            }
            TAG_BYTES if i + 4 <= raw.len() => {
                let len = u32::from_le_bytes(raw[i..i + 4].try_into().unwrap_or([0; 4])) as usize;
                i += 4;
                let end = (i + len).min(raw.len());
                let bytes = raw[i..end].to_vec();
                i = end;
                Datum::Bytes(bytes)
            }
            _ => Datum::Null,
        };
        out.push((id, value));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_row_key_layout() {
        let key = encode_row_key_with_handle(1, 1);
        let mut expected = Vec::new();
        expected.extend_from_slice(b"t");
        encode_int(&mut expected, 1);
        expected.extend_from_slice(b"_r");
        encode_int(&mut expected, 1);
        assert_eq!(key.as_bytes(), expected.as_slice());
        assert_eq!(
            key.as_bytes(),
            hex("7480000000000000015f728000000000000001")
        );
        assert_eq!(decode_handle_from_row_key(key.as_bytes()).unwrap(), 1);
    }

    #[test]
    fn test_encode_index_seek_key() {
        let key = encode_index_seek_key(2, 3, b"abc");
        let raw = key.as_bytes();
        assert!(raw.starts_with(b"t"));
        assert!(raw.windows(2).any(|w| w == b"_i"));
        assert!(raw.ends_with(b"abc"));
    }

    #[test]
    fn test_encode_index_key_roundtrip_handle() {
        let key = encode_index_key(2, 1, &[Datum::Bytes(b"abc".to_vec())], 9, false);
        assert_eq!(decode_handle_from_index_key(key.as_bytes()).unwrap(), 9);
        let prefix = encode_index_seek_prefix(2, 1, &[Datum::Bytes(b"abc".to_vec())]);
        assert!(key.as_bytes().starts_with(prefix.as_bytes()));
        let (values, handle) = decode_index_key_values(key.as_bytes(), 1, false).unwrap();
        assert_eq!(values, vec![Datum::Bytes(b"abc".to_vec())]);
        assert_eq!(handle, 9);
        let unique = encode_index_key(2, 1, &[Datum::I64(4)], 0, true);
        let (uvals, _) = decode_index_key_values(unique.as_bytes(), 1, true).unwrap();
        assert_eq!(uvals, vec![Datum::I64(4)]);
    }

    #[test]
    fn test_rowcodec_roundtrip() {
        let cols = [
            (1, FieldType::new_longlong()),
            (2, FieldType::new_string()),
            (3, FieldType::new_double()),
        ];
        let values = vec![
            Datum::I64(42),
            Datum::Bytes(b"hello".to_vec()),
            Datum::F64(1.5),
        ];
        let encoded = encode_row(&[1, 2, 3], &values);
        assert_eq!(encoded[0], CODEC_VER);
        let decoded = decode_row(&encoded, &cols);
        assert_eq!(
            decoded,
            vec![
                (1, values[0].clone()),
                (2, values[1].clone()),
                (3, values[2].clone())
            ]
        );
        let with_null = encode_row(&[1, 2], &[Datum::I64(7), Datum::Null]);
        let decoded_null = decode_row(
            &with_null,
            &[(1, FieldType::new_longlong()), (2, FieldType::new_string())],
        );
        assert_eq!(decoded_null, vec![(1, Datum::I64(7)), (2, Datum::Null)]);
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
}
