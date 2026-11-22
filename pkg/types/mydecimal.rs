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
        let (int_part, mut frac) = match self.digits.split_once('.') {
            Some((i, f)) => (i.to_string(), f.to_string()),
            None => (self.digits.clone(), String::new()),
        };
        if frac.len() < scale {
            frac.push_str(&"0".repeat(scale - frac.len()));
        }
        // Reducing the scale rounds (MySQL `MyDecimal.Round`, half away from
        // zero): `cast(999.99 as decimal(5,1))` is `1000.0`.
        let mut round_up = frac.as_bytes().get(scale).is_some_and(|b| *b >= b'5');
        let mut whole: Vec<u8> = int_part.bytes().chain(frac.bytes().take(scale)).collect();
        if round_up {
            let mut index = whole.len();
            while index > 0 {
                index -= 1;
                if whole[index] == b'9' {
                    whole[index] = b'0';
                } else {
                    whole[index] += 1;
                    round_up = false;
                    break;
                }
            }
            if round_up {
                whole.insert(0, b'1');
            }
        }
        let int_len = whole.len().saturating_sub(scale);
        let mut int_out = String::from_utf8_lossy(&whole[..int_len]).into_owned();
        while int_out.len() > 1 && int_out.starts_with('0') {
            int_out.remove(0);
        }
        if int_out.is_empty() {
            int_out.push('0');
        }
        let digits = if scale == 0 {
            int_out
        } else if int_len == whole.len() {
            // No fractional digits survived (e.g. the value was an integer).
            format!("{int_out}.{}", "0".repeat(scale))
        } else {
            format!("{int_out}.{}", String::from_utf8_lossy(&whole[int_len..]))
        };
        Self {
            negative: self.negative,
            digits,
        }
    }

    /// Saturate to `DECIMAL(precision, scale)`: an integer part wider than
    /// `precision - scale` becomes all nines (TiDB `MyDecimal.Round` overflow
    /// path), preserving the sign and the fractional scale.
    pub fn clamped(&self, precision: i32, scale: i32) -> Self {
        if precision <= 0 || scale < 0 {
            return self.clone();
        }
        let int_limit = (precision - scale).max(0) as usize;
        let int_digits = self
            .digits
            .split('.')
            .next()
            .unwrap_or("")
            .trim_start_matches('0')
            .len();
        if int_digits <= int_limit {
            return self.clone();
        }
        let mut text = String::new();
        if self.negative {
            text.push('-');
        }
        text.push_str(&"9".repeat(int_limit));
        if scale > 0 {
            text.push('.');
            text.push_str(&"9".repeat(scale as usize));
        }
        Self::parse(&text).unwrap_or_else(|_| Self::from_i64(0))
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

    /// Exact decimal `+` / `-` (MySQL `DecimalAdd` / `DecimalSub`): the result
    /// keeps the wider operand scale, so `1.10 + 2` is `3.10`.
    pub fn checked_add(&self, other: &Self, subtract: bool) -> Self {
        let (int1, frac1) = self.int_frac();
        let (int2, frac2) = other.int_frac();
        let scale = frac1.len().max(frac2.len());
        let a = format!("{int1}{frac1:0<scale$}", scale = scale);
        let b = format!("{int2}{frac2:0<scale$}", scale = scale);
        let b_negative = other.negative != subtract;
        let cmp = ucmp(&a, &b);
        let (negative, magnitude) = match (self.negative, b_negative) {
            (false, false) => (false, uadd(&a, &b)),
            (true, true) => (true, uadd(&a, &b)),
            (false, true) => match cmp {
                Ordering::Less => (true, usub(&b, &a)),
                _ => (false, usub(&a, &b)),
            },
            (true, false) => match cmp {
                Ordering::Less => (false, usub(&b, &a)),
                _ => (true, usub(&a, &b)),
            },
        };
        Self::from_unscaled(negative, &magnitude, scale)
    }

    /// Exact decimal `*` (MySQL `DecimalMul`). The product keeps `s1 + s2`
    /// fractional digits, capped at MySQL's maximum scale of 30 (rounded).
    pub fn checked_mul(&self, other: &Self) -> Self {
        let (int1, frac1) = self.int_frac();
        let (int2, frac2) = other.int_frac();
        let a = format!("{int1}{frac1}");
        let b = format!("{int2}{frac2}");
        let product = umul(&a, &b);
        let scale = frac1.len() + frac2.len();
        Self::from_unscaled(self.negative != other.negative, &product, scale)
            .with_frac((scale.min(MAX_DECIMAL_SCALE)) as i32)
    }

    /// Exact decimal `/` (MySQL `DecimalDiv`): `frac_incr` extra fractional
    /// digits over the dividend's scale, capped at 30 and rounded.
    pub fn checked_div(&self, other: &Self, frac_incr: usize) -> Option<Self> {
        if other.digits.chars().all(|c| c == '0' || c == '.') {
            return None;
        }
        let (int1, frac1) = self.int_frac();
        let (int2, frac2) = other.int_frac();
        let scale = (frac1.len() + frac_incr).min(MAX_DECIMAL_SCALE);
        // a / b = (A * 10^(s2 + scale)) / (B * 10^s1), with A/B the literal
        // digit strings.
        let mut num = format!("{int1}{frac1}");
        num.push_str(&"0".repeat(frac2.len() + scale));
        let mut den = format!("{int2}{frac2}");
        den.push_str(&"0".repeat(frac1.len()));
        let (mut quotient, remainder) = udivmod(&num, &den);
        // Round half away from zero on the next digit.
        if ucmp(&uadd(&remainder, &remainder), &den) != Ordering::Less {
            quotient = uadd(&quotient, "1");
        }
        Some(Self::from_unscaled(
            self.negative != other.negative,
            &quotient,
            scale,
        ))
    }

    fn int_frac(&self) -> (String, String) {
        match self.digits.split_once('.') {
            Some((int, frac)) => (int.to_string(), frac.to_string()),
            None => (self.digits.clone(), String::new()),
        }
    }

    /// Build a decimal from an unscaled digit string and its implied scale.
    fn from_unscaled(negative: bool, digits: &str, scale: usize) -> Self {
        let digits = strip_leading_zeros(digits);
        let text = if scale == 0 {
            digits
        } else if digits.len() > scale {
            let split = digits.len() - scale;
            format!("{}.{}", &digits[..split], &digits[split..])
        } else {
            format!("0.{}{}", "0".repeat(scale - digits.len()), digits)
        };
        let mut value = Self::parse(&text).unwrap_or_else(|_| Self::from_i64(0));
        let is_zero = value.digits.bytes().all(|b| b == b'0' || b == b'.');
        value.negative = negative && !is_zero;
        value
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
        // Compare digit strings: `DECIMAL(65,2)` values past 2^53 (`±92233…`)
        // are distinct but equal as `f64`.
        let (self_neg, a) = split_decimal_sign(&self.to_string());
        let (other_neg, b) = split_decimal_sign(&other.to_string());
        match (self_neg, other_neg) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => decimal_abs_cmp(&a, &b),
            (true, true) => decimal_abs_cmp(&a, &b).reverse(),
        }
    }
}

/// `("-12.30")` → `(true, "12.30")`.
fn split_decimal_sign(text: &str) -> (bool, String) {
    match text.strip_prefix('-') {
        Some(rest) => (true, rest.to_string()),
        None => (false, text.trim_start_matches('+').to_string()),
    }
}

/// Magnitude comparison of `"12.30"` forms: integer digit runs first, then the
/// fractions zero-padded to a common width.
fn decimal_abs_cmp(a: &str, b: &str) -> Ordering {
    let (a_int, a_frac) = a.split_once('.').unwrap_or((a, ""));
    let (b_int, b_frac) = b.split_once('.').unwrap_or((b, ""));
    ucmp(a_int, b_int).then_with(|| {
        let width = a_frac.len().max(b_frac.len());
        let a_frac = format!("{a_frac:0<width$}");
        let b_frac = format!("{b_frac:0<width$}");
        a_frac.cmp(&b_frac)
    })
}

/// MySQL's maximum `DECIMAL` scale.
pub const MAX_DECIMAL_SCALE: usize = 30;

fn strip_leading_zeros(s: &str) -> String {
    let trimmed = s.trim_start_matches('0');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Compare two unsigned integer digit strings (no sign, leading zeros ok).
fn ucmp(a: &str, b: &str) -> Ordering {
    let a = strip_leading_zeros(a);
    let b = strip_leading_zeros(b);
    a.len().cmp(&b.len()).then_with(|| a.cmp(&b))
}

/// Schoolbook addition of two unsigned integer digit strings.
fn uadd(a: &str, b: &str) -> String {
    let a: Vec<u8> = a.bytes().rev().map(|c| c - b'0').collect();
    let b: Vec<u8> = b.bytes().rev().map(|c| c - b'0').collect();
    let mut out = Vec::with_capacity(a.len().max(b.len()) + 1);
    let mut carry = 0u8;
    for i in 0..a.len().max(b.len()) {
        let sum = a.get(i).copied().unwrap_or(0) + b.get(i).copied().unwrap_or(0) + carry;
        out.push(sum % 10);
        carry = sum / 10;
    }
    if carry > 0 {
        out.push(carry);
    }
    let text: String = out.iter().rev().map(|d| (d + b'0') as char).collect();
    strip_leading_zeros(&text)
}

/// `a - b` for unsigned digit strings with `a >= b`.
fn usub(a: &str, b: &str) -> String {
    let a: Vec<u8> = a.bytes().rev().map(|c| c - b'0').collect();
    let b: Vec<u8> = b.bytes().rev().map(|c| c - b'0').collect();
    let mut out = Vec::with_capacity(a.len());
    let mut borrow = 0i8;
    for (i, digit) in a.iter().enumerate() {
        let mut diff = *digit as i8 - borrow - b.get(i).copied().unwrap_or(0) as i8;
        borrow = 0;
        if diff < 0 {
            diff += 10;
            borrow = 1;
        }
        out.push(diff as u8);
    }
    let text: String = out.iter().rev().map(|d| (d + b'0') as char).collect();
    strip_leading_zeros(&text)
}

/// Schoolbook multiplication of two unsigned integer digit strings.
fn umul(a: &str, b: &str) -> String {
    let a: Vec<u32> = a.bytes().map(|c| u32::from(c - b'0')).collect();
    let b: Vec<u32> = b.bytes().map(|c| u32::from(c - b'0')).collect();
    if a.iter().all(|d| *d == 0) || b.iter().all(|d| *d == 0) {
        return "0".to_string();
    }
    let mut out = vec![0u32; a.len() + b.len()];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            let pos = i + j + 1;
            let cur = out[pos] + a[i] * b[j];
            out[pos] = cur % 10;
            out[pos - 1] += cur / 10;
        }
    }
    let text: String = out
        .iter()
        .map(|d| (d + u32::from(b'0')) as u8 as char)
        .collect();
    strip_leading_zeros(&text)
}

/// Long division `(quotient, remainder)` of unsigned digit strings.
fn udivmod(a: &str, b: &str) -> (String, String) {
    let a = strip_leading_zeros(a);
    let b = strip_leading_zeros(b);
    if ucmp(&a, &b) == Ordering::Less {
        return ("0".to_string(), a);
    }
    let mut quotient = String::new();
    let mut remainder = String::from("0");
    for digit in a.chars() {
        remainder.push(digit);
        remainder = strip_leading_zeros(&remainder);
        let mut count = 0u8;
        while ucmp(&remainder, &b) != Ordering::Less {
            remainder = usub(&remainder, &b);
            count += 1;
        }
        quotient.push((b'0' + count) as char);
    }
    (strip_leading_zeros(&quotient), remainder)
}

#[cfg(test)]
mod arith_tests {
    use super::*;

    fn d(text: &str) -> MyDecimal {
        MyDecimal::parse(text).unwrap()
    }

    #[test]
    fn exact_add_sub_keep_scale() {
        assert_eq!(d("1.10").checked_add(&d("2"), false).to_string(), "3.10");
        assert_eq!(d("1").checked_add(&d("2.5"), false).to_string(), "3.5");
        assert_eq!(d("1.5").checked_add(&d("2.5"), true).to_string(), "-1.0");
        assert_eq!(d("2.5").checked_add(&d("1.5"), true).to_string(), "1.0");
        assert_eq!(d("-1.5").checked_add(&d("1.5"), false).to_string(), "0.0");
    }

    #[test]
    fn exact_mul_caps_scale_at_30() {
        assert_eq!(d("1.5").checked_mul(&d("2")).to_string(), "3.0");
        let big = d("2.00000000000000000000000000000001")
            .checked_mul(&d("1.000000000000000000000000000000000000000000002"));
        assert_eq!(big.to_string(), "2.000000000000000000000000000000");
        let p = d("0.5999991229316").checked_mul(&d("0.918755041726043"));
        assert_eq!(p.with_frac(17).to_string(), "0.55125221922461136");
    }

    #[test]
    fn exact_div_uses_frac_increment() {
        assert_eq!(
            d("1").checked_div(&d("3"), 4).unwrap().to_string(),
            "0.3333"
        );
        let one = format!("1.{}", "0".repeat(30));
        let three = format!("3.{}", "0".repeat(30));
        assert_eq!(
            d(&one).checked_div(&d(&three), 4).unwrap().to_string(),
            format!("0.{}", "3".repeat(30))
        );
        assert!(d("1").checked_div(&d("0"), 4).is_none());
    }
}
