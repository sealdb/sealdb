//! TiDB `util/codec` EncodeValue / DecodeOne (TypeDefault Cop rows).

use super::{
    decode_bytes, decode_float, decode_int, decode_uint, decode_uvarint, decode_varint,
    encode_bytes, encode_float, encode_int, encode_uint, encode_varint,
};
use sealdb_errno::{ERR_INVALID_KEY, Error, Result};
use sealdb_types::Datum;

pub const NIL_FLAG: u8 = 0;
pub const BYTES_FLAG: u8 = 1;
pub const COMPACT_BYTES_FLAG: u8 = 2;
pub const INT_FLAG: u8 = 3;
pub const UINT_FLAG: u8 = 4;
pub const FLOAT_FLAG: u8 = 5;
pub const DECIMAL_FLAG: u8 = 6;
pub const DURATION_FLAG: u8 = 7;
pub const VARINT_FLAG: u8 = 8;
pub const UVARINT_FLAG: u8 = 9;

/// Encode one datum the way TiDB `EncodeValue` does for Cop TypeDefault rows.
pub fn encode_value(buf: &mut Vec<u8>, value: &Datum) {
    match value {
        Datum::Null => buf.push(NIL_FLAG),
        Datum::I64(v) => {
            buf.push(INT_FLAG);
            encode_int(buf, *v);
        }
        Datum::U64(v) => {
            buf.push(UINT_FLAG);
            encode_uint(buf, *v);
        }
        Datum::F64(v) => {
            buf.push(FLOAT_FLAG);
            encode_float(buf, *v);
        }
        Datum::Bytes(b) => {
            buf.push(BYTES_FLAG);
            encode_bytes(buf, b);
        }
        Datum::Bit(b) => {
            buf.push(UINT_FLAG);
            encode_uint(buf, b.iter().fold(0u64, |acc, x| (acc << 8) | u64::from(*x)));
        }
        Datum::Decimal(d) => {
            // MVB: UTF-8 decimal string after a compact-bytes length (TiDB uses
            // MyDecimal bin; string form is enough for round-trip in tests).
            let s = d.to_string();
            buf.push(DECIMAL_FLAG);
            encode_compact_bytes(buf, s.as_bytes());
        }
        Datum::Time(t) => {
            buf.push(UINT_FLAG);
            encode_uint(buf, t.to_packed_u64());
        }
        // Coprocessor rows carry the enum/set label text.
        Datum::Enum(e) => {
            buf.push(BYTES_FLAG);
            encode_bytes(buf, &e.label);
        }
    }
}

/// Decode one EncodeValue datum; returns remaining bytes.
pub fn decode_one(b: &[u8]) -> Result<(&[u8], Datum)> {
    if b.is_empty() {
        return Err(Error::new(ERR_INVALID_KEY, "invalid encoded key"));
    }
    let flag = b[0];
    let rest = &b[1..];
    match flag {
        NIL_FLAG => Ok((rest, Datum::Null)),
        INT_FLAG => {
            let (rest, v) = decode_int(rest)?;
            Ok((rest, Datum::I64(v)))
        }
        UINT_FLAG => {
            let (rest, v) = decode_uint(rest)?;
            Ok((rest, Datum::U64(v)))
        }
        VARINT_FLAG => {
            let (rest, v) = decode_varint(rest)?;
            Ok((rest, Datum::I64(v)))
        }
        UVARINT_FLAG => {
            let (rest, v) = decode_uvarint(rest)?;
            Ok((rest, Datum::U64(v)))
        }
        FLOAT_FLAG => {
            let (rest, v) = decode_float(rest)?;
            Ok((rest, Datum::F64(v)))
        }
        BYTES_FLAG => {
            let (rest, v) = decode_bytes(rest, false)?;
            Ok((rest, Datum::Bytes(v)))
        }
        COMPACT_BYTES_FLAG => {
            let (rest, v) = decode_compact_bytes(rest)?;
            Ok((rest, Datum::Bytes(v)))
        }
        DECIMAL_FLAG => {
            let (rest, v) = decode_compact_bytes(rest)?;
            let s = String::from_utf8_lossy(&v);
            let dec = sealdb_types::MyDecimal::parse(&s)
                .map_err(|e| Error::new(ERR_INVALID_KEY, format!("decimal decode: {e}")))?;
            Ok((rest, Datum::Decimal(dec)))
        }
        DURATION_FLAG => {
            let (rest, v) = decode_int(rest)?;
            Ok((rest, Datum::I64(v)))
        }
        other => Err(Error::new(
            ERR_INVALID_KEY,
            format!("invalid encoded key flag {other}"),
        )),
    }
}

fn encode_compact_bytes(buf: &mut Vec<u8>, v: &[u8]) {
    encode_varint(buf, v.len() as i64);
    buf.extend_from_slice(v);
}

fn decode_compact_bytes(b: &[u8]) -> Result<(&[u8], Vec<u8>)> {
    let (rest, n) = decode_varint(b)?;
    if n < 0 || n as usize > rest.len() {
        return Err(Error::new(ERR_INVALID_KEY, "invalid compact bytes length"));
    }
    let n = n as usize;
    Ok((&rest[n..], rest[..n].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_roundtrip() {
        let values = [
            Datum::Null,
            Datum::I64(-7),
            Datum::U64(42),
            Datum::F64(1.5),
            Datum::Bytes(b"hi".to_vec()),
        ];
        let mut buf = Vec::new();
        for v in &values {
            encode_value(&mut buf, v);
        }
        let mut rest = buf.as_slice();
        for expect in &values {
            let (next, got) = decode_one(rest).unwrap();
            assert_eq!(&got, expect);
            rest = next;
        }
        assert!(rest.is_empty());
    }
}
