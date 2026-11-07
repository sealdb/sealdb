use sealdb_errno::{ERR_TRUNCATED_WRONG_VALUE, Error, Result};
use std::cmp::Ordering;
use std::fmt::{Display, Formatter};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MyDecimal {
    negative: bool,
    digits: String,
}

impl MyDecimal {
    pub fn from_i64(v: i64) -> Self {
        Self {
            negative: v < 0,
            digits: v.unsigned_abs().to_string(),
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.is_empty() {
            return Err(Error::new(ERR_TRUNCATED_WRONG_VALUE, "empty decimal"));
        }
        let (negative, rest) = if let Some(rest) = s.strip_prefix('-') {
            (true, rest)
        } else {
            (false, s.strip_prefix('+').unwrap_or(s))
        };
        if !rest.chars().all(|c| c.is_ascii_digit() || c == '.')
            || rest.chars().filter(|c| *c == '.').count() > 1
            || rest.is_empty()
        {
            return Err(Error::new(
                ERR_TRUNCATED_WRONG_VALUE,
                format!("invalid decimal {s}"),
            ));
        }
        Ok(Self {
            negative: negative && rest != "0" && rest != "0.0",
            digits: rest.to_string(),
        })
    }

    pub fn to_i64(&self) -> Result<i64> {
        let int_part = self.digits.split('.').next().unwrap_or("0");
        let v: i64 = int_part
            .parse()
            .map_err(|_| Error::new(ERR_TRUNCATED_WRONG_VALUE, "decimal overflow"))?;
        Ok(if self.negative { -v } else { v })
    }

    pub fn to_f64(&self) -> Result<f64> {
        let v: f64 = self
            .digits
            .parse()
            .map_err(|_| Error::new(ERR_TRUNCATED_WRONG_VALUE, "invalid decimal"))?;
        Ok(if self.negative { -v } else { v })
    }

    /// Round/pad fractional digits to `scale` (TiDB `MyDecimal.Round`).
    pub fn with_frac(&self, scale: i32) -> Self {
        if scale < 0 {
            return self.clone();
        }
        let scale = scale as usize;
        let (int_part, frac) = match self.digits.split_once('.') {
            Some((i, f)) => (i.to_string(), f.to_string()),
            None => (self.digits.clone(), String::new()),
        };
        let digits = if scale == 0 {
            int_part
        } else if frac.len() >= scale {
            format!("{int_part}.{}", &frac[..scale])
        } else {
            format!("{int_part}.{frac}{:0>width$}", "", width = scale - frac.len())
        };
        Self {
            negative: self.negative,
            digits,
        }
    }

    /// Memcomparable integer encoding of the decimal's integer value.
    /// Full MyDecimal bin format is completed in the completeness wave.
    pub fn encode_int_cmp(&self) -> Result<i64> {
        self.to_i64()
    }

    /// Number of fractional digits in the literal (`1.500` → 3).
    pub fn frac_digits(&self) -> i32 {
        self.digits
            .split_once('.')
            .map(|(_, frac)| frac.len() as i32)
            .unwrap_or(0)
    }
}

impl Display for MyDecimal {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if self.negative {
            write!(f, "-{}", self.digits)
        } else {
            write!(f, "{}", self.digits)
        }
    }
}

impl PartialOrd for MyDecimal {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MyDecimal {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.to_f64(), other.to_f64()) {
            (Ok(a), Ok(b)) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
            _ => self.to_string().cmp(&other.to_string()),
        }
    }
}
