use super::number::{decode_uint, decode_uint_desc, encode_uint, encode_uint_desc};
use sealdb_errno::Result;

const SIGN_MASK: u64 = 0x8000_0000_0000_0000;

fn encode_float_to_cmp_uint64(f: f64) -> u64 {
    let mut u = f.to_bits();
    if f >= 0.0 {
        u |= SIGN_MASK;
    } else {
        u = !u;
    }
    u
}

fn decode_cmp_uint_to_float(mut u: u64) -> f64 {
    if u & SIGN_MASK > 0 {
        u &= !SIGN_MASK;
    } else {
        u = !u;
    }
    f64::from_bits(u)
}

pub fn encode_float(buf: &mut Vec<u8>, v: f64) {
    encode_uint(buf, encode_float_to_cmp_uint64(v));
}

pub fn decode_float(b: &[u8]) -> Result<(&[u8], f64)> {
    let (rest, u) = decode_uint(b)?;
    Ok((rest, decode_cmp_uint_to_float(u)))
}

pub fn encode_float_desc(buf: &mut Vec<u8>, v: f64) {
    encode_uint_desc(buf, encode_float_to_cmp_uint64(v));
}

pub fn decode_float_desc(b: &[u8]) -> Result<(&[u8], f64)> {
    let (rest, u) = decode_uint_desc(b)?;
    Ok((rest, decode_cmp_uint_to_float(u)))
}
