use sealdb_kv::Key;
use sealdb_util::codec::{encode_int, encode_int_to_cmp_uint};

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
