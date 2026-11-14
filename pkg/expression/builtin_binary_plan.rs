//! Decoding support for `tidb_decode_binary_plan`.
//!
//! The builtin takes the base64 text form of a binary plan (base64 of a
//! snappy-compressed protobuf payload) and renders it as text. This module
//! implements the decoding steps that can fail, mirroring TiDB's error
//! messages byte for byte:
//!
//! * `illegal base64 data at input byte N` — standard (padded) base64 decode.
//! * `snappy: corrupt input` — snappy block decompression.
//! * `proto: illegal wireType N` — protobuf wire-format validation.
//!
//! The textual rendering of a successfully decoded plan requires the binary
//! plan protobuf schema, which this crate does not model yet, so a valid
//! payload currently decodes to an empty plan. The failure paths (the ones
//! users hit with malformed input) are complete; see `planner/core/binary_plan`
//! in the integration tests.

/// Decode a base64 binary-plan payload into its plan text.
///
/// On failure the returned string is TiDB's decode error message; callers turn
/// it into a warning and return the empty string.
pub fn decode_binary_plan(encoded: &str) -> Result<String, String> {
    let compressed = base64_decode_std(encoded)?;
    let payload = snappy_decode(&compressed)?;
    check_protobuf(&payload)?;
    // A well-formed payload decodes to the plan's text form. The plan schema is
    // not modeled yet, so there is nothing to render.
    Ok(String::new())
}

/// Standard (padded) base64 decode with Go-compatible error messages.
fn base64_decode_std(input: &str) -> Result<Vec<u8>, String> {
    fn value(byte: u8, index: usize) -> Result<u8, String> {
        match byte {
            b'A'..=b'Z' => Ok(byte - b'A'),
            b'a'..=b'z' => Ok(byte - b'a' + 26),
            b'0'..=b'9' => Ok(byte - b'0' + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err(format!("illegal base64 data at input byte {index}")),
        }
    }

    // Go's decoder skips CR and LF anywhere in the stream.
    let bytes: Vec<u8> = input
        .bytes()
        .filter(|byte| *byte != b'\r' && *byte != b'\n')
        .collect();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut index = 0;
    while index < bytes.len() {
        let remaining = bytes.len() - index;
        if remaining < 4 {
            return Err(format!("illegal base64 data at input byte {index}"));
        }
        let a = value(bytes[index], index)?;
        let b = value(bytes[index + 1], index + 1)?;
        let third = bytes[index + 2];
        let fourth = bytes[index + 3];
        let after = index + 4;
        if third == b'=' {
            if fourth != b'=' || after != bytes.len() {
                let bad = if fourth != b'=' { after - 1 } else { after };
                return Err(format!("illegal base64 data at input byte {bad}"));
            }
            out.push((a << 2) | (b >> 4));
            index = after;
            continue;
        }
        let c = value(third, index + 2)?;
        if fourth == b'=' {
            if after != bytes.len() {
                return Err(format!("illegal base64 data at input byte {after}"));
            }
            out.push((a << 2) | (b >> 4));
            out.push((b << 4) | (c >> 2));
            index = after;
            continue;
        }
        let d = value(fourth, index + 3)?;
        out.push((a << 2) | (b >> 4));
        out.push((b << 4) | (c >> 2));
        out.push((c << 6) | d);
        index = after;
    }
    Ok(out)
}

/// Snappy block-format decompression; every failure is `snappy: corrupt input`
/// like the reference implementation.
fn snappy_decode(input: &[u8]) -> Result<Vec<u8>, String> {
    const CORRUPT: &str = "snappy: corrupt input";
    let mut pos = 0usize;
    let mut expected = 0usize;
    let mut shift = 0u32;
    loop {
        if pos >= input.len() {
            return Err(CORRUPT.to_string());
        }
        let byte = input[pos];
        pos += 1;
        expected |= ((byte & 0x7f) as usize) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift > 28 {
            return Err(CORRUPT.to_string());
        }
    }
    let mut out: Vec<u8> = Vec::with_capacity(expected);
    while pos < input.len() {
        let tag = input[pos];
        pos += 1;
        match tag & 0x03 {
            0 => {
                let mut len = (tag >> 2) as usize;
                if len >= 60 {
                    let extra = len - 59;
                    if pos + extra > input.len() {
                        return Err(CORRUPT.to_string());
                    }
                    let mut value = 0usize;
                    for offset in 0..extra {
                        value |= (input[pos + offset] as usize) << (8 * offset);
                    }
                    pos += extra;
                    len = value;
                }
                len += 1;
                if pos + len > input.len() || out.len() + len > expected {
                    return Err(CORRUPT.to_string());
                }
                out.extend_from_slice(&input[pos..pos + len]);
                pos += len;
            }
            1 => {
                let len = ((tag >> 2) & 0x07) as usize + 4;
                if pos >= input.len() {
                    return Err(CORRUPT.to_string());
                }
                let offset = (((tag >> 5) as usize) << 8) | input[pos] as usize;
                pos += 1;
                copy_from_output(&mut out, offset, len, expected)?;
            }
            2 => {
                let len = (tag >> 2) as usize + 1;
                if pos + 2 > input.len() {
                    return Err(CORRUPT.to_string());
                }
                let offset = input[pos] as usize | ((input[pos + 1] as usize) << 8);
                pos += 2;
                copy_from_output(&mut out, offset, len, expected)?;
            }
            _ => {
                let len = (tag >> 2) as usize + 1;
                if pos + 4 > input.len() {
                    return Err(CORRUPT.to_string());
                }
                let offset = input[pos] as usize
                    | ((input[pos + 1] as usize) << 8)
                    | ((input[pos + 2] as usize) << 16)
                    | ((input[pos + 3] as usize) << 24);
                pos += 4;
                copy_from_output(&mut out, offset, len, expected)?;
            }
        }
    }
    if out.len() != expected {
        return Err(CORRUPT.to_string());
    }
    Ok(out)
}

fn copy_from_output(
    out: &mut Vec<u8>,
    offset: usize,
    len: usize,
    expected: usize,
) -> Result<(), String> {
    if offset == 0 || offset > out.len() || out.len() + len > expected {
        return Err("snappy: corrupt input".to_string());
    }
    let start = out.len() - offset;
    for step in 0..len {
        let byte = out[start + step];
        out.push(byte);
    }
    Ok(())
}

/// Walk the protobuf wire format, reporting illegal wire types the way the
/// reference protobuf library does (`proto: illegal wireType N`).
fn check_protobuf(payload: &[u8]) -> Result<(), String> {
    let mut pos = 0usize;
    skip_fields(payload, &mut pos, false)
}

fn read_varint(payload: &[u8], pos: &mut usize) -> Result<u64, String> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        if *pos >= payload.len() {
            return Err("unexpected EOF".to_string());
        }
        let byte = payload[*pos];
        *pos += 1;
        if shift >= 64 {
            return Err("proto: integer overflow".to_string());
        }
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
    }
}

fn skip_fields(payload: &[u8], pos: &mut usize, in_group: bool) -> Result<(), String> {
    while *pos < payload.len() {
        let tag = read_varint(payload, pos)?;
        let wire_type = (tag & 0x07) as u8;
        let field = tag >> 3;
        if field == 0 {
            return Err("proto: illegal tag 0".to_string());
        }
        match wire_type {
            0 => {
                read_varint(payload, pos)?;
            }
            1 => {
                if *pos + 8 > payload.len() {
                    return Err("unexpected EOF".to_string());
                }
                *pos += 8;
            }
            2 => {
                let len = read_varint(payload, pos)? as usize;
                if *pos + len > payload.len() {
                    return Err("unexpected EOF".to_string());
                }
                *pos += len;
            }
            3 => {
                // A nested group consumes its own end-group tag before
                // returning, so the outer loop simply continues.
                skip_fields(payload, pos, true)?;
            }
            4 => {
                if in_group {
                    return Ok(());
                }
                return Err("proto: unexpected end group".to_string());
            }
            5 => {
                if *pos + 4 > payload.len() {
                    return Err("unexpected EOF".to_string());
                }
                *pos += 4;
            }
            other => return Err(format!("proto: illegal wireType {other}")),
        }
    }
    if in_group {
        return Err("unexpected EOF".to_string());
    }
    Ok(())
}
