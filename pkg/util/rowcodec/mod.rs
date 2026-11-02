// TiDB `pkg/util/rowcodec`: new row format (CodecVer 128).
// Completeness wave / not this leftover: checksum encode/verify, JSON, vector.

pub mod common;
pub mod decoder;
pub mod encoder;
pub mod row;

pub use common::CODEC_VER;
pub use decoder::decode;
pub use encoder::encode;

#[cfg(test)]
mod tests {
    use super::*;
    use sealdb_types::{Datum, FieldType};

    #[test]
    fn test_encode_decode_int_and_bytes() {
        let cols = [(1, FieldType::new_longlong()), (2, FieldType::new_string())];
        let values = [Datum::I64(-7), Datum::Bytes(b"ab".to_vec())];
        let raw = encode(&[1, 2], &values);
        assert_eq!(raw[0], CODEC_VER);
        assert_eq!(
            decode(&raw, &cols),
            vec![(1, values[0].clone()), (2, values[1].clone())]
        );
    }

    #[test]
    fn test_encode_decode_null_and_float() {
        let cols = [
            (1, FieldType::new_longlong()),
            (2, FieldType::new_double()),
            (3, FieldType::new_string()),
        ];
        let values = [Datum::I64(1), Datum::F64(2.5), Datum::Null];
        let raw = encode(&[1, 2, 3], &values);
        assert_eq!(
            decode(&raw, &cols),
            vec![(1, Datum::I64(1)), (2, Datum::F64(2.5)), (3, Datum::Null)]
        );
    }
}
