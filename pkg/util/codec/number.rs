use sealdb_errno::{Error, Result};

const SIGN_MASK: u64 = 0x8000_0000_0000_0000;

pub fn encode_int_to_cmp_uint(v: i64) -> u64 {
    (v as u64) ^ SIGN_MASK
}

pub fn decode_cmp_uint_to_int(u: u64) -> i64 {
    (u ^ SIGN_MASK) as i64
}

pub fn encode_int(buf: &mut Vec<u8>, v: i64) {
    buf.extend_from_slice(&encode_int_to_cmp_uint(v).to_be_bytes());
}

pub fn encode_int_desc(buf: &mut Vec<u8>, v: i64) {
    buf.extend_from_slice(&(!encode_int_to_cmp_uint(v)).to_be_bytes());
}

pub fn decode_int(b: &[u8]) -> Result<(&[u8], i64)> {
    if b.len() < 8 {
        return Err(Error::new(
            sealdb_errno::ERR_INVALID_KEY,
            "insufficient bytes to decode value",
        ));
    }
    let u = u64::from_be_bytes(b[..8].try_into().expect("8 bytes"));
    Ok((&b[8..], decode_cmp_uint_to_int(u)))
}

pub fn decode_int_desc(b: &[u8]) -> Result<(&[u8], i64)> {
    if b.len() < 8 {
        return Err(Error::new(
            sealdb_errno::ERR_INVALID_KEY,
            "insufficient bytes to decode value",
        ));
    }
    let u = u64::from_be_bytes(b[..8].try_into().expect("8 bytes"));
    Ok((&b[8..], decode_cmp_uint_to_int(!u)))
}

pub fn encode_uint(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_be_bytes());
}

pub fn encode_uint_desc(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&(!v).to_be_bytes());
}

pub fn decode_uint(b: &[u8]) -> Result<(&[u8], u64)> {
    if b.len() < 8 {
        return Err(Error::new(
            sealdb_errno::ERR_INVALID_KEY,
            "insufficient bytes to decode value",
        ));
    }
    let v = u64::from_be_bytes(b[..8].try_into().expect("8 bytes"));
    Ok((&b[8..], v))
}

pub fn decode_uint_desc(b: &[u8]) -> Result<(&[u8], u64)> {
    if b.len() < 8 {
        return Err(Error::new(
            sealdb_errno::ERR_INVALID_KEY,
            "insufficient bytes to decode value",
        ));
    }
    let v = u64::from_be_bytes(b[..8].try_into().expect("8 bytes"));
    Ok((&b[8..], !v))
}

pub fn encode_varint(buf: &mut Vec<u8>, v: i64) {
    let mut tmp = [0u8; 10];
    let n = encode_zigzag(&mut tmp, v);
    buf.extend_from_slice(&tmp[..n]);
}

pub fn decode_varint(b: &[u8]) -> Result<(&[u8], i64)> {
    let (rest, u) = decode_uvarint(b)?;
    Ok((rest, decode_zigzag(u)))
}

pub fn encode_uvarint(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

pub fn decode_uvarint(mut b: &[u8]) -> Result<(&[u8], u64)> {
    let mut x = 0u64;
    let mut s = 0u32;
    for _ in 0..10 {
        if b.is_empty() {
            return Err(Error::new(
                sealdb_errno::ERR_INVALID_KEY,
                "insufficient bytes to decode value",
            ));
        }
        let byte = b[0];
        b = &b[1..];
        if byte < 0x80 {
            if s == 63 && byte > 1 {
                return Err(Error::new(
                    sealdb_errno::ERR_INVALID_KEY,
                    "value larger than 64 bits",
                ));
            }
            return Ok((b, x | (u64::from(byte) << s)));
        }
        x |= u64::from(byte & 0x7f) << s;
        s += 7;
    }
    Err(Error::new(
        sealdb_errno::ERR_INVALID_KEY,
        "value larger than 64 bits",
    ))
}

fn encode_zigzag(dst: &mut [u8], v: i64) -> usize {
    let u = ((v << 1) ^ (v >> 63)) as u64;
    let mut n = 0;
    let mut x = u;
    while x >= 0x80 {
        dst[n] = (x as u8) | 0x80;
        x >>= 7;
        n += 1;
    }
    dst[n] = x as u8;
    n + 1
}

fn decode_zigzag(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -((u & 1) as i64)
}
