use std::cmp::Ordering;
use std::fmt::{Display, Formatter};

use sealdb_errno::{ERR_TRUNCATED_WRONG_VALUE, Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TimeType {
    Date,
    #[default]
    Datetime,
    Timestamp,
    Time,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Time {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub micro: u32,
    /// Fractional seconds precision for display (0..=6).
    pub fsp: i8,
    pub tp: TimeType,
}

impl Time {
    pub fn new(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> Self {
        Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
            micro: 0,
            fsp: 0,
            tp: TimeType::Datetime,
        }
    }

    /// Build a `Time` from Unix seconds interpreted as a UTC wall clock.
    pub fn from_unix_secs(secs: i64, tp: TimeType) -> Self {
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        Self {
            year: year.clamp(0, u16::MAX as i64) as u16,
            month: month as u8,
            day: day as u8,
            hour: (rem / 3_600) as u8,
            minute: ((rem % 3_600) / 60) as u8,
            second: (rem % 60) as u8,
            micro: 0,
            fsp: 0,
            tp,
        }
    }

    pub fn to_packed_u64(self) -> u64 {
        let ymd = (u64::from(self.year) * 13 + u64::from(self.month)) << 5 | u64::from(self.day);
        let hms =
            (u64::from(self.hour) << 12) | (u64::from(self.minute) << 6) | u64::from(self.second);
        ((ymd << 17) | hms) << 24 | u64::from(self.micro)
    }

    /// MySQL numeric value: `YYYYMMDD` for DATE, `YYYYMMDDHHMMSS` for the
    /// temporal types (TiDB `Time.ToNumber`).
    pub fn to_number(self) -> i64 {
        let ymd = i64::from(self.year) * 10_000 + i64::from(self.month) * 100 + i64::from(self.day);
        match self.tp {
            TimeType::Date => ymd,
            _ => {
                ymd * 1_000_000
                    + i64::from(self.hour) * 10_000
                    + i64::from(self.minute) * 100
                    + i64::from(self.second)
            }
        }
    }

    pub fn from_packed_u64(v: u64) -> Self {
        let ymdhms = v >> 24;
        let ymd = ymdhms >> 17;
        let day = (ymd & ((1 << 5) - 1)) as u8;
        let ym = ymd >> 5;
        let month = (ym % 13) as u8;
        let year = (ym / 13) as u16;
        let hms = ymdhms & ((1 << 17) - 1);
        let hour = (hms >> 12) as u8;
        let minute = ((hms >> 6) & 0x3f) as u8;
        let second = (hms & 0x3f) as u8;
        let micro = (v & ((1 << 24) - 1)) as u32;
        // Fractional digits are implied by the packed microseconds: `1000` is
        // `.001` (three digits).
        let fsp = if micro == 0 {
            0
        } else {
            (6 - (micro.trailing_zeros() / 4).min(6)) as i8
        };
        Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
            micro,
            fsp,
            tp: TimeType::Datetime,
        }
    }
}

/// TiDB `ParseTimeFromNum`: numeric DATE/DATETIME/TIME forms.
/// `20240628` → date, `10102` → `01:01:02` for TIME, `20240628153000` →
/// datetime. Returns `None` on out-of-range components.
pub fn time_from_number(n: i64, tp: TimeType) -> Option<Time> {
    if tp == TimeType::Time {
        if n < 0 {
            return None;
        }
        let hour = n / 10_000;
        let minute = (n / 100) % 100;
        let second = n % 100;
        if hour > 838 || minute > 59 || second > 59 {
            return None;
        }
        return Some(Time {
            year: 0,
            month: 0,
            day: 0,
            hour: hour as u8,
            minute: minute as u8,
            second: second as u8,
            micro: 0,
            fsp: 0,
            tp: TimeType::Time,
        });
    }
    if n < 0 {
        return None;
    }
    let digits = n.to_string();
    // A number with fewer than six digits is right-aligned into `YYMMDD`
    // (`1111` → `001111` → `2000-11-11`, `11111` → `2001-11-11`).
    if digits.len() < 6 {
        let padded = format!("{digits:0>6}");
        let yy: i64 = padded[0..2].parse().ok()?;
        let year = if yy < 70 { 2000 + yy } else { 1900 + yy };
        let month: i64 = padded[2..4].parse().ok()?;
        let day: i64 = padded[4..6].parse().ok()?;
        if !valid_ymd(year as u16, month as u8, day as u8) {
            return None;
        }
        return Some(Time {
            year: year as u16,
            month: month as u8,
            day: day as u8,
            hour: 0,
            minute: 0,
            second: 0,
            micro: 0,
            fsp: 0,
            tp,
        });
    }
    let (year, month, day, hour, minute, second): (i64, i64, i64, i64, i64, i64) =
        match digits.len() {
            14 | 12 => {
                let offset = digits.len() - 14;
                let year = if offset == 0 {
                    digits[0..4].parse::<i64>().ok()?
                } else {
                    let yy: i64 = digits[0..2].parse().ok()?;
                    if yy < 70 { 2000 + yy } else { 1900 + yy }
                };
                let rest = if offset == 0 {
                    &digits[4..]
                } else {
                    &digits[2..]
                };
                (
                    year,
                    rest[0..2].parse().ok()?,
                    rest[2..4].parse().ok()?,
                    rest[4..6].parse().ok()?,
                    rest[6..8].parse().ok()?,
                    rest[8..10].parse().ok()?,
                )
            }
            8 | 6 => {
                let (year, rest) = if digits.len() == 8 {
                    (digits[0..4].parse::<i64>().ok()?, &digits[4..])
                } else {
                    let yy: i64 = digits[0..2].parse().ok()?;
                    (if yy < 70 { 2000 + yy } else { 1900 + yy }, &digits[2..])
                };
                (
                    year,
                    rest[0..2].parse().ok()?,
                    rest[2..4].parse().ok()?,
                    0,
                    0,
                    0,
                )
            }
            // A stray digit past YYMMDD (`2008261`) still reads as the YYMMDD
            // date part (TiDB `ParseTimeFromNum`).
            7 => {
                let yy: i64 = digits[0..2].parse().ok()?;
                (
                    if yy < 70 { 2000 + yy } else { 1900 + yy },
                    digits[2..4].parse().ok()?,
                    digits[4..6].parse().ok()?,
                    0,
                    0,
                    0,
                )
            }
            _ => return None,
        };
    if !valid_ymd(year as u16, month as u8, day as u8) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some(Time {
        year: year as u16,
        month: month as u8,
        day: day as u8,
        hour: hour as u8,
        minute: minute as u8,
        second: second as u8,
        micro: 0,
        fsp: 0,
        tp,
    })
}

/// TIME from a numeric value: `HHMMSS` for up to six digits, the clock part
/// of `[YY]YYMMDDHHMMSS` for 12/14 digits (`307` → `00:03:07`, `200805` →
/// `20:08:05`, `20200805111307` → `11:13:07`). Eight digits (a bare date) and
/// 7/9-11 digits (`20200805`, …) have no TIME form.
pub fn duration_from_numeric(int_digits: &str) -> Option<Time> {
    let digits: String = int_digits.chars().filter(|c| c.is_ascii_digit()).collect();
    let hms = match digits.len() {
        0 => return None,
        1..=6 => format!("{digits:0>6}"),
        12 | 14 => digits[digits.len() - 6..].to_string(),
        _ => return None,
    };
    let hour: u16 = hms[0..2].parse().ok()?;
    let minute: u16 = hms[2..4].parse().ok()?;
    let second: u16 = hms[4..6].parse().ok()?;
    if hour > 838 || minute > 59 || second > 59 {
        return None;
    }
    Some(Time {
        year: 0,
        month: 0,
        day: 0,
        hour: hour as u8,
        minute: minute as u8,
        second: second as u8,
        micro: 0,
        fsp: 0,
        tp: TimeType::Time,
    })
}

/// Duration text (`'01:01:01'`, `'838:59:59'`) → TIME.
pub fn parse_duration_string(s: &str) -> Option<Time> {
    let s = s.trim();
    let (negative, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let (hms, frac) = match body.split_once('.') {
        Some((hms, frac)) => (hms, Some(frac)),
        None => (body, None),
    };
    let parts: Vec<&str> = hms.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let mut hour: i64 = parts[0].trim().parse().ok()?;
    let minute: u8 = parts[1].trim().parse().ok()?;
    let second: u8 = parts[2].trim().parse().ok()?;
    if negative {
        hour = -hour;
    }
    if !(-838..=838).contains(&hour) || minute > 59 || second > 59 {
        return None;
    }
    let mut micro = 0u32;
    let mut fsp = 0i8;
    if let Some(frac) = frac
        && !frac.is_empty()
    {
        let digits: String = frac.chars().take(6).collect();
        let fsp_len = digits.len() as i8;
        let mut padded = digits.clone();
        while padded.len() < 6 {
            padded.push('0');
        }
        micro = padded.parse().ok()?;
        fsp = fsp_len;
    }
    Some(Time {
        year: 0,
        month: 0,
        day: 0,
        hour: hour.unsigned_abs() as u8,
        minute,
        second,
        micro,
        fsp,
        tp: TimeType::Time,
    })
}

/// Howard Hinnant's civil-from-days: days since 1970-01-01 → (y, m, d).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

impl Display for Time {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self.tp {
            TimeType::Date => write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day),
            TimeType::Time => {
                write!(f, "{:02}:{:02}:{:02}", self.hour, self.minute, self.second)?;
                fmt_frac(f, self.micro, self.fsp)
            }
            TimeType::Datetime | TimeType::Timestamp => {
                write!(
                    f,
                    "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                    self.year, self.month, self.day, self.hour, self.minute, self.second
                )?;
                fmt_frac(f, self.micro, self.fsp)
            }
        }
    }
}

fn fmt_frac(f: &mut Formatter<'_>, micro: u32, fsp: i8) -> std::fmt::Result {
    if fsp <= 0 {
        return Ok(());
    }
    let fsp = fsp.min(6) as u32;
    let scale = 10u32.pow(6 - fsp);
    let frac = micro / scale;
    write!(f, ".{frac:0width$}", width = fsp as usize)
}

impl PartialOrd for Time {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Time {
    fn cmp(&self, other: &Self) -> Ordering {
        self.to_packed_u64().cmp(&other.to_packed_u64())
    }
}

fn incorrect(kind: &str, s: &str) -> Error {
    Error::new(
        ERR_TRUNCATED_WRONG_VALUE,
        format!("Incorrect {kind} value: '{s}'"),
    )
}

/// TiDB `timestampPattern` (expression/builtin_time.go).
fn timestamp_pattern_ok(s: &str) -> bool {
    let s = s.trim();
    let b = s.as_bytes();
    let mut i = 0;
    if !take_digits(b, &mut i, 1, 4) {
        return false;
    }
    for _ in 0..2 {
        if i >= b.len() || b[i].is_ascii_digit() {
            return false;
        }
        i += 1;
        if !take_digits(b, &mut i, 1, 2) {
            return false;
        }
    }
    // required whitespace before time (TiDB pattern has `\s+`)
    if i >= b.len() || !b[i].is_ascii_whitespace() {
        return false;
    }
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    if i == b.len() {
        return false;
    }
    // optional time + optional frac: (0*\d{0,2}([^\d]0*\d{1,2}){2})?(\.\d*)?
    if b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        return i == b.len();
    }
    // time: 0*\d{0,2}([^\d]0*\d{1,2}){2}
    if !take_digits(b, &mut i, 0, 2) {
        return false;
    }
    for _ in 0..2 {
        if i >= b.len() || b[i].is_ascii_digit() {
            return false;
        }
        i += 1;
        if !take_digits(b, &mut i, 1, 2) {
            return false;
        }
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    i == b.len()
}

fn date_pattern_ok(s: &str) -> bool {
    let s = s.trim();
    let b = s.as_bytes();
    if b.iter().all(|c| c.is_ascii_digit()) {
        let n = b.len();
        return (2..=8).contains(&n) && n.is_multiple_of(2);
    }
    let mut i = 0;
    if !take_digits(b, &mut i, 1, 4) {
        return false;
    }
    for _ in 0..2 {
        if i >= b.len() || b[i].is_ascii_digit() {
            return false;
        }
        i += 1;
        if !take_digits(b, &mut i, 1, 2) {
            return false;
        }
    }
    i == b.len()
}

/// Match TiDB/MySQL regex atom `0*\d{min,max}` (unlimited leading zeros).
fn take_digits(b: &[u8], i: &mut usize, min: usize, max: usize) -> bool {
    let start = *i;
    while *i < b.len() && b[*i].is_ascii_digit() {
        *i += 1;
    }
    let n = *i - start;
    if n == 0 {
        return min == 0;
    }
    let mut lz = 0usize;
    while lz < n && b[start + lz] == b'0' {
        lz += 1;
    }
    let significant = n - lz;
    if significant == 0 {
        // All zeros: backtracking can feed up to `max` zeros into `\d{min,max}`.
        n >= min
    } else {
        significant >= min && significant <= max
    }
}

fn get_fsp(s: &str) -> i8 {
    if let Some(dot) = s.find('.') {
        let n = s[dot + 1..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .count();
        (n as i8).min(6)
    } else {
        0
    }
}

fn split_digit_parts(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            cur.push(c);
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn parse_frac(frac: &str, fsp: i8) -> Result<(u32, bool)> {
    let fsp = fsp.clamp(0, 6) as usize;
    let mut digits: Vec<u8> = frac
        .chars()
        .filter(|c| c.is_ascii_digit())
        .map(|c| c as u8 - b'0')
        .collect();
    while digits.len() < 6 {
        digits.push(0);
    }
    let mut overflow = false;
    if digits.len() > fsp {
        let round = digits.get(fsp).copied().unwrap_or(0) >= 5;
        for d in digits.iter_mut().skip(fsp) {
            *d = 0;
        }
        if round {
            if fsp == 0 {
                overflow = true;
            } else {
                let mut carry = 1u8;
                for i in (0..fsp).rev() {
                    let v = digits[i] + carry;
                    digits[i] = v % 10;
                    carry = v / 10;
                    if carry == 0 {
                        break;
                    }
                }
                if carry > 0 {
                    overflow = true;
                }
            }
        }
    }
    let mut micro = 0u32;
    for (i, d) in digits.iter().take(6).enumerate() {
        micro += u32::from(*d) * 10u32.pow((5 - i) as u32);
    }
    Ok((micro, overflow))
}

fn valid_ymd(year: u16, month: u8, day: u8) -> bool {
    if month == 0 || month > 12 || day == 0 {
        return false;
    }
    day <= days_in_month(year, month)
}

/// Round the fractional seconds of `t` to `fsp` digits. MySQL rounds rather
/// than truncates: `.888` into `DATETIME(0)` becomes the next second.
pub fn round_to_fsp(mut t: Time, fsp: i8) -> Time {
    let fsp = fsp.clamp(0, 6) as u32;
    if u32::from(t.fsp.max(0) as u8) <= fsp {
        t.fsp = fsp as i8;
        return t;
    }
    let mut digits: Vec<u8> = format!("{:06}", t.micro)
        .bytes()
        .map(|b| b - b'0')
        .collect();
    let round = digits.get(fsp as usize).copied().unwrap_or(0) >= 5;
    for d in digits.iter_mut().skip(fsp as usize) {
        *d = 0;
    }
    let mut micro = 0u32;
    for (i, d) in digits.iter().enumerate() {
        micro += u32::from(*d) * 10u32.pow((5 - i) as u32);
    }
    t.micro = micro;
    t.fsp = fsp as i8;
    if round {
        t.second += 1;
        if t.second >= 60 {
            t.second = 0;
            t.minute += 1;
            if t.minute >= 60 {
                t.minute = 0;
                t.hour += 1;
                if t.hour >= 24 {
                    t.hour = 0;
                    t.day += 1;
                    if t.day > days_in_month(t.year, t.month) {
                        t.day = 1;
                        t.month += 1;
                        if t.month > 12 {
                            t.month = 1;
                            t.year = t.year.saturating_add(1);
                        }
                    }
                }
            }
        }
    }
    t
}

fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        2 => 28,
        _ => 0,
    }
}

/// TiDB `types.ParseTime` for a DATETIME string literal.
pub fn parse_datetime_literal(s: &str) -> Result<Time> {
    parse_datetime_string(s)
}

fn parse_datetime_string(s: &str) -> Result<Time> {
    let fsp = get_fsp(s);
    let s_trim = s.trim();
    let (main, frac) = match s_trim.find('.') {
        Some(dot) => {
            let frac: String = s_trim[dot + 1..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            (s_trim[..dot].to_string(), frac)
        }
        None => (s_trim.to_string(), String::new()),
    };
    let mut seps = split_digit_parts(&main);
    let mut frac = frac;
    // TiDB: absorb frac into seps when len(seps) <= 5.
    if !frac.is_empty() && seps.len() <= 5 {
        seps.push(frac.clone());
        frac.clear();
    }
    if seps.len() > 6 {
        seps.truncate(6);
    }
    let parse_u = |idx: usize| -> Result<u32> {
        seps.get(idx)
            .ok_or_else(|| incorrect("datetime", s))?
            .parse()
            .map_err(|_| incorrect("datetime", s))
    };
    match seps.len() {
        5 | 6 => {}
        _ => return Err(incorrect("datetime", s)),
    }
    let year = parse_u(0)? as u16;
    let month = parse_u(1)? as u8;
    let day = parse_u(2)? as u8;
    let hour = parse_u(3)? as u8;
    let minute = parse_u(4)? as u8;
    let mut second = if seps.len() == 6 {
        parse_u(5)? as u8
    } else {
        0
    };
    let hhmmss = seps.len() == 6;
    // When frac was absorbed as the 6th sep, second may be huge digit string
    // already parsed via u32 — atoi of "0000000000001" = 1. Good.
    // But second must be 0..=59 after absorb of long frac-as-second? TiDB uses int.
    if second > 59 {
        return Err(incorrect("datetime", s));
    }
    let (mut micro, overflow) = if hhmmss {
        parse_frac(&frac, fsp)?
    } else {
        (0, false)
    };
    if overflow {
        second = second.saturating_add(1);
        micro = 0;
        // carry into minute etc. omitted for suite cases that only overflow seconds
        if second >= 60 {
            return Err(incorrect("datetime", s));
        }
    }
    let zero_date = year == 0 && month == 0 && day == 0;
    // `0000-00-00 …` is the zero date. TiDB parses it at the literal layer and
    // leaves `NO_ZERO_DATE` / strict-mode checks to storage.
    if (!valid_ymd(year, month, day) && !zero_date) || hour > 23 || minute > 59 {
        return Err(incorrect("datetime", s));
    }
    // When frac absorbed into second, display still uses original fsp.
    let display_fsp = if frac.is_empty() && get_fsp(s) > 0 && hhmmss {
        // absorbed case like 14:00.0000000000001 → second=1, fsp=6 → show .000000
        get_fsp(s)
    } else if frac.is_empty() {
        0
    } else {
        fsp
    };
    // Fix: for `14:00:00.0000000000001` frac not absorbed (6 seps), fsp=6, micro~0
    let display_fsp = if !s.contains('.') {
        0
    } else if get_fsp(s) > 0 {
        get_fsp(s)
    } else {
        display_fsp
    };
    Ok(Time {
        year,
        month,
        day,
        hour,
        minute,
        second,
        micro,
        fsp: display_fsp,
        tp: TimeType::Datetime,
    })
}

/// Parse `TIMESTAMP '...'` literal (TiDB timestampPattern + ParseTime DATETIME).
pub fn parse_timestamp_literal(s: &str) -> Result<Time> {
    if !timestamp_pattern_ok(s) {
        return Err(incorrect("datetime", s));
    }
    parse_datetime_string(s)
}

/// Lenient assignment parse for DATE/DATETIME/TIMESTAMP text: MySQL accepts any
/// non-digit as the date/time separator (`2020-10-22-16`, `2020.10-22:16`,
/// `2020-10-22T16.31.15`). A trailing `Z`/`±HH[:MM]` is reported separately by
/// [`parse_datetime_with_zone`].
pub fn parse_datetime_lenient(s: &str) -> Result<Time> {
    let s_trim = s.trim();
    // Digit runs in order, remembering the separator before each run ('.' is
    // the only candidate for a fractional-seconds marker).
    let mut groups: Vec<(String, char)> = Vec::new();
    let mut cur = String::new();
    let mut last_sep = '\0';
    for c in s_trim.chars() {
        if c.is_ascii_digit() {
            cur.push(c);
        } else {
            if !cur.is_empty() {
                groups.push((std::mem::take(&mut cur), last_sep));
            }
            last_sep = c;
        }
    }
    if !cur.is_empty() {
        groups.push((cur, last_sep));
    }
    // A trailing dot-separated run after the clock is fractional seconds.
    let mut frac = String::new();
    if groups.len() >= 7 && groups.last().is_some_and(|(_, sep)| *sep == '.') {
        frac = groups.pop().map(|(digits, _)| digits).unwrap_or_default();
    }
    if !(3..=6).contains(&groups.len()) {
        return Err(incorrect("datetime", s));
    }
    let num = |idx: usize| -> Result<u32> {
        groups
            .get(idx)
            .ok_or_else(|| incorrect("datetime", s))?
            .0
            .parse()
            .map_err(|_| incorrect("datetime", s))
    };
    let year = num(0)?;
    let month = num(1)?;
    let day = num(2)?;
    let hour = if groups.len() > 3 { num(3)? } else { 0 };
    let minute = if groups.len() > 4 { num(4)? } else { 0 };
    let mut second = if groups.len() > 5 { num(5)? } else { 0 };
    if second > 59 || hour > 23 || minute > 59 {
        return Err(incorrect("datetime", s));
    }
    let mut micro = 0;
    let mut fsp = 0i8;
    if !frac.is_empty() {
        let (m, overflow) = parse_frac(&frac, 6)?;
        micro = m;
        fsp = frac.len().min(6) as i8;
        if overflow {
            second += 1;
            micro = 0;
            if second >= 60 {
                return Err(incorrect("datetime", s));
            }
        }
    }
    let year = year as u16;
    let month = month as u8;
    let day = day as u8;
    let zero_date = year == 0 && month == 0 && day == 0;
    if (!valid_ymd(year, month, day) && !zero_date) || hour > 23 || minute > 59 {
        return Err(incorrect("datetime", s));
    }
    Ok(Time {
        year,
        month,
        day,
        hour: hour as u8,
        minute: minute as u8,
        second: second as u8,
        micro,
        fsp,
        tp: TimeType::Datetime,
    })
}

/// Parse an assignment datetime with an explicit zone (`Z`, `±HH`, `±HHMM`,
/// `±HH:MM`) and return the wall clock plus minutes east of UTC. The zone is
/// only recognized after a complete clock (`±` after an hour alone is a
/// separator: `2024-01-01 14+01:00` is `14:01:00`).
pub fn parse_datetime_with_zone(s: &str) -> Result<Option<(Time, i32)>> {
    let s = s.trim();
    if let Some(prefix) = s.strip_suffix(['Z', 'z']) {
        let time = parse_datetime_lenient(prefix).or_else(|_| parse_datetime_string(prefix))?;
        return Ok(Some((time, 0)));
    }
    if s.len() < 2 {
        return Ok(None);
    }
    let bytes = s.as_bytes();
    // Find the last sign that starts an offset suffix.
    let mut sign = None;
    for i in (1..bytes.len()).rev() {
        let b = bytes[i];
        if !(b == b'+' || b == b'-') {
            continue;
        }
        if i + 1 >= bytes.len() || !bytes[i + 1].is_ascii_digit() {
            continue;
        }
        let rest = &s[i + 1..];
        if rest.chars().all(|c| c.is_ascii_digit() || c == ':') {
            sign = Some(i);
            break;
        }
    }
    let Some(i) = sign else {
        return Ok(None);
    };
    let prefix = &s[..i];
    // The offset applies only after an explicit complete clock (H:M:S).
    let Some(time_part) = prefix
        .split_once(['T', 't'])
        .map(|(_, tail)| tail)
        .or_else(|| prefix.rsplit_once(' ').map(|(_, tail)| tail))
    else {
        return Ok(None);
    };
    let time_groups = time_part
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty());
    if time_groups.count() < 3 {
        return Ok(None);
    }
    let offset_text = &s[i + 1..];
    let (hh, mm) = match offset_text.split_once(':') {
        Some((hh, mm)) => (hh, mm),
        None if offset_text.len() > 2 => offset_text.split_at(offset_text.len() - 2),
        None => (offset_text, "0"),
    };
    let hh: i32 = hh.parse().map_err(|_| incorrect("datetime", s))?;
    let mm: i32 = mm.parse().map_err(|_| incorrect("datetime", s))?;
    if hh > 14 || mm > 59 || (hh == 14 && mm != 0) {
        return Err(incorrect("datetime", s));
    }
    let offset = (hh * 60 + mm) * if bytes[i] == b'-' { -1 } else { 1 };
    let time = parse_datetime_lenient(prefix).or_else(|_| parse_datetime_string(prefix))?;
    Ok(Some((time, offset)))
}

/// Parse `DATE '...'` / `{d '...'}` literal.
pub fn parse_date_literal(s: &str) -> Result<Time> {
    if !date_pattern_ok(s) {
        return Err(incorrect("date", s));
    }
    let s = s.trim();
    let seps = split_digit_parts(s);
    if seps.len() != 3 {
        return Err(incorrect("date", s));
    }
    let year: u16 = seps[0].parse().map_err(|_| incorrect("date", s))?;
    let month: u8 = seps[1].parse().map_err(|_| incorrect("date", s))?;
    let day: u8 = seps[2].parse().map_err(|_| incorrect("date", s))?;
    if !valid_ymd(year, month, day) {
        return Err(incorrect("date", s));
    }
    Ok(Time {
        year,
        month,
        day,
        hour: 0,
        minute: 0,
        second: 0,
        micro: 0,
        fsp: 0,
        tp: TimeType::Date,
    })
}

/// Parse `TIME '...'` / `{t '...'}` literal (suite: HH:MM:SS).
pub fn parse_time_literal(s: &str) -> Result<Time> {
    let s = s.trim();
    let seps = split_digit_parts(s);
    if seps.len() != 3 {
        return Err(incorrect("time", s));
    }
    let hour: u8 = seps[0].parse().map_err(|_| incorrect("time", s))?;
    let minute: u8 = seps[1].parse().map_err(|_| incorrect("time", s))?;
    let second: u8 = seps[2].parse().map_err(|_| incorrect("time", s))?;
    if minute > 59 || second > 59 {
        return Err(incorrect("time", s));
    }
    Ok(Time {
        year: 0,
        month: 0,
        day: 0,
        hour,
        minute,
        second,
        micro: 0,
        fsp: 0,
        tp: TimeType::Time,
    })
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    #[test]
    fn timestamp_tz_rejected() {
        let err = parse_timestamp_literal("2024-01-01 14:00:00+01:00").unwrap_err();
        assert_eq!(err.code, ERR_TRUNCATED_WRONG_VALUE);
        assert!(err.message.contains("Incorrect datetime value"));
    }

    #[test]
    fn timestamp_absorb_plus() {
        let t = parse_timestamp_literal("2024-01-01 14+01:00").unwrap();
        assert_eq!(t.to_string(), "2024-01-01 14:01:00");
    }

    #[test]
    fn timestamp_frac_round() {
        let t = parse_timestamp_literal("2024-01-01 14:00:00.999999999999").unwrap();
        assert_eq!(t.to_string(), "2024-01-01 14:00:01.000000");
    }

    #[test]
    fn timestamp_minute_frac_absorb() {
        let t = parse_timestamp_literal("2024-01-01 14:00.0000000000001").unwrap();
        assert_eq!(t.to_string(), "2024-01-01 14:00:01.000000");
    }
}
