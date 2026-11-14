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
    /// `YEAR` value: TiDB stores the year number but the text protocol prints
    /// it zero-padded to four digits (`0000`, `2001`). Distinct from `I64` so
    /// `SELECT` renders the padded form while numeric comparisons/arithmetic
    /// use the plain integer.
    Year(i64),
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

/// Go/TiDB's shortest round-trip rendering of a float64 (`strconv.FormatFloat(v,
/// 'g', -1, 64)` as used by `%v`). Rust's `to_string` also rounds to the
/// shortest form but breaks exact ties away from zero where Ryu/Go breaks them
/// to even, so long float64 expansions can differ in the last digit
/// (`-111.11111450195312` vs `...13`).
pub fn shortest_f64(v: f64) -> String {
    if !v.is_finite() {
        return v.to_string();
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0".to_string() } else { "0".to_string() };
    }
    for prec in 14..=16usize {
        if let Some(text) = scientific_to_plain(prec, v) {
            if text.parse::<f64>().map(|parsed| parsed == v).unwrap_or(false) {
                return text;
            }
        }
    }
    v.to_string()
}

/// Formats `v` with `prec + 1` significant digits (correctly rounded, ties to
/// even, which `core::fmt` guarantees) and expands the scientific notation.
fn scientific_to_plain(prec: usize, v: f64) -> Option<String> {
    let formatted = format!("{:.*e}", prec, v);
    let (mantissa, exponent) = formatted.split_once('e')?;
    let exponent: i32 = exponent.parse().ok()?;
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    let point = 1 + exponent;
    if point <= 0 {
        out.push_str("0.");
        for _ in 0..(-point) {
            out.push('0');
        }
        out.push_str(&digits);
    } else if (point as usize) >= digits.len() {
        out.push_str(&digits);
        for _ in 0..(point as usize - digits.len()) {
            out.push('0');
        }
    } else {
        out.push_str(&digits[..point as usize]);
        out.push('.');
        out.push_str(&digits[point as usize..]);
    }
    if out.contains('.') {
        while out.ends_with('0') {
            out.pop();
        }
        if out.ends_with('.') {
            out.pop();
        }
    }
    Some(out)
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
        Datum::I64(_) | Datum::U64(_) | Datum::F64(_) | Datum::Year(_) | Datum::Decimal(_)
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
            Datum::Year(v) => Ok(*v),
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
            Datum::Year(v) => Ok(*v as f64),
            Datum::U64(v) => Ok(*v as f64),
            Datum::F64(v) => Ok(*v),
            Datum::Bytes(b) => parse_f64(b),
            Datum::Bit(b) => Ok(bit_as_u64(b) as f64),
            Datum::Decimal(d) => d.to_f64(),
            // TiDB `Time.ToNumber()` keeps the fractional seconds, so a
            // temporal → DOUBLE conversion keeps them too (`19:38:25.001` →
            // `193825.001`).
            Datum::Time(t) => time_numeric_text(t)
                .parse::<f64>()
                .map_err(|e| Error::new(ERR_TRUNCATED_WRONG_VALUE, e.to_string())),
            Datum::Enum(e) => Ok(e.number as f64),
        }
    }

    pub fn as_bytes(&self) -> Vec<u8> {
        match self {
            Datum::Null => Vec::new(),
            Datum::I64(v) => v.to_string().into_bytes(),
            Datum::Year(v) => v.to_string().into_bytes(),
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
        // YEAR compares as its integer value (`y = 2001`, `y + 1`); the
        // four-digit padding is display-only.
        if matches!(self, Datum::Year(_)) || matches!(other, Datum::Year(_)) {
            let a = match self {
                Datum::Year(v) => Datum::I64(*v),
                other => other.clone(),
            };
            let b = match other {
                Datum::Year(v) => Datum::I64(*v),
                other => other.clone(),
            };
            return a.compare(&b);
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
            Datum::Null | Datum::I64(_) | Datum::U64(_) | Datum::Year(_) => EvalType::Int,
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
        // YEAR keeps MySQL's two-digit window (1-69 → 20xx, 70-99 → 19xx).
        if ft.tp == crate::field_type::TYPE_YEAR {
            let n = match self {
                Datum::Time(t) => i128::from(t.year),
                Datum::F64(v) => i128::from(v.round() as i64),
                Datum::Decimal(d) => i128::from(d.to_f64().unwrap_or(0.0).round() as i64),
                other => i128::from(other.as_i64()?),
            };
            let year = match n {
                0 => 0,
                1..=69 => 2000 + n,
                70..=99 => 1900 + n,
                1901..=2155 => n,
                _ => {
                    return Err(Error::new(
                        ERR_DATA_OUT_OF_RANGE,
                        format!("Invalid YEAR value: '{n}'"),
                    ));
                }
            };
            return Ok(Datum::Year(year as i64));
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
            EvalType::String => {
                // A BIT value renders as its unsigned integer (`b'10101'` → '21').
                let mut bytes = match self {
                    Datum::Bit(b) => bit_as_u64(b).to_string().into_bytes(),
                    other => other.as_bytes(),
                };
                // `BINARY(N)` (`TypeString` + binary charset) is fixed-length:
                // MySQL/TiDB zero-pad on write and truncate past N bytes, so
                // `insert 'a'` stores `'a\0'` and `a = 'a'` no longer matches.
                if ft.tp == crate::field_type::TYPE_STRING && ft.charset == "binary" && ft.flen > 0
                {
                    let len = ft.flen as usize;
                    if bytes.len() != len {
                        bytes.resize(len, 0);
                    }
                }
                // TiDB parses a JSON document into its binary form, where a
                // decimal number is a float64: the text is renormalized to the
                // shortest f64 form when the double cannot round-trip it.
                if ft.tp == crate::field_type::TYPE_JSON {
                    bytes = normalize_json_numbers(&bytes);
                }
                Ok(Datum::Bytes(bytes))
            }
            EvalType::Decimal => {
                // A BIT value converts through its unsigned integer, not the
                // raw big-endian bytes its `display` would expose; a temporal
                // value uses its numeric form (`2020-10-30 08:21:33.455555` →
                // `20201030082133.455555`), like TiDB's `TimeToDecimal`.
                let text = match self {
                    Datum::Bit(bytes) => {
                        bytes.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b)).to_string()
                    }
                    Datum::Time(t) => time_numeric_text(t),
                    _ => self.display(),
                };
                let d = MyDecimal::parse(&text)?.with_frac(ft.decimal);
                Ok(Datum::Decimal(d))
            }
            EvalType::Datetime => match self {
                Datum::Time(t) => {
                    let mut t = crate::time::round_to_fsp(*t, ft.decimal.clamp(0, 6) as i8);
                    match ft.tp {
                        // CAST(x AS DATE) drops the time part.
                        crate::field_type::TYPE_DATE => {
                            t.hour = 0;
                            t.minute = 0;
                            t.second = 0;
                            t.micro = 0;
                            t.fsp = 0;
                            t.tp = crate::time::TimeType::Date;
                        }
                        // CAST(date AS DATETIME) is the midnight datetime.
                        crate::field_type::TYPE_DATETIME => {
                            t.tp = crate::time::TimeType::Datetime;
                        }
                        crate::field_type::TYPE_TIMESTAMP => {
                            t.tp = crate::time::TimeType::Timestamp;
                        }
                        crate::field_type::TYPE_DURATION => {
                            t.tp = crate::time::TimeType::Time;
                        }
                        _ => {}
                    }
                    Ok(Datum::Time(t))
                }
                Datum::Bytes(b) => {
                    let s = String::from_utf8_lossy(b);
                    let kind = temporal_kind(ft);
                    // TiDB routes an all-digit string through `ParseTimeFromNum`
                    // (`'2008261'` is the YYMMDD number 2020-08-26); a float
                    // form keeps only its integer part (`ParseTimeFromFloatString`,
                    // so `20200826173501.125` is 17:35:01). A TIME target keeps
                    // only the clock of the parsed datetime.
                    let whole = numeric_datetime_whole(s.trim());
                    let numeric = whole.and_then(|text| {
                        let n = text.parse::<i64>().ok()?;
                        if ft.tp == crate::field_type::TYPE_DURATION {
                            return crate::time::duration_from_numeric(text);
                        }
                        let tp = if ft.tp == crate::field_type::TYPE_DATE {
                            crate::time::TimeType::Date
                        } else if ft.tp == crate::field_type::TYPE_TIMESTAMP {
                            crate::time::TimeType::Timestamp
                        } else {
                            crate::time::TimeType::Datetime
                        };
                        crate::time::time_from_number(n, tp)
                    });
                    let mut t = if let Some(t) = numeric {
                        t
                    } else if ft.tp == crate::field_type::TYPE_DURATION {
                        crate::time::parse_duration_string(&s).ok_or_else(|| {
                            Error::new(
                                ERR_TRUNCATED_WRONG_VALUE,
                                format!("Incorrect time value: '{s}'"),
                            )
                        })?
                    } else {
                        crate::time::parse_timestamp_literal(&s)
                            .or_else(|_| {
                                if s.contains(' ') {
                                    crate::time::parse_timestamp_literal(&s)
                                } else {
                                    crate::time::parse_timestamp_literal(&format!("{s} 00:00:00"))
                                }
                            })
                            .map_err(|_| {
                                Error::new(
                                    ERR_TRUNCATED_WRONG_VALUE,
                                    format!("Incorrect {kind} value: '{s}'"),
                                )
                            })?
                    };
                    if ft.tp == crate::field_type::TYPE_TIMESTAMP {
                        t.tp = crate::time::TimeType::Timestamp;
                    } else if ft.tp == crate::field_type::TYPE_DATE {
                        // CAST(… AS DATE) drops the time part.
                        t.hour = 0;
                        t.minute = 0;
                        t.second = 0;
                        t.micro = 0;
                        t.fsp = 0;
                        t.tp = crate::time::TimeType::Date;
                    } else if ft.tp == crate::field_type::TYPE_DURATION {
                        t.tp = crate::time::TimeType::Time;
                    } else {
                        t.tp = crate::time::TimeType::Datetime;
                    }
                    Ok(Datum::Time(crate::time::round_to_fsp(
                        t,
                        ft.decimal.clamp(0, 6) as i8,
                    )))
                }
                // TiDB `ParseTimeFromNum`: numeric DATE/DATETIME/TIME literals.
                other => {
                    // TIME keeps the clock part of the number and has no form
                    // for a bare date (`20200805` is an "Incorrect time value").
                    if ft.tp == crate::field_type::TYPE_DURATION {
                        let text = other.display();
                        // Only the integral part feeds the clock digits; a
                        // FLOAT's shortest form can be scientific notation.
                        let integral = match other {
                            Datum::F64(v) if v.is_finite() => {
                                // A FLOAT column keeps its shortest float32 form
                                // (`200805100000`, not the exact binary value).
                                if *v != 0.0 && v.abs() < 1e15 && (*v as f32) as f64 == *v {
                                    (*v as f32).to_string()
                                } else {
                                    format!("{:.0}", v.trunc())
                                }
                            }
                            Datum::Decimal(d) => {
                                d.to_string().split('.').next().unwrap_or("0").to_string()
                            }
                            _ => text.split('.').next().unwrap_or("0").to_string(),
                        };
                        let t = crate::time::duration_from_numeric(&integral).ok_or_else(|| {
                            // TiDB reads a FLOAT through `ParseDuration`
                            // (`Incorrect time value`); an integer goes through
                            // the numeric parser (`Truncated incorrect time value`).
                            let message = if matches!(other, Datum::F64(_)) {
                                format!("Incorrect time value: '{text}'")
                            } else {
                                format!("Truncated incorrect time value: '{text}'")
                            };
                            Error::new(ERR_TRUNCATED_WRONG_VALUE, message)
                        })?;
                        return Ok(Datum::Time(t));
                    }
                    // UNSIGNED values beyond `i64` still report the invalid
                    // datetime (1292), not an integer overflow.
                    let raw: i128 = match other {
                        Datum::U64(v) => *v as i128,
                        _ => other.as_i64()? as i128,
                    };
                    let n = i64::try_from(raw).map_err(|_| {
                        Error::new(
                            ERR_TRUNCATED_WRONG_VALUE,
                            format!(
                                "Incorrect {} value: '{}'",
                                temporal_kind(ft),
                                other.display()
                            ),
                        )
                    })?;
                    let tp = if ft.tp == crate::field_type::TYPE_DATE {
                        crate::time::TimeType::Date
                    } else if ft.tp == crate::field_type::TYPE_DURATION {
                        crate::time::TimeType::Time
                    } else if ft.tp == crate::field_type::TYPE_TIMESTAMP {
                        crate::time::TimeType::Timestamp
                    } else {
                        crate::time::TimeType::Datetime
                    };
                    let mut t = crate::time::time_from_number(n, tp).ok_or_else(|| {
                        Error::new(
                            ERR_TRUNCATED_WRONG_VALUE,
                            format!(
                                "Incorrect {} value: '{}'",
                                temporal_kind(ft),
                                other.display()
                            ),
                        )
                    })?;
                    // Keep the fractional seconds of a DECIMAL/DOUBLE source
                    // (`20201030082133.455555` → `.455555`).
                    if ft.tp != crate::field_type::TYPE_DATE
                        && let Some((_, frac)) = other.display().split_once('.')
                    {
                        let mut digits: String =
                            frac.chars().filter(|c| c.is_ascii_digit()).take(6).collect();
                        let fsp = digits.len() as i8;
                        while digits.len() < 6 {
                            digits.push('0');
                        }
                        t.micro = digits.parse().unwrap_or(0);
                        t.fsp = fsp;
                    }
                    Ok(Datum::Time(crate::time::round_to_fsp(
                        t,
                        ft.decimal.clamp(0, 6) as i8,
                    )))
                }
            },
        }
    }

    pub fn display(&self) -> String {
        match self {
            Datum::Null => "NULL".to_string(),
            Datum::I64(v) => v.to_string(),
            // TiDB `DumpTextRow`: only the zero year prints `0000`; every
            // other year keeps its plain integer spelling (`2001`).
            Datum::Year(0) => "0000".to_string(),
            Datum::Year(v) => v.to_string(),
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
            Datum::Year(v) => serde_json::json!({"type": "int", "value": v}),
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

/// The target type name TiDB uses in `Incorrect <kind> value` (1292).
fn temporal_kind(ft: &FieldType) -> &'static str {
    if ft.tp == crate::field_type::TYPE_DATE {
        "date"
    } else if ft.tp == crate::field_type::TYPE_DURATION {
        "time"
    } else if ft.tp == crate::field_type::TYPE_TIMESTAMP {
        "timestamp"
    } else {
        "datetime"
    }
}

/// TiDB `ParseTimeFromFloatString`: the integer part of a numeric datetime text
/// (`20200826173501.125` → `20200826173501`); `None` when the text is not an
/// all-digit number with an optional fractional part.
fn numeric_datetime_whole(text: &str) -> Option<&str> {
    let (whole, frac) = match text.split_once('.') {
        Some((whole, frac)) => (whole, Some(frac)),
        None => (text, None),
    };
    if whole.is_empty() || !whole.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if let Some(frac) = frac
        && !frac.bytes().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    Some(whole)
}

fn parse_f64(b: &[u8]) -> Result<f64> {
    let s = std::str::from_utf8(b).unwrap_or("").trim();
    s.parse::<f64>()
        .map_err(|_| Error::new(ERR_TRUNCATED_WRONG_VALUE, format!("truncated float {s}")))
}

/// TiDB `TimeToDecimal` text form: the packed `YYYYMMDDHHMMSS` number with the
/// stored fractional seconds appended (`2020-10-30 08:21:33.455555` →
/// `20201030082133.455555`).
fn time_numeric_text(t: &crate::time::Time) -> String {
    let base = t.to_number();
    let fsp = t.fsp.clamp(0, 6) as usize;
    if fsp == 0 || t.micro == 0 {
        return base.to_string();
    }
    let frac = format!("{:06}", t.micro);
    format!("{base}.{}", &frac[..fsp])
}

/// Rewrites the decimal numbers of a JSON document to the double form TiDB
/// stores (`types.ParseJSON` → `CreateBinary(float64)`), leaving integers and
/// the numbers that already round-trip unchanged.
fn normalize_json_numbers(bytes: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return bytes.to_vec();
    };
    if serde_json::from_str::<serde_json::Value>(text).is_err() {
        return bytes.to_vec();
    }
    let raw = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(raw.len());
    let mut i = 0;
    let mut in_string = false;
    let mut escaped = false;
    while i < raw.len() {
        let b = raw[i];
        if in_string {
            out.push(b);
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if b == b'"' {
            in_string = true;
            out.push(b);
            i += 1;
            continue;
        }
        if b.is_ascii_digit() || b == b'-' {
            let start = i;
            let mut end = i + 1;
            while end < raw.len()
                && (raw[end].is_ascii_digit()
                    || matches!(raw[end], b'.' | b'e' | b'E' | b'+' | b'-'))
            {
                end += 1;
            }
            out.extend_from_slice(json_number_text(&text[start..end]).as_bytes());
            i = end;
            continue;
        }
        out.push(b);
        i += 1;
    }
    out
}

/// The stored text of one JSON number token.
fn json_number_text(token: &str) -> String {
    let plain = !token.contains(['e', 'E']);
    // An integer keeps its literal form (int64/uint64 JSON kind).
    if plain && !token.contains('.') {
        return token.to_string();
    }
    let Ok(v) = token.parse::<f64>() else {
        return token.to_string();
    };
    if !v.is_finite() {
        return token.to_string();
    }
    let shortest = shortest_f64(v);
    // Compare without the redundant zeros (`1.500` keeps its literal text).
    let mut normalized = token.trim_start_matches('+').to_string();
    if let Some((int_part, frac)) = normalized.split_once('.') {
        let frac = frac.trim_end_matches('0');
        normalized = if frac.is_empty() {
            int_part.to_string()
        } else {
            format!("{int_part}.{frac}")
        };
    }
    if normalized == shortest {
        token.to_string()
    } else {
        shortest
    }
}

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
        Datum::Year(v) => *v as u64,
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


