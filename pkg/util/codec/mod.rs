pub mod bytes;
pub mod datum;
pub mod float;
pub mod number;

pub use bytes::{
    decode_bytes, decode_bytes_desc, encode_bytes, encode_bytes_desc, encoded_bytes_length,
};
pub use datum::{BYTES_FLAG, INT_FLAG, NIL_FLAG, UINT_FLAG, decode_one, encode_value};
pub use float::{decode_float, decode_float_desc, encode_float, encode_float_desc};
pub use number::{
    decode_int, decode_int_desc, decode_uint, decode_uint_desc, decode_uvarint, decode_varint,
    encode_int, encode_int_desc, encode_int_to_cmp_uint, encode_uint, encode_uint_desc,
    encode_uvarint, encode_varint,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_int_known_vectors() {
        let mut buf = Vec::new();
        encode_int(&mut buf, 0);
        assert_eq!(buf, hex("8000000000000000"));
        buf.clear();
        encode_int(&mut buf, 1);
        assert_eq!(buf, hex("8000000000000001"));
        buf.clear();
        encode_int(&mut buf, -1);
        assert_eq!(buf, hex("7fffffffffffffff"));
        let (_, v) = decode_int(&buf).unwrap();
        assert_eq!(v, -1);
    }

    #[test]
    fn test_encode_uint_and_float() {
        let mut buf = Vec::new();
        encode_uint(&mut buf, 1);
        assert_eq!(buf, hex("0000000000000001"));
        buf.clear();
        encode_float(&mut buf, 1.5);
        let (rest, v) = decode_float(&buf).unwrap();
        assert!(rest.is_empty());
        assert_eq!(v, 1.5);
        assert!(encode_float_bytes(-1.0) < encode_float_bytes(0.0));
        assert!(encode_float_bytes(0.0) < encode_float_bytes(1.0));
    }

    #[test]
    fn test_encode_bytes_known_vectors() {
        let mut buf = Vec::new();
        encode_bytes(&mut buf, &[]);
        assert_eq!(buf, hex("0000000000000000f7"));
        buf.clear();
        encode_bytes(&mut buf, &[1, 2, 3]);
        assert_eq!(buf, hex("0102030000000000fa"));
        buf.clear();
        encode_bytes(&mut buf, &[1, 2, 3, 0]);
        assert_eq!(buf, hex("0102030000000000fb"));
        buf.clear();
        encode_bytes(&mut buf, &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(buf, hex("0102030405060708ff0000000000000000f7"));
        let (_, decoded) = decode_bytes(&buf, false).unwrap();
        assert_eq!(decoded, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    fn encode_float_bytes(v: f64) -> Vec<u8> {
        let mut buf = Vec::new();
        encode_float(&mut buf, v);
        buf
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
}
