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
    Decimal(MyDecimal),
    Time(Time),
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
            Datum::Decimal(d) => d.to_i64(),
            Datum::Time(t) => Ok(t.to_packed_u64() as i64),
        }
    }

    pub fn as_f64(&self) -> Result<f64> {
        match self {
            Datum::Null => Ok(0.0),
            Datum::I64(v) => Ok(*v as f64),
            Datum::U64(v) => Ok(*v as f64),
            Datum::F64(v) => Ok(*v),
            Datum::Bytes(b) => parse_f64(b),
            Datum::Decimal(d) => d.to_f64(),
            Datum::Time(t) => Ok(t.to_packed_u64() as f64),
        }
    }

    pub fn as_bytes(&self) -> Vec<u8> {
        match self {
            Datum::Null => Vec::new(),
            Datum::I64(v) => v.to_string().into_bytes(),
            Datum::U64(v) => v.to_string().into_bytes(),
            Datum::F64(v) => format_float(*v).into_bytes(),
            Datum::Bytes(b) => b.clone(),
            Datum::Decimal(d) => d.to_string().into_bytes(),
            Datum::Time(t) => t.to_string().into_bytes(),
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
            (Datum::Bytes(a), Datum::Bytes(b)) => a.cmp(b),
            (Datum::Decimal(a), Datum::Decimal(b)) => a.cmp(b),
            (Datum::Time(a), Datum::Time(b)) => a.cmp(b),
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
            Datum::Decimal(_) => EvalType::Decimal,
            Datum::Time(_) => EvalType::Datetime,
        }
    }

    pub fn cast(&self, ft: &FieldType) -> Result<Datum> {
        if self.is_null() {
            return Ok(Datum::Null);
        }
        match ft.eval_type() {
            EvalType::Int => {
                if ft.is_unsigned() {
                    match self {
                        Datum::U64(v) => Ok(Datum::U64(*v)),
                        Datum::I64(v) if *v >= 0 => Ok(Datum::U64(*v as u64)),
                        Datum::I64(_) => {
                            Err(Error::new(ERR_DATA_OUT_OF_RANGE, "UNSIGNED overflow"))
                        }
                        Datum::Decimal(d) => {
                            let s = d.to_string();
                            if s.starts_with('-') {
                                return Err(Error::new(
                                    ERR_DATA_OUT_OF_RANGE,
                                    "UNSIGNED overflow",
                                ));
                            }
                            let int_part = s.split('.').next().unwrap_or("0");
                            int_part
                                .parse::<u64>()
                                .map(Datum::U64)
                                .map_err(|_| {
                                    Error::new(ERR_DATA_OUT_OF_RANGE, "UNSIGNED overflow")
                                })
                        }
                        other => {
                            let v = other.as_i64()?;
                            if v < 0 {
                                return Err(Error::new(
                                    ERR_DATA_OUT_OF_RANGE,
                                    "UNSIGNED overflow",
                                ));
                            }
                            Ok(Datum::U64(v as u64))
                        }
                    }
                } else {
                    Ok(Datum::I64(self.as_i64()?))
                }
            }
            EvalType::Real => Ok(Datum::F64(self.as_f64()?)),
            EvalType::String => Ok(Datum::Bytes(self.as_bytes())),
            EvalType::Decimal => {
                let d = MyDecimal::parse(&self.display())?.with_frac(ft.decimal);
                Ok(Datum::Decimal(d))
            }
            EvalType::Datetime => Ok(self.clone()),
        }
    }

    pub fn display(&self) -> String {
        match self {
            Datum::Null => "NULL".to_string(),
            Datum::I64(v) => v.to_string(),
            Datum::U64(v) => v.to_string(),
            Datum::F64(v) => format_float(*v),
            Datum::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
            Datum::Decimal(d) => d.to_string(),
            Datum::Time(t) => t.to_string(),
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
            Datum::Decimal(d) => serde_json::json!({"type": "decimal", "value": d.to_string()}),
            Datum::Time(t) => serde_json::json!({"type": "time", "value": t.to_string()}),
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
fn format_float(v: f64) -> String {
    if !v.is_finite() {
        return v.to_string();
    }
    let abs = v.abs();
    if abs != 0.0 && (abs >= 1e15 || abs < 1e-4) {
        let s = format!("{v:e}");
        // Rust may emit `1e+23`; TiDB goldens use `1e23`.
        return s.replace("e+", "e").replace("E+", "e").replace('E', "e");
    }
    if v.fract() == 0.0 {
        format!("{v:.1}")
    } else {
        v.to_string()
    }
}
