use sealdb_errno::{Error, Result};

const ENC_GROUP_SIZE: usize = 8;
const ENC_MARKER: u8 = 0xFF;
const ENC_PAD: u8 = 0x00;

pub fn encode_bytes(buf: &mut Vec<u8>, data: &[u8]) {
    let mut idx = 0;
    loop {
        let remain = data.len().saturating_sub(idx);
        if remain >= ENC_GROUP_SIZE {
            buf.extend_from_slice(&data[idx..idx + ENC_GROUP_SIZE]);
            buf.push(ENC_MARKER);
            idx += ENC_GROUP_SIZE;
            if idx == data.len() {
                buf.extend_from_slice(&[0; ENC_GROUP_SIZE]);
                buf.push(ENC_MARKER - ENC_GROUP_SIZE as u8);
                break;
            }
        } else {
            buf.extend_from_slice(&data[idx..]);
            let pad_count = ENC_GROUP_SIZE - remain;
            buf.extend(std::iter::repeat_n(ENC_PAD, pad_count));
            buf.push(ENC_MARKER - pad_count as u8);
            break;
        }
    }
}

pub fn encoded_bytes_length(data_len: usize) -> usize {
    let modulus = data_len % ENC_GROUP_SIZE;
    let pad_count = ENC_GROUP_SIZE - modulus;
    data_len + pad_count + 1 + data_len / ENC_GROUP_SIZE
}

pub fn decode_bytes(b: &[u8], reverse: bool) -> Result<(&[u8], Vec<u8>)> {
    let mut buf = Vec::with_capacity(b.len());
    let mut rest = b;
    loop {
        if rest.len() < ENC_GROUP_SIZE + 1 {
            return Err(Error::new(
                sealdb_errno::ERR_INVALID_KEY,
                "insufficient bytes to decode value",
            ));
        }
        let group = &rest[..ENC_GROUP_SIZE];
        let marker = rest[ENC_GROUP_SIZE];
        let pad_count = if reverse { marker } else { ENC_MARKER - marker };
        if pad_count as usize > ENC_GROUP_SIZE {
            return Err(Error::new(
                sealdb_errno::ERR_INVALID_KEY,
                "invalid marker byte",
            ));
        }
        let real = ENC_GROUP_SIZE - pad_count as usize;
        buf.extend_from_slice(&group[..real]);
        rest = &rest[ENC_GROUP_SIZE + 1..];
        if pad_count != 0 {
            let pad_byte = if reverse { ENC_MARKER } else { ENC_PAD };
            if group[real..].iter().any(|v| *v != pad_byte) {
                return Err(Error::new(
                    sealdb_errno::ERR_INVALID_KEY,
                    "invalid padding byte",
                ));
            }
            break;
        }
    }
    if reverse {
        buf.iter_mut().for_each(|x| *x = !*x);
    }
    Ok((rest, buf))
}

pub fn encode_bytes_desc(buf: &mut Vec<u8>, data: &[u8]) {
    let start = buf.len();
    encode_bytes(buf, data);
    buf[start..].iter_mut().for_each(|x| *x = !*x);
}

pub fn decode_bytes_desc(b: &[u8]) -> Result<(&[u8], Vec<u8>)> {
    decode_bytes(b, true)
}
