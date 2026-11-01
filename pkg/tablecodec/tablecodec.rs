use sealdb_errno::{Error, Result};
use sealdb_kv::Key;
use sealdb_util::codec::{decode_int, encode_int, encode_int_to_cmp_uint};

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

pub fn encode_row(col_ids: &[i64], values: &[sealdb_types::Datum]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&(col_ids.len() as u32).to_le_bytes());
    for (id, value) in col_ids.iter().zip(values.iter()) {
        buf.extend_from_slice(&id.to_le_bytes());
        match value {
            sealdb_types::Datum::Null => buf.push(TAG_NULL),
            sealdb_types::Datum::I64(v) => {
                buf.push(TAG_I64);
                buf.extend_from_slice(&v.to_le_bytes());
            }
            sealdb_types::Datum::U64(v) => {
                buf.push(TAG_I64);
                buf.extend_from_slice(&(*v as i64).to_le_bytes());
            }
            sealdb_types::Datum::F64(v) => {
                buf.push(TAG_F64);
                buf.extend_from_slice(&v.to_le_bytes());
            }
            other => {
                let bytes = other.as_bytes();
                buf.push(TAG_BYTES);
                buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                buf.extend_from_slice(&bytes);
            }
        }
    }
    buf
}

pub fn decode_row(raw: &[u8]) -> Vec<(i64, sealdb_types::Datum)> {
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
            TAG_NULL => sealdb_types::Datum::Null,
            TAG_I64 if i + 8 <= raw.len() => {
                let v = i64::from_le_bytes(raw[i..i + 8].try_into().unwrap_or([0; 8]));
                i += 8;
                sealdb_types::Datum::I64(v)
            }
            TAG_F64 if i + 8 <= raw.len() => {
                let v = f64::from_le_bytes(raw[i..i + 8].try_into().unwrap_or([0; 8]));
                i += 8;
                sealdb_types::Datum::F64(v)
            }
            TAG_BYTES if i + 4 <= raw.len() => {
                let len = u32::from_le_bytes(raw[i..i + 4].try_into().unwrap_or([0; 4])) as usize;
                i += 4;
                let end = (i + len).min(raw.len());
                let bytes = raw[i..end].to_vec();
                i = end;
                sealdb_types::Datum::Bytes(bytes)
            }
            _ => sealdb_types::Datum::Null,
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

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
}
