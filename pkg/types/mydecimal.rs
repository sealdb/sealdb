use sealdb_errno::{ERR_TRUNCATED_WRONG_VALUE, Error, Result};
use std::cmp::Ordering;
use std::fmt::{Display, Formatter};

/// `10^0 ..= 10^9` — one decimal group's worth of values.
const POWERS10: [u32; 10] = [
    1,
    10,
    100,
    1_000,
    10_000,
    100_000,
    1_000_000,
    10_000_000,
    100_000_000,
    1_000_000_000,
];

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
        // TiDB drops leading integer zeros when it builds a decimal literal
        // (`000003.000000` prints `3.000000`); the fractional scale is kept.
        let (int_part, frac_part) = match rest.split_once('.') {
            Some((int_part, frac)) => (int_part, Some(frac)),
            None => (rest, None),
        };
        let int_part = int_part.trim_start_matches('0');
        let int_part = if int_part.is_empty() { "0" } else { int_part };
        let is_zero =
            int_part == "0" && frac_part.is_none_or(|frac| frac.bytes().all(|b| b == b'0'));
        let digits = match frac_part {
            Some(frac) => format!("{int_part}.{frac}"),
            None => int_part.to_string(),
        };
        Ok(Self {
            negative: negative && !is_zero,
            digits,
        })
    }

    /// Unary minus on the decimal value (zero stays unsigned zero).
    pub fn negated(&self) -> Self {
        let mut out = self.clone();
        if out.digits != "0" && out.digits != "0.0" {
            out.negative = !out.negative;
        }
        out
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
            format!(
                "{int_part}.{frac}{:0>width$}",
                "",
                width = scale - frac.len()
            )
        };
        Self {
            negative: self.negative,
            digits,
        }
    }

    /// Memcomparable integer encoding of the decimal's integer value.
    pub fn encode_int_cmp(&self) -> Result<i64> {
        self.to_i64()
    }

    /// MySQL `MyDecimal::toBin` body — the payload written after the
    /// `decimalFlag` / precision / scale header. Digits are grouped base
    /// `10^9`, the leading and trailing groups use the minimal number of
    /// bytes, negative values are stored as the ones' complement, and the
    /// first byte has its high bit flipped so memcomparable order matches
    /// numeric order (TiDB `WriteBin`, `mydecimal.go`).
    pub fn encode_bin(&self, precision: usize, scale: usize) -> Result<Vec<u8>> {
        if precision > 65 || scale > 30 || scale > precision {
            return Err(Error::new(ERR_TRUNCATED_WRONG_VALUE, "bad decimal spec"));
        }
        let intg = precision - scale;
        let words_int = intg / 9;
        let dig_int = intg - words_int * 9;
        let words_frac = scale / 9;
        let dig_frac = scale - words_frac * 9;

        let bad = |m: &str| Error::new(ERR_TRUNCATED_WRONG_VALUE, m.to_string());
        let (mut i_str, mut f_str) = match self.digits.split_once('.') {
            Some((i, f)) => (i.to_string(), f.to_string()),
            None => (self.digits.clone(), String::new()),
        };
        i_str = i_str.trim_start_matches('0').to_string();
        if i_str.len() > intg {
            return Err(bad("decimal out of range"));
        }
        // Pad the integer part on the left and the fraction on the right so
        // every group can be read by a fixed offset.
        i_str = format!("{:0>width$}", i_str, width = intg);
        f_str.truncate(scale);
        // TiDB `decimal2bin` zero-pads the fraction, so an integer-valued
        // decimal must yield all-`0` digits, not spaces.
        f_str = format!("{:0<width$}", f_str, width = scale);

        let group = |s: &str| -> Result<u32> {
            s.parse::<u32>()
                .map_err(|_| bad("decimal digit group overflow"))
        };
        let mut words: Vec<u32> = Vec::with_capacity(words_int + words_frac + 2);
        let mut off = 0;
        if dig_int > 0 {
            words.push(group(&i_str[off..off + dig_int])?);
            off += dig_int;
        }
        for _ in 0..words_int {
            words.push(group(&i_str[off..off + 9])?);
            off += 9;
        }
        off = 0;
        for _ in 0..words_frac {
            words.push(group(&f_str[off..off + 9])?);
            off += 9;
        }
        if dig_frac > 0 {
            words.push(group(&f_str[off..off + dig_frac])?);
        }

        // `dig2bytes[n]`: bytes needed to hold an `n`-digit group.
        const DIG2BYTES: [usize; 10] = [0, 1, 1, 2, 2, 3, 3, 4, 4, 4];
        let mut out = Vec::with_capacity(3 + words.len() * 4);
        let mut w = 0usize;
        let push_group = |out: &mut Vec<u8>, value: u32, nbytes: usize| {
            let x = if self.negative { !value } else { value };
            for i in (0..nbytes).rev() {
                out.push(((x >> (8 * i)) & 0xff) as u8);
            }
        };
        if dig_int > 0 {
            let v = words[w] % POWERS10[dig_int];
            push_group(&mut out, v, DIG2BYTES[dig_int]);
            w += 1;
        }
        for _ in 0..words_int {
            push_group(&mut out, words[w], 4);
            w += 1;
        }
        for _ in 0..words_frac {
            push_group(&mut out, words[w], 4);
            w += 1;
        }
        if dig_frac > 0 {
            // The trailing group was parsed from exactly `dig_frac` digits, so
            // it already holds the value TiDB extracts with `word / powers10`.
            push_group(&mut out, words[w], DIG2BYTES[dig_frac]);
        }
        if let Some(first) = out.first_mut() {
            *first ^= 0x80;
        }
        Ok(out)
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
