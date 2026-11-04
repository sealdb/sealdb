//! TiDB `util/chunk.Codec` — tipb EncodeType_TypeChunk column layout.
//!
//! Per-column layout: `len:u32le`, `nullCount:u32le`, optional nullBitmap,
//! optional offsets (varlen), then data. Fixed widths match TiDB `getFixedLen`.

use sealdb_errno::{ERR_UNKNOWN_ERROR, Error, Result};
use sealdb_types::{
    Datum, TYPE_DATE, TYPE_DATETIME, TYPE_DOUBLE, TYPE_DURATION, TYPE_FLOAT, TYPE_INT24, TYPE_LONG,
    TYPE_LONG_LONG, TYPE_NEW_DECIMAL, TYPE_SHORT, TYPE_TIMESTAMP, TYPE_TINY, TYPE_YEAR,
};

const VAR_ELEM_LEN: i32 = -1;
const SIZE_TIME: usize = 16;
const SIZE_DECIMAL: usize = 40;

/// Fixed element length for a MySQL type code, or -1 for variable length.
pub fn fixed_len(tp: u8) -> i32 {
    match tp {
        TYPE_FLOAT => 4,
        TYPE_TINY | TYPE_SHORT | TYPE_INT24 | TYPE_LONG | TYPE_LONG_LONG | TYPE_DOUBLE
        | TYPE_YEAR | TYPE_DURATION => 8,
        TYPE_DATE | TYPE_DATETIME | TYPE_TIMESTAMP => SIZE_TIME as i32,
        TYPE_NEW_DECIMAL => SIZE_DECIMAL as i32,
        _ => VAR_ELEM_LEN,
    }
}

/// Encode columnar TypeChunk bytes from row-oriented Datums.
pub fn encode_chunk(col_tps: &[u8], rows: &[Vec<Datum>]) -> Result<Vec<u8>> {
    if col_tps.is_empty() {
        return Ok(Vec::new());
    }
    let n = rows.len();
    let mut out = Vec::new();
    for (ci, &tp) in col_tps.iter().enumerate() {
        let mut nulls = vec![false; n];
        let mut values: Vec<Option<Vec<u8>>> = Vec::with_capacity(n);
        for row in rows {
            let d = row.get(ci).unwrap_or(&Datum::Null);
            if d.is_null() {
                nulls[values.len()] = true;
                values.push(None);
            } else {
                values.push(Some(encode_cell(tp, d)?));
            }
        }
        encode_column(&mut out, tp, &nulls, &values)?;
    }
    Ok(out)
}

/// Decode TypeChunk bytes into row-oriented Datums.
pub fn decode_chunk(col_tps: &[u8], mut buffer: &[u8]) -> Result<Vec<Vec<Datum>>> {
    if col_tps.is_empty() {
        return Ok(Vec::new());
    }
    let mut columns: Vec<Vec<Datum>> = Vec::with_capacity(col_tps.len());
    let mut num_rows = None;
    for &tp in col_tps {
        let (rest, col, n) = decode_column(buffer, tp)?;
        buffer = rest;
        if let Some(expect) = num_rows {
            if n != expect {
                return Err(Error::new(
                    ERR_UNKNOWN_ERROR,
                    format!("tipb chunk column length mismatch {n} vs {expect}"),
                ));
            }
        } else {
            num_rows = Some(n);
        }
        columns.push(col);
    }
    let n = num_rows.unwrap_or(0);
    let mut rows = vec![Vec::with_capacity(col_tps.len()); n];
    for col in columns {
        for (i, cell) in col.into_iter().enumerate() {
            rows[i].push(cell);
        }
    }
    Ok(rows)
}

fn encode_column(
    out: &mut Vec<u8>,
    tp: u8,
    nulls: &[bool],
    values: &[Option<Vec<u8>>],
) -> Result<()> {
    let n = nulls.len();
    out.extend_from_slice(&(n as u32).to_le_bytes());
    let null_count = nulls.iter().filter(|n| **n).count() as u32;
    out.extend_from_slice(&null_count.to_le_bytes());
    if null_count > 0 {
        let nbytes = n.div_ceil(8);
        let mut bitmap = vec![0u8; nbytes];
        for (i, is_null) in nulls.iter().enumerate() {
            if !*is_null {
                bitmap[i / 8] |= 1 << (i % 8);
            }
        }
        out.extend_from_slice(&bitmap);
    }
    let fixed = fixed_len(tp);
    if fixed == VAR_ELEM_LEN {
        let mut offsets = Vec::with_capacity(n + 1);
        offsets.push(0i64);
        let mut data = Vec::new();
        for v in values {
            if let Some(bytes) = v {
                data.extend_from_slice(bytes);
            }
            offsets.push(data.len() as i64);
        }
        for off in offsets {
            out.extend_from_slice(&off.to_le_bytes());
        }
        out.extend_from_slice(&data);
    } else {
        let elem = fixed as usize;
        for v in values {
            match v {
                Some(bytes) => {
                    if bytes.len() != elem {
                        return Err(Error::new(
                            ERR_UNKNOWN_ERROR,
                            format!("fixed cell len {} != {elem}", bytes.len()),
                        ));
                    }
                    out.extend_from_slice(bytes);
                }
                None => out.extend(std::iter::repeat_n(0u8, elem)),
            }
        }
    }
    Ok(())
}

fn decode_column(buffer: &[u8], tp: u8) -> Result<(&[u8], Vec<Datum>, usize)> {
    if buffer.len() < 8 {
        return Err(Error::new(ERR_UNKNOWN_ERROR, "tipb chunk column header"));
    }
    let n = u32::from_le_bytes(buffer[0..4].try_into().unwrap()) as usize;
    let null_count = u32::from_le_bytes(buffer[4..8].try_into().unwrap()) as usize;
    let mut rest = &buffer[8..];
    let null_bitmap = if null_count > 0 {
        let nbytes = n.div_ceil(8);
        if rest.len() < nbytes {
            return Err(Error::new(ERR_UNKNOWN_ERROR, "tipb chunk null bitmap"));
        }
        let bm = &rest[..nbytes];
        rest = &rest[nbytes..];
        Some(bm.to_vec())
    } else {
        None
    };
    let is_null = |i: usize| -> bool {
        match &null_bitmap {
            None => false,
            Some(bm) => (bm[i / 8] & (1 << (i % 8))) == 0,
        }
    };
    let fixed = fixed_len(tp);
    let mut col = Vec::with_capacity(n);
    if fixed == VAR_ELEM_LEN {
        let offset_bytes = (n + 1) * 8;
        if rest.len() < offset_bytes {
            return Err(Error::new(ERR_UNKNOWN_ERROR, "tipb chunk offsets"));
        }
        let mut offsets = Vec::with_capacity(n + 1);
        for i in 0..=n {
            let start = i * 8;
            offsets.push(i64::from_le_bytes(
                rest[start..start + 8].try_into().unwrap(),
            ));
        }
        rest = &rest[offset_bytes..];
        let data_len = offsets[n] as usize;
        if rest.len() < data_len {
            return Err(Error::new(ERR_UNKNOWN_ERROR, "tipb chunk var data"));
        }
        let data = &rest[..data_len];
        rest = &rest[data_len..];
        for i in 0..n {
            if is_null(i) {
                col.push(Datum::Null);
            } else {
                let s = offsets[i] as usize;
                let e = offsets[i + 1] as usize;
                col.push(Datum::Bytes(data[s..e].to_vec()));
            }
        }
    } else {
        let elem = fixed as usize;
        let data_len = elem * n;
        if rest.len() < data_len {
            return Err(Error::new(ERR_UNKNOWN_ERROR, "tipb chunk fixed data"));
        }
        let data = &rest[..data_len];
        rest = &rest[data_len..];
        for i in 0..n {
            if is_null(i) {
                col.push(Datum::Null);
            } else {
                let cell = &data[i * elem..(i + 1) * elem];
                col.push(decode_fixed_cell(tp, cell)?);
            }
        }
    }
    Ok((rest, col, n))
}

fn encode_cell(tp: u8, d: &Datum) -> Result<Vec<u8>> {
    const TIME_LEN: i32 = SIZE_TIME as i32;
    const DEC_LEN: i32 = SIZE_DECIMAL as i32;
    match (fixed_len(tp), d) {
        (4, Datum::F64(v)) => Ok((*v as f32).to_bits().to_le_bytes().to_vec()),
        (4, Datum::I64(v)) => Ok((*v as f32).to_bits().to_le_bytes().to_vec()),
        (8, Datum::I64(v)) => Ok(v.to_le_bytes().to_vec()),
        (8, Datum::U64(v)) => Ok(v.to_le_bytes().to_vec()),
        (8, Datum::F64(v)) => Ok(v.to_bits().to_le_bytes().to_vec()),
        (TIME_LEN, Datum::Time(t)) => {
            let mut buf = t.to_packed_u64().to_le_bytes().to_vec();
            buf.resize(SIZE_TIME, 0);
            Ok(buf)
        }
        (TIME_LEN, Datum::U64(v)) => {
            let mut buf = v.to_le_bytes().to_vec();
            buf.resize(SIZE_TIME, 0);
            Ok(buf)
        }
        (DEC_LEN, Datum::Decimal(d)) => {
            let mut buf = d.to_string().into_bytes();
            buf.resize(SIZE_DECIMAL, 0);
            Ok(buf)
        }
        (VAR_ELEM_LEN, Datum::Bytes(b)) => Ok(b.clone()),
        (VAR_ELEM_LEN, Datum::I64(v)) => Ok(v.to_string().into_bytes()),
        (VAR_ELEM_LEN, Datum::U64(v)) => Ok(v.to_string().into_bytes()),
        (VAR_ELEM_LEN, other) => Ok(format!("{other:?}").into_bytes()),
        (fixed, other) => Err(Error::new(
            ERR_UNKNOWN_ERROR,
            format!("cannot encode {other:?} as fixed {fixed} for tp={tp}"),
        )),
    }
}

fn decode_fixed_cell(tp: u8, cell: &[u8]) -> Result<Datum> {
    match tp {
        TYPE_FLOAT => {
            let bits = u32::from_le_bytes(cell[..4].try_into().unwrap());
            Ok(Datum::F64(f32::from_bits(bits) as f64))
        }
        TYPE_DOUBLE => {
            let bits = u64::from_le_bytes(cell[..8].try_into().unwrap());
            Ok(Datum::F64(f64::from_bits(bits)))
        }
        TYPE_TINY | TYPE_SHORT | TYPE_INT24 | TYPE_LONG | TYPE_LONG_LONG | TYPE_YEAR
        | TYPE_DURATION => {
            let v = i64::from_le_bytes(cell[..8].try_into().unwrap());
            Ok(Datum::I64(v))
        }
        TYPE_DATE | TYPE_DATETIME | TYPE_TIMESTAMP => {
            let v = u64::from_le_bytes(cell[..8].try_into().unwrap());
            Ok(Datum::U64(v))
        }
        TYPE_NEW_DECIMAL => {
            let end = cell.iter().position(|&b| b == 0).unwrap_or(cell.len());
            let s = String::from_utf8_lossy(&cell[..end]);
            let dec = sealdb_types::MyDecimal::parse(&s).map_err(|e| {
                Error::new(ERR_UNKNOWN_ERROR, format!("decimal in tipb chunk: {e}"))
            })?;
            Ok(Datum::Decimal(dec))
        }
        _ => Ok(Datum::Bytes(cell.to_vec())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sealdb_types::TYPE_VAR_STRING;

    #[test]
    fn test_tipb_chunk_roundtrip_mixed() {
        let tps = [TYPE_LONG_LONG, TYPE_VAR_STRING, TYPE_DOUBLE];
        let rows = vec![
            vec![
                Datum::I64(1),
                Datum::Bytes(b"a".to_vec()),
                Datum::F64(1.5),
            ],
            vec![Datum::Null, Datum::Bytes(b"bb".to_vec()), Datum::Null],
            vec![
                Datum::I64(-3),
                Datum::Bytes(Vec::new()),
                Datum::F64(-2.0),
            ],
        ];
        let bytes = encode_chunk(&tps, &rows).unwrap();
        let decoded = decode_chunk(&tps, &bytes).unwrap();
        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded[0][0], Datum::I64(1));
        assert_eq!(decoded[0][1], Datum::Bytes(b"a".to_vec()));
        assert_eq!(decoded[1][0], Datum::Null);
        assert_eq!(decoded[1][1], Datum::Bytes(b"bb".to_vec()));
        assert_eq!(decoded[2][0], Datum::I64(-3));
        if let Datum::F64(v) = decoded[0][2] {
            assert!((v - 1.5).abs() < 1e-9);
        } else {
            panic!("{:?}", decoded[0][2]);
        }
    }
}
