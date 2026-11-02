/// TiDB new-row format version byte (`CodecVer` in `pkg/util/rowcodec/common.go`).
pub const CODEC_VER: u8 = 128;
/// Column ids or offsets use 4 bytes instead of 1/2 when set.
pub const ROW_FLAG_LARGE: u8 = 1;
/// Checksum trailer present. Encode currently strips this flag (NoChecksum).
pub const ROW_FLAG_CHECKSUM: u8 = 2;

/// Compact little-endian signed int: 1, 2, 4, or 8 bytes depending on magnitude.
pub fn encode_int(buf: &mut Vec<u8>, v: i64) {
    if v == i64::from(v as i8) {
        buf.push(v as u8);
    } else if v == i64::from(v as i16) {
        buf.extend_from_slice(&(v as i16).to_le_bytes());
    } else if v == i64::from(v as i32) {
        buf.extend_from_slice(&(v as i32).to_le_bytes());
    } else {
        buf.extend_from_slice(&v.to_le_bytes());
    }
}

/// Inverse of `encode_int`; `val.len()` selects the compact width.
pub fn decode_int(val: &[u8]) -> i64 {
    match val.len() {
        1 => val[0] as i8 as i64,
        2 => i16::from_le_bytes(val.try_into().unwrap_or([0; 2])) as i64,
        4 => i32::from_le_bytes(val.try_into().unwrap_or([0; 4])) as i64,
        _ => i64::from_le_bytes(pad8(val)),
    }
}

/// Compact little-endian unsigned int: 1, 2, 4, or 8 bytes depending on magnitude.
pub fn encode_uint(buf: &mut Vec<u8>, v: u64) {
    if v == u64::from(v as u8) {
        buf.push(v as u8);
    } else if v == u64::from(v as u16) {
        buf.extend_from_slice(&(v as u16).to_le_bytes());
    } else if v == u64::from(v as u32) {
        buf.extend_from_slice(&(v as u32).to_le_bytes());
    } else {
        buf.extend_from_slice(&v.to_le_bytes());
    }
}

/// Inverse of `encode_uint`; `val.len()` selects the compact width.
pub fn decode_uint(val: &[u8]) -> u64 {
    match val.len() {
        1 => u64::from(val[0]),
        2 => u64::from(u16::from_le_bytes(val.try_into().unwrap_or([0; 2]))),
        4 => u64::from(u32::from_le_bytes(val.try_into().unwrap_or([0; 4]))),
        _ => u64::from_le_bytes(pad8(val)),
    }
}

/// Zero-extend `val` to 8 bytes for little-endian i64/u64 decode.
fn pad8(val: &[u8]) -> [u8; 8] {
    let mut out = [0u8; 8];
    let n = val.len().min(8);
    out[..n].copy_from_slice(&val[..n]);
    out
}
