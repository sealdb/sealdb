use crate::field_type::{EvalType, FieldType};
use crate::mydecimal::MyDecimal;
use crate::time::Time;
use sealdb_errno::{ERR_DATA_OUT_OF_RANGE, ERR_TRUNCATED_WRONG_VALUE, Error, Result};

#[derive(Clone, Debug, PartialEq)]
pub enum Datum {
    Null,
    I64(i64),
    U64(u64),
    F64(f64),
    Bytes(Vec<u8>),
    /// `BIT(n)` value: fixed-width big-endian bytes. Distinct from `Bytes` so
    /// numeric comparisons/functions treat it as an unsigned integer while
    /// `SELECT` prints the raw padded bytes (TiDB binary representation).
    Bit(Vec<u8>),
    Decimal(MyDecimal),
    Time(Time),
    /// `ENUM`/`SET` value: `(label, number)`. TiDB stores the member index
    /// (enum) / bitmask (set) and keeps the label for display: numeric
    /// comparisons use `number`, string comparisons use `label`.
    Enum(EnumValue),
}

#[derive(Clone, Debug, PartialEq)]
pub struct EnumValue {
    pub label: Vec<u8>,
    pub number: u64,
}

impl EnumValue {
    pub fn new(label: Vec<u8>, number: u64) -> Self {
        Self { label, number }
    }
}

/// MySQL's string-to-number conversion: the longest leading numeric prefix.
pub fn mysql_numeric_prefix(text: &str) -> f64 {
    let text = text.trim_start();
    let bytes = text.as_bytes();
    let mut end = 0;
    if end < bytes.len() && (bytes[end] == b'+' || bytes[end] == b'-') {
        end += 1;
    }
    let mut seen_digit = false;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
        seen_digit = true;
    }
    if end < bytes.len() && bytes[end] == b'.' {
        let mut probe = end + 1;
        while probe < bytes.len() && bytes[probe].is_ascii_digit() {
            probe += 1;
            seen_digit = true;
        }
        if seen_digit {
            end = probe;
        }
    }
    if seen_digit && end < bytes.len() && (bytes[end] == b'e' || bytes[end] == b'E') {
        let mut probe = end + 1;
        if probe < bytes.len() && (bytes[probe] == b'+' || bytes[probe] == b'-') {
            probe += 1;
        }
        let mut exponent = false;
        while probe < bytes.len() && bytes[probe].is_ascii_digit() {
            probe += 1;
            exponent = true;
        }
        if exponent {
            end = probe;
        }
    }
    if !seen_digit {
        return 0.0;
    }
    text[..end].parse().unwrap_or(0.0)
}

fn is_numeric_datum(datum: &Datum) -> bool {
    matches!(
        datum,
        Datum::I64(_) | Datum::U64(_) | Datum::F64(_) | Datum::Decimal(_)
    )
}

impl Datum {
    pub fn is_null(&self) -> bool {
        matches!(self, Datum::Null)
    }

    pub fn as_i64(&self) -> Result<i64> {
        match self {
            Datum::Null => Ok(0),
            Datum::I64(v) => Ok(*v),
            Datum::U64(v) if *v <= i64::MAX as u64 => Ok(*v as i64),
            Datum::U64(_) => Err(Error::new(
                ERR_DATA_OUT_OF_RANGE,
                "BIGINT UNSIGNED overflow",
            )),
            Datum::F64(v) => Ok(*v as i64),
            Datum::Bytes(b) => parse_i64(b),
            Datum::Bit(b) => {
                let v = bit_as_u64(b);
                if v <= i64::MAX as u64 {
                    Ok(v as i64)
                } else {
                    Err(Error::new(ERR_DATA_OUT_OF_RANGE, "BIGINT overflow"))
                }
            }
            Datum::Decimal(d) => d.to_i64(),
            // MySQL numeric value: `HHMMSS` for TIME, `YYYYMMDDHHMMSS` for
            // DATE/DATETIME (`types.Datum.GetInt64` on KindMysqlTime).
            Datum::Time(t) => Ok(t.to_number()),
            Datum::Enum(e) => Ok(e.number as i64),
        }
    }

    pub fn as_f64(&self) -> Result<f64> {
        match self {
            Datum::Null => Ok(0.0),
            Datum::I64(v) => Ok(*v as f64),
            Datum::U64(v) => Ok(*v as f64),
            Datum::F64(v) => Ok(*v),
            Datum::Bytes(b) => parse_f64(b),
            Datum::Bit(b) => Ok(bit_as_u64(b) as f64),
            Datum::Decimal(d) => d.to_f64(),
            Datum::Time(t) => Ok(t.to_number() as f64),
            Datum::Enum(e) => Ok(e.number as f64),
        }
    }

    pub fn as_bytes(&self) -> Vec<u8> {
        match self {
            Datum::Null => Vec::new(),
            Datum::I64(v) => v.to_string().into_bytes(),
            Datum::U64(v) => v.to_string().into_bytes(),
            Datum::F64(v) => format_float(*v).into_bytes(),
            Datum::Bytes(b) => b.clone(),
            Datum::Bit(b) => b.clone(),
            Datum::Decimal(d) => d.to_string().into_bytes(),
            Datum::Time(t) => t.to_string().into_bytes(),
            Datum::Enum(e) => e.label.clone(),
        }
    }

    pub fn compare(&self, other: &Datum) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        if self.is_null() && other.is_null() {
            return Ordering::Equal;
        }
        if self.is_null() {
            return Ordering::Less;
        }
        if other.is_null() {
            return Ordering::Greater;
        }
        match (self, other) {
            (Datum::I64(a), Datum::I64(b)) => a.cmp(b),
            (Datum::U64(a), Datum::U64(b)) => a.cmp(b),
            // MySQL compares a signed with an unsigned value as `uint64` (a
            // negative value is always smaller); comparing through `f64` would
            // collapse values above 2^53.
            (Datum::I64(a), Datum::U64(b)) => {
                if *a < 0 {
                    Ordering::Less
                } else {
                    (*a as u64).cmp(b)
                }
            }
            (Datum::U64(a), Datum::I64(b)) => {
                if *b < 0 {
                    Ordering::Greater
                } else {
                    a.cmp(&(*b as u64))
                }
            }
            (Datum::Bytes(a), Datum::Bytes(b)) => a.cmp(b),
            (Datum::Bit(a), Datum::Bit(b)) => a.cmp(b),
            // A binary literal compared with a string keeps its raw bytes
            // (`0x39 = '9'` is true, `x'4D7953514C' = 'MySQL'` is true).
            (Datum::Bit(a), Datum::Bytes(b)) => a.cmp(b),
            (Datum::Bytes(a), Datum::Bit(b)) => a.cmp(b),
            (Datum::Bit(a), other) | (other, Datum::Bit(a))
                if is_numeric_datum(other) || matches!(other, Datum::Bit(_)) =>
            {
                bit_as_u128(a).cmp(&numeric_as_u128(other))
            }
            (Datum::Decimal(a), Datum::Decimal(b)) => a.cmp(b),
            (Datum::Time(a), Datum::Time(b)) => a.cmp(b),
            // TiDB compares temporal values with numbers via their MySQL
            // numeric form (`20191111 = DATE '2019-11-11'`).
            (Datum::Time(a), Datum::I64(b)) => a.to_number().cmp(b),
            (Datum::I64(a), Datum::Time(b)) => a.cmp(&b.to_number()),
            (Datum::Time(a), Datum::U64(b)) => (a.to_number() as i128).cmp(&i128::from(*b)),
            (Datum::U64(a), Datum::Time(b)) => i128::from(*a).cmp(&(b.to_number() as i128)),
            (Datum::Time(a), Datum::F64(b)) => (a.to_number() as f64)
                .partial_cmp(b)
                .unwrap_or(Ordering::Equal),
            (Datum::F64(a), Datum::Time(b)) => a
                .partial_cmp(&(b.to_number() as f64))
                .unwrap_or(Ordering::Equal),
            // ENUM/SET: numbers compare by member index / bitmask, strings
            // (and other enum/set labels) compare by label.
            (Datum::Enum(a), Datum::Enum(b)) => a.label.cmp(&b.label),
            (Datum::Enum(a), Datum::Bytes(b)) => a.label.cmp(b),
            (Datum::Bytes(a), Datum::Enum(b)) => a.cmp(&b.label),
            (Datum::Enum(a), Datum::I64(b)) => i128::from(a.number).cmp(&i128::from(*b)),
            (Datum::I64(a), Datum::Enum(b)) => i128::from(*a).cmp(&i128::from(b.number)),
            (Datum::Enum(a), Datum::U64(b)) => u128::from(a.number).cmp(&u128::from(*b)),
            (Datum::U64(a), Datum::Enum(b)) => u128::from(*a).cmp(&u128::from(b.number)),
            (Datum::Enum(a), Datum::F64(b)) => {
                (a.number as f64).partial_cmp(b).unwrap_or(Ordering::Equal)
            }
            (Datum::F64(a), Datum::Enum(b)) => {
                a.partial_cmp(&(b.number as f64)).unwrap_or(Ordering::Equal)
            }
            (Datum::Enum(a), Datum::Decimal(b)) => (a.number as f64)
                .partial_cmp(&b.to_f64().unwrap_or(0.0))
                .unwrap_or(Ordering::Equal),
            (Datum::Decimal(a), Datum::Enum(b)) => a
                .to_f64()
                .unwrap_or(0.0)
                .partial_cmp(&(b.number as f64))
                .unwrap_or(Ordering::Equal),
            // MySQL compares a string with a number by the string's numeric
            // prefix (`'10ab' > 9`; `'aaa' > 0` is false).
            (Datum::Bytes(b), other) if is_numeric_datum(other) => {
                mysql_numeric_prefix(&String::from_utf8_lossy(b))
                    .partial_cmp(&other.as_f64().unwrap_or(0.0))
                    .unwrap_or(Ordering::Equal)
            }
            (other, Datum::Bytes(b)) if is_numeric_datum(other) => other
                .as_f64()
                .unwrap_or(0.0)
                .partial_cmp(&mysql_numeric_prefix(&String::from_utf8_lossy(b)))
                .unwrap_or(Ordering::Equal),
            _ => match (self.as_f64(), other.as_f64()) {
                (Ok(a), Ok(b)) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
                _ => self.as_bytes().cmp(&other.as_bytes()),
            },
        }
    }

    pub fn eval_type(&self) -> EvalType {
        match self {
            Datum::Null | Datum::I64(_) | Datum::U64(_) => EvalType::Int,
            Datum::F64(_) => EvalType::Real,
            Datum::Bytes(_) => EvalType::String,
            Datum::Bit(_) => EvalType::Int,
            Datum::Decimal(_) => EvalType::Decimal,
            Datum::Time(_) => EvalType::Datetime,
            Datum::Enum(_) => EvalType::String,
        }
    }

    pub fn cast(&self, ft: &FieldType) -> Result<Datum> {
        if self.is_null() {
            return Ok(Datum::Null);
        }
        if ft.tp == crate::field_type::TYPE_BIT {
            return Ok(Datum::Bit(bit_bytes_for(self, ft.flen)));
        }
        match ft.eval_type() {
            EvalType::Int => {
                if ft.is_unsigned() {
                    match self {
                        Datum::U64(v) => Ok(Datum::U64(*v)),
                        // MySQL/TiDB: CAST(negative AS UNSIGNED) wraps (e.g. -1 → u64::MAX).
                        Datum::I64(v) => Ok(Datum::U64(*v as u64)),
                        Datum::Decimal(d) => {
                            let s = d.to_string();
                            if s.starts_with('-') {
                                return Err(Error::new(ERR_DATA_OUT_OF_RANGE, "UNSIGNED overflow"));
                            }
                            let int_part = s.split('.').next().unwrap_or("0");
                            int_part
                                .parse::<u64>()
                                .map(Datum::U64)
                                .map_err(|_| Error::new(ERR_DATA_OUT_OF_RANGE, "UNSIGNED overflow"))
                        }
                        other => {
                            let v = other.as_i64()?;
                            if v < 0 {
                                return Err(Error::new(ERR_DATA_OUT_OF_RANGE, "UNSIGNED overflow"));
                            }
                            Ok(Datum::U64(v as u64))
                        }
                    }
                } else {
                    Ok(Datum::I64(self.as_i64()?))
                }
            }
            EvalType::Real => {
                let v = self.as_f64()?;
                // FLOAT is float32; DOUBLE stays float64.
                if ft.tp == crate::field_type::TYPE_FLOAT {
                    Ok(Datum::F64((v as f32) as f64))
                } else {
                    Ok(Datum::F64(v))
                }
            }
            EvalType::String => Ok(Datum::Bytes(self.as_bytes())),
            EvalType::Decimal => {
                let d = MyDecimal::parse(&self.display())?.with_frac(ft.decimal);
                Ok(Datum::Decimal(d))
            }
            EvalType::Datetime => match self {
                Datum::Time(t) => Ok(Datum::Time(*t)),
                Datum::Bytes(b) => {
                    let s = String::from_utf8_lossy(b);
                    let mut t = if ft.tp == crate::field_type::TYPE_DURATION {
                        crate::time::parse_duration_string(&s).ok_or_else(|| {
                            Error::new(
                                ERR_TRUNCATED_WRONG_VALUE,
                                format!("Incorrect time value: '{s}'"),
                            )
                        })?
                    } else {
                        crate::time::parse_timestamp_literal(&s).or_else(|_| {
                            if s.contains(' ') {
                                crate::time::parse_timestamp_literal(&s)
                            } else {
                                crate::time::parse_timestamp_literal(&format!("{s} 00:00:00"))
                            }
                        })?
                    };
                    if ft.tp == crate::field_type::TYPE_TIMESTAMP {
                        t.tp = crate::time::TimeType::Timestamp;
                    } else if ft.tp == crate::field_type::TYPE_DATE {
                        t.tp = crate::time::TimeType::Date;
                    } else if ft.tp == crate::field_type::TYPE_DURATION {
                        t.tp = crate::time::TimeType::Time;
                    } else {
                        t.tp = crate::time::TimeType::Datetime;
                    }
                    Ok(Datum::Time(t))
                }
                // TiDB `ParseTimeFromNum`: numeric DATE/DATETIME/TIME literals.
                other => {
                    let n = other.as_i64()?;
                    let tp = if ft.tp == crate::field_type::TYPE_DATE {
                        crate::time::TimeType::Date
                    } else if ft.tp == crate::field_type::TYPE_DURATION {
                        crate::time::TimeType::Time
                    } else if ft.tp == crate::field_type::TYPE_TIMESTAMP {
                        crate::time::TimeType::Timestamp
                    } else {
                        crate::time::TimeType::Datetime
                    };
                    let t = crate::time::time_from_number(n, tp).ok_or_else(|| {
                        Error::new(
                            ERR_TRUNCATED_WRONG_VALUE,
                            format!("Incorrect datetime value: '{}'", other.display()),
                        )
                    })?;
                    Ok(Datum::Time(t))
                }
            },
        }
    }

    pub fn display(&self) -> String {
        match self {
            Datum::Null => "NULL".to_string(),
            Datum::I64(v) => v.to_string(),
            Datum::U64(v) => v.to_string(),
            Datum::F64(v) => format_float(*v),
            Datum::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
            Datum::Bit(b) => String::from_utf8_lossy(b).into_owned(),
            Datum::Decimal(d) => d.to_string(),
            Datum::Time(t) => t.to_string(),
            Datum::Enum(e) => String::from_utf8_lossy(&e.label).into_owned(),
        }
    }

    pub fn typed_json(&self) -> serde_json::Value {
        match self {
            Datum::Null => serde_json::Value::Null,
            Datum::I64(v) => serde_json::json!({"type": "int", "value": v}),
            Datum::U64(v) => serde_json::json!({"type": "uint", "value": v}),
            Datum::F64(v) => serde_json::json!({"type": "float", "value": v}),
            Datum::Bytes(b) => {
                serde_json::json!({"type": "string", "value": String::from_utf8_lossy(b)})
            }
            Datum::Bit(b) => {
                serde_json::json!({"type": "bit", "value": bit_as_u64(b)})
            }
            Datum::Decimal(d) => serde_json::json!({"type": "decimal", "value": d.to_string()}),
            Datum::Time(t) => serde_json::json!({"type": "time", "value": t.to_string()}),
            Datum::Enum(e) => serde_json::json!({
                "type": "string",
                "value": String::from_utf8_lossy(&e.label)
            }),
        }
    }
}

fn parse_i64(b: &[u8]) -> Result<i64> {
    let s = std::str::from_utf8(b).unwrap_or("").trim();
    s.parse::<i64>()
        .map_err(|_| Error::new(ERR_TRUNCATED_WRONG_VALUE, format!("truncated integer {s}")))
}

fn parse_f64(b: &[u8]) -> Result<f64> {
    let s = std::str::from_utf8(b).unwrap_or("").trim();
    s.parse::<f64>()
        .map_err(|_| Error::new(ERR_TRUNCATED_WRONG_VALUE, format!("truncated float {s}")))
}

/// TiDB/MySQL REAL text form: scientific for large/small magnitudes (`1e23`),
/// otherwise decimal; whole values keep a single fractional digit (`2.0`).
fn bit_as_u64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b))
}

fn bit_as_u128(bytes: &[u8]) -> u128 {
    bytes
        .iter()
        .fold(0u128, |acc, b| (acc << 8) | u128::from(*b))
}

fn numeric_as_u128(datum: &Datum) -> u128 {
    match datum {
        Datum::U64(v) => u128::from(*v),
        Datum::I64(v) => *v as i128 as u128,
        Datum::F64(v) => *v as u128,
        Datum::Decimal(d) => d.to_f64().unwrap_or(0.0) as u128,
        Datum::Bit(b) => bit_as_u128(b),
        _ => 0,
    }
}

/// Big-endian bytes of `flen` bits, masked to the declared width.
fn bit_bytes_for(value: &Datum, flen: i32) -> Vec<u8> {
    let n = flen.max(1) as u32;
    let mask = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
    let bits = match value {
        Datum::U64(v) => *v,
        Datum::I64(v) => *v as u64,
        Datum::F64(v) => *v as u64,
        Datum::Decimal(d) => d.to_f64().unwrap_or(0.0) as u64,
        // A string constant read into a BIT column is a number (`'48'` → 48);
        // non-decimal text (and hex bytes that are not digits) keeps its
        // big-endian binary value.
        Datum::Bytes(b) => std::str::from_utf8(b)
            .ok()
            .map(str::trim)
            .and_then(|text| text.parse::<u64>().ok())
            .unwrap_or_else(|| bit_as_u64(b)),
        Datum::Bit(b) => bit_as_u64(b),
        Datum::Time(t) => t.to_packed_u64(),
        Datum::Enum(e) => e.number,
        Datum::Null => 0,
    };
    let width = ((n as usize).div_ceil(8)).clamp(1, 8);
    let be = (bits & mask).to_be_bytes();
    be[8 - width..].to_vec()
}

fn format_float(v: f64) -> String {
    if !v.is_finite() {
        return v.to_string();
    }
    // A FLOAT column stores float32: print its shortest float32 form
    // (`281.3699951171875` → `281.37`). Large values keep float64 digits so
    // genuine DOUBLE columns (e.g. `2^64`) are not truncated.
    let abs = v.abs();
    if abs != 0.0 && abs < 1e15 && (v as f32) as f64 == v {
        return (v as f32).to_string();
    }
    if abs != 0.0 && (abs >= 1e15 || abs < 1e-4) {
        let s = format!("{v:e}");
        // Rust may emit `1e+23`; TiDB goldens use `1e23`.
        return s.replace("e+", "e").replace("E+", "e").replace('E', "e");
    }
    // TiDB prints FLOAT/DOUBLE like Go `%v` (`1.0` → `1`); DECIMAL keeps its
    // scale through `Datum::Decimal` instead.
    v.to_string()
}
