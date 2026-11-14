//! TiDB `planner/cardinality` range estimation subset (stats version 2).
//!
//! `LOAD STATS` fills histograms straight from TiDB's stats JSON, so the
//! planner can reproduce the exact `estRows` of a TiDB EXPLAIN:
//!
//! * column bounds are stored as text and interpolated through a typed scalar
//!   (`convertDatumToScalar`),
//! * index bounds are stored as `codec.EncodeKey` bytes and interpolated on the
//!   encoded key (TiDB `Index.Histogram`), which is what makes
//!   `idx_1(col_3) range:[1981-09-15,+inf]` estimate `1860.39` rather than the
//!   column histogram's `1860.38`.
use crate::table::{ColumnStats, Histogram, IndexStats, Table};
use sealdb_types::field_type::{
    TYPE_BLOB, TYPE_DATE, TYPE_DATETIME, TYPE_DECIMAL, TYPE_DOUBLE, TYPE_ENUM, TYPE_FLOAT,
    TYPE_INT24, TYPE_LONG, TYPE_LONG_BLOB, TYPE_LONG_LONG, TYPE_MEDIUM_BLOB, TYPE_NEW_DATE,
    TYPE_NEW_DECIMAL, TYPE_SET, TYPE_SHORT, TYPE_STRING, TYPE_TIMESTAMP, TYPE_TINY, TYPE_TINY_BLOB,
    TYPE_VAR_STRING, TYPE_VARCHAR, TYPE_YEAR,
};
use sealdb_types::{Datum, FieldType, Time};

/// How a histogram bound / literal is turned into an ordered scalar.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum HistValueKind {
    /// Numeric columns: text parses straight to `f64`.
    #[default]
    Number,
    /// DATE / DATETIME / TIMESTAMP: microseconds since 1970-01-01 (linear, so
    /// the interpolation fraction matches TiDB's `Time.Sub(minTime)`).
    Time,
    /// Strings and blobs: raw bytes compared lexicographically.
    Text,
}

/// TiDB `FieldType` → histogram scalar kind.
pub fn kind_of(ft: &FieldType) -> HistValueKind {
    match ft.tp {
        TYPE_TINY | TYPE_SHORT | TYPE_LONG | TYPE_LONG_LONG | TYPE_INT24 | TYPE_YEAR
        | TYPE_FLOAT | TYPE_DOUBLE | TYPE_DECIMAL | TYPE_NEW_DECIMAL => HistValueKind::Number,
        TYPE_DATE | TYPE_DATETIME | TYPE_TIMESTAMP | TYPE_NEW_DATE => HistValueKind::Time,
        TYPE_VARCHAR | TYPE_VAR_STRING | TYPE_STRING | TYPE_BLOB | TYPE_TINY_BLOB
        | TYPE_MEDIUM_BLOB | TYPE_LONG_BLOB | TYPE_ENUM | TYPE_SET => HistValueKind::Text,
        _ => HistValueKind::Number,
    }
}

/// One range predicate as the detacher produced it (`bool` = inclusive).
#[derive(Clone, Debug)]
pub struct StatsRange {
    pub low: Option<(Datum, bool)>,
    pub high: Option<(Datum, bool)>,
}

impl StatsRange {
    pub fn point(value: Datum) -> Self {
        Self {
            low: Some((value.clone(), true)),
            high: Some((value, true)),
        }
    }
}

#[derive(Clone, PartialEq)]
enum Bound {
    /// Integer bound kept exact: `bigint` handles above 2^53 collapse onto the
    /// same `f64`, which would merge two distinct range endpoints.
    Int(i128),
    Num(f64),
    Bytes(Vec<u8>),
}

impl Bound {
    fn cmp(&self, other: &Bound) -> std::cmp::Ordering {
        match (self, other) {
            (Bound::Int(a), Bound::Int(b)) => a.cmp(b),
            (Bound::Num(a), Bound::Num(b)) => a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal),
            (Bound::Bytes(a), Bound::Bytes(b)) => a.cmp(b),
            // Mixed numeric kinds cannot happen for one column; compare through
            // the float space when a histogram bound is a float.
            (Bound::Int(a), Bound::Num(b)) => (*a as f64)
                .partial_cmp(b)
                .unwrap_or(std::cmp::Ordering::Equal),
            (Bound::Num(a), Bound::Int(b)) => a
                .partial_cmp(&(*b as f64))
                .unwrap_or(std::cmp::Ordering::Equal),
            // Mixed kinds cannot happen for one column; fall back to equality.
            _ => std::cmp::Ordering::Equal,
        }
    }
}

/// Days since 1970-01-01 (Howard Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Microseconds since 1970-01-01 for a temporal value.
fn time_scalar(t: &Time) -> f64 {
    let days = days_from_civil(i64::from(t.year), i64::from(t.month), i64::from(t.day));
    (days * 86_400_000_000
        + i64::from(t.hour) * 3_600_000_000
        + i64::from(t.minute) * 60_000_000
        + i64::from(t.second) * 1_000_000
        + i64::from(t.micro)) as f64
}

/// Parse a canonical TiDB temporal bound (`YYYY-MM-DD[ HH:MM:SS[.ffffff]]`).
fn parse_time_scalar(text: &str) -> Option<f64> {
    let text = text.trim();
    let mut parts = text.split(' ');
    let date = parts.next()?;
    let clock = parts.next().unwrap_or("00:00:00");
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    let mut c = clock.split(':');
    let hour: i64 = c.next().unwrap_or("0").parse().ok()?;
    let minute: i64 = c.next().unwrap_or("0").parse().ok()?;
    let sec = c.next().unwrap_or("0");
    let (second, micro) = match sec.split_once('.') {
        Some((s, f)) => {
            let mut frac = f.trim_end_matches('0').to_string();
            frac.truncate(6);
            while frac.len() < 6 {
                frac.push('0');
            }
            (s.parse::<i64>().ok()?, frac.parse::<i64>().ok()?)
        }
        None => (sec.parse::<i64>().ok()?, 0),
    };
    Some(
        (days_from_civil(year, month, day) * 86_400_000_000
            + hour * 3_600_000_000
            + minute * 60_000_000
            + second * 1_000_000
            + micro) as f64,
    )
}

fn number_scalar(text: &str) -> Option<Bound> {
    let text = text.trim();
    // TiDB keeps integer bounds exact (`bigint unsigned` handle values above
    // 2^53 must not collapse onto one `f64`).
    if !text.contains(['.', 'e', 'E'])
        && let Ok(value) = text.parse::<i128>()
    {
        return Some(Bound::Int(value));
    }
    text.parse::<f64>().ok().map(Bound::Num)
}

/// Convert a histogram bound (stored as text) to an ordered scalar.
fn bound_scalar(kind: HistValueKind, text: &str) -> Option<Bound> {
    match kind {
        HistValueKind::Number => number_scalar(text),
        HistValueKind::Time => parse_time_scalar(text).map(Bound::Num),
        HistValueKind::Text => Some(Bound::Bytes(text.as_bytes().to_vec())),
    }
}

/// Convert a literal datum to the same scalar space as the histogram bounds.
fn datum_scalar(kind: HistValueKind, value: &Datum) -> Option<Bound> {
    match (kind, value) {
        (_, Datum::Null) => None,
        (HistValueKind::Number, Datum::I64(v)) => Some(Bound::Int(*v as i128)),
        (HistValueKind::Number, Datum::Year(v)) => Some(Bound::Int(*v as i128)),
        (HistValueKind::Number, Datum::U64(v)) => Some(Bound::Int(*v as i128)),
        (HistValueKind::Number, Datum::F64(v)) => Some(Bound::Num(*v)),
        (HistValueKind::Number, Datum::Decimal(d)) => d.to_f64().ok().map(Bound::Num),
        (HistValueKind::Number, Datum::Bytes(b) | Datum::Bit(b)) => {
            number_scalar(&String::from_utf8_lossy(b))
        }
        (HistValueKind::Number, Datum::Time(t)) => Some(Bound::Num(time_scalar(t))),
        (HistValueKind::Number, Datum::Enum(e)) => Some(Bound::Num(e.number as f64)),
        (HistValueKind::Time, Datum::Time(t)) => Some(Bound::Num(time_scalar(t))),
        (HistValueKind::Time, Datum::Bytes(b)) => {
            parse_time_scalar(&String::from_utf8_lossy(b)).map(Bound::Num)
        }
        (HistValueKind::Text, Datum::Bytes(b) | Datum::Bit(b)) => Some(Bound::Bytes(b.clone())),
        (HistValueKind::Text, Datum::Time(t)) => Some(Bound::Bytes(
            crate::table::datum_key(&Datum::Time(*t)).into_bytes(),
        )),
        (HistValueKind::Text, other) => {
            Some(Bound::Bytes(crate::table::datum_key(other).into_bytes()))
        }
        (HistValueKind::Time, other) => {
            parse_time_scalar(&crate::table::datum_key(other)).map(Bound::Num)
        }
    }
}

fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    let n = a.len().min(b.len());
    (0..n).find(|&i| a[i] != b[i]).unwrap_or(n)
}

/// TiDB `convertBytesToScalar`: base-256 of (at most) the first 8 bytes.
fn base256(bytes: &[u8]) -> f64 {
    let mut buf = [0u8; 8];
    let n = bytes.len().min(8);
    buf[..n].copy_from_slice(&bytes[..n]);
    u64::from_be_bytes(buf) as f64
}

impl Histogram {
    /// TiDB `Histogram.NotNullCount`.
    pub fn not_null_count(&self) -> f64 {
        self.buckets.last().map(|b| b.count as f64).unwrap_or(0.0)
    }

    fn bucket_scalars(&self, idx: usize, kind: HistValueKind) -> Option<(Bound, Bound)> {
        let bucket = self.buckets.get(idx)?;
        Some((
            bound_scalar(kind, &bucket.lower)?,
            bound_scalar(kind, &bucket.upper)?,
        ))
    }

    /// TiDB `Histogram.LocateBucket`: `(in_bucket, bucket_idx, match_last)`.
    fn locate(&self, kind: HistValueKind, value: &Bound) -> (bool, usize, bool) {
        if self.buckets.is_empty() {
            return (false, 0, false);
        }
        for (i, bucket) in self.buckets.iter().enumerate() {
            let Some(upper) = bound_scalar(kind, &bucket.upper) else {
                continue;
            };
            if upper.cmp(value) == std::cmp::Ordering::Less {
                continue;
            }
            let Some((lower, upper)) = self.bucket_scalars(i, kind) else {
                return (false, i, false);
            };
            if lower.cmp(value) == std::cmp::Ordering::Greater {
                return (false, i, false);
            }
            if upper.cmp(value) == std::cmp::Ordering::Equal {
                return (true, i, true);
            }
            return (true, i, false);
        }
        (false, self.buckets.len() - 1, false)
    }

    /// TiDB `convertDatumToScalar` fraction (`calcFraction`).
    fn calc_fraction(lower: &Bound, upper: &Bound, value: &Bound) -> f64 {
        let (l, u, v) = match (lower, upper, value) {
            (Bound::Num(l), Bound::Num(u), Bound::Num(v)) => (*l, *u, *v),
            (Bound::Int(l), Bound::Int(u), Bound::Int(v)) => {
                ((*l as f64), (*u as f64), (*v as f64))
            }
            // Strings interpolate on the base-256 value of the common suffix.
            (Bound::Bytes(l), Bound::Bytes(u), Bound::Bytes(v)) => {
                let prefix = common_prefix(l, u);
                (
                    base256(&l[prefix.min(l.len())..]),
                    base256(&u[prefix.min(u.len())..]),
                    base256(&v[prefix.min(v.len())..]),
                )
            }
            _ => return 0.5,
        };
        if u <= l {
            return 0.5;
        }
        if v <= l {
            return 0.0;
        }
        if v >= u {
            return 1.0;
        }
        let frac = (v - l) / (u - l);
        if frac.is_nan() || frac < 0.0 || frac > 1.0 {
            0.5
        } else {
            frac
        }
    }

    /// TiDB `Histogram.LessRowCount`.
    pub fn less_row_count(&self, kind: HistValueKind, value: &Datum) -> f64 {
        let Some(value) = datum_scalar(kind, value) else {
            return 0.0;
        };
        if self.buckets.is_empty() {
            return 0.0;
        }
        let notnull = self.not_null_count();
        let mut exceeded = true;
        let mut idx = self.buckets.len() - 1;
        for (i, bucket) in self.buckets.iter().enumerate() {
            let Some(upper) = bound_scalar(kind, &bucket.upper) else {
                continue;
            };
            if upper.cmp(&value) != std::cmp::Ordering::Less {
                exceeded = false;
                idx = i;
                break;
            }
        }
        if exceeded {
            return notnull;
        }
        let pre_count = if idx == 0 {
            0.0
        } else {
            self.buckets[idx - 1].count as f64
        };
        let Some((lower, upper)) = self.bucket_scalars(idx, kind) else {
            return pre_count;
        };
        if lower.cmp(&value) == std::cmp::Ordering::Greater {
            return pre_count;
        }
        let cur_count = self.buckets[idx].count as f64;
        let cur_repeats = self.buckets[idx].repeats as f64;
        if upper.cmp(&value) == std::cmp::Ordering::Equal {
            return cur_count - cur_repeats;
        }
        pre_count
            + Self::calc_fraction(&lower, &upper, &value) * (cur_count - cur_repeats - pre_count)
    }

    /// TiDB `Histogram.EqualRowCount` (bucket repeats / bucket NDV).
    pub fn equal_row_count(&self, kind: HistValueKind, value: &Datum) -> Option<f64> {
        let value = datum_scalar(kind, value)?;
        if self.buckets.is_empty() {
            return None;
        }
        let (in_bucket, idx, matched) = self.locate(kind, &value);
        if !in_bucket {
            return None;
        }
        let bucket = &self.buckets[idx];
        if matched {
            return Some(bucket.repeats as f64);
        }
        let prev = if idx == 0 {
            0.0
        } else {
            self.buckets[idx - 1].count as f64
        };
        let bucket_count = bucket.count as f64 - prev;
        if bucket.ndv > 1 {
            return Some((bucket_count - bucket.repeats as f64) / (bucket.ndv as f64 - 1.0));
        }
        // v1 histogram: buckets carry no NDV. TiDB estimates a single value as
        // `realtimeRowCount / Histogram.NDV` (`equalRowCountOnColumn`), not the
        // whole bucket row count.
        if self.ndv > 0 {
            return Some(self.not_null_count() / self.ndv as f64);
        }
        Some(bucket_count)
    }

    /// Rows the histogram attributes to exactly `value`.
    ///
    /// TiDB records a point frequency (`Repeat`) only for the last value of a
    /// bucket; every other value is interpolated through the bucket NDV. A
    /// sampled histogram whose buckets carry no NDV therefore credits no rows
    /// to a value that is not a recorded bucket boundary — the same shape as
    /// TiDB's `equalRowCountOnColumn` falling back to
    /// `estimateRowCountWithUniformDistribution`, which is bounded by the
    /// non-NULL rows the histogram already covers.
    pub fn boundary_repeats(&self, kind: HistValueKind, value: &Datum) -> f64 {
        let Some(value) = datum_scalar(kind, value) else {
            return 0.0;
        };
        let (in_bucket, idx, matched) = self.locate(kind, &value);
        if !in_bucket || !matched {
            return 0.0;
        }
        self.buckets[idx].repeats as f64
    }

    /// TiDB `Histogram.BetweenRowCount` for `[a, b)`.
    pub fn between_row_count(&self, kind: HistValueKind, low: &Datum, high: &Datum) -> f64 {
        let less_low = self.less_row_count(kind, low);
        let less_high = self.less_row_count(kind, high);
        let mut range_est = less_high - less_low;
        let notnull = self.not_null_count();
        let ndv = self.ndv;
        let low_equal = self.equal_row_count(kind, low).unwrap_or(0.0);
        let ndv_avg = if ndv > 0 { notnull / ndv as f64 } else { 0.0 };
        if range_est < low_equal.max(ndv_avg) && ndv > 0 {
            range_est = less_high.min(notnull - less_low).min(low_equal + ndv_avg);
        }
        range_est
    }
}

impl IndexStats {
    /// TiDB `TopN.QueryTopN`: rows recorded for an exact encoded key.
    fn topn_lookup(&self, key: &[u8]) -> Option<f64> {
        self.topn
            .iter()
            .find(|(encoded, _)| encoded.as_slice() == key)
            .map(|(_, count)| *count as f64)
    }

    /// TiDB `TopN.BetweenCount` over encoded keys: rows whose key is in
    /// `[low, high)`.
    fn topn_between(&self, low: Option<&[u8]>, high: Option<&[u8]>) -> f64 {
        self.topn
            .iter()
            .filter(|(key, _)| {
                low.is_none_or(|low| key.as_slice() >= low)
                    && high.is_none_or(|high| key.as_slice() < high)
            })
            .map(|(_, count)| *count as f64)
            .sum()
    }

    /// TiDB `Index.Histogram.LessRowCount` over encoded index keys.
    pub fn less_row_count(&self, value: &[u8]) -> f64 {
        let Some(hist) = &self.histogram else {
            return 0.0;
        };
        let notnull = hist.not_null_count();
        if hist.buckets.is_empty() {
            return 0.0;
        }
        let mut exceeded = true;
        let mut idx = hist.buckets.len() - 1;
        for (i, bucket) in hist.buckets.iter().enumerate() {
            if bucket.upper.as_slice() >= value {
                exceeded = false;
                idx = i;
                break;
            }
        }
        if exceeded {
            return notnull;
        }
        let pre_count = if idx == 0 {
            0.0
        } else {
            hist.buckets[idx - 1].count as f64
        };
        let bucket = &hist.buckets[idx];
        if bucket.lower.as_slice() > value {
            return pre_count;
        }
        let cur_count = bucket.count as f64;
        let cur_repeats = bucket.repeats as f64;
        if bucket.upper.as_slice() == value {
            return cur_count - cur_repeats;
        }
        let prefix = common_prefix(&bucket.lower, &bucket.upper);
        let (l, u, v) = (
            base256(&bucket.lower[prefix.min(bucket.lower.len())..]),
            base256(&bucket.upper[prefix.min(bucket.upper.len())..]),
            base256(&value[prefix.min(value.len())..]),
        );
        let frac = if u <= l {
            0.5
        } else if v <= l {
            0.0
        } else if v >= u {
            1.0
        } else {
            (v - l) / (u - l)
        };
        pre_count + frac * (cur_count - cur_repeats - pre_count)
    }

    /// TiDB `Index.Histogram.BetweenRowCount` for `[low, high)` on encoded keys.
    pub fn between_row_count(&self, low: &[u8], high: &[u8]) -> f64 {
        let less_low = self.less_row_count(low);
        let less_high = self.less_row_count(high);
        let notnull = self
            .histogram
            .as_ref()
            .map(|h| h.not_null_count())
            .unwrap_or(0.0);
        let ndv = self.histogram.as_ref().map(|h| h.ndv).unwrap_or(0);
        let low_equal = self.equal_row_count(low);
        let ndv_avg = if ndv > 0 { notnull / ndv as f64 } else { 0.0 };
        let mut range_est = less_high - less_low;
        if range_est < low_equal.max(ndv_avg) && ndv > 0 {
            range_est = less_high.min(notnull - less_low).min(low_equal + ndv_avg);
        }
        range_est
    }

    /// TiDB `equalRowCountOnIndex` for a point range.
    ///
    /// Stats version 2 only trusts a repeated bucket boundary (TopN is queried
    /// first); every other value — including one the histogram does not cover —
    /// falls back to `estimateRowCountWithUniformDistribution`, i.e.
    /// `NotNullCount / (NDV - |TopN|)`. A value inside a sampled histogram
    /// bucket lands exactly on that average (`3554536 / 3373199 = 1.05`).
    pub fn point_row_count(&self, value: &[u8]) -> f64 {
        let Some(hist) = &self.histogram else {
            return 0.0;
        };
        if self.stats_ver >= 2 {
            if let Some(bucket) = hist
                .buckets
                .iter()
                .find(|bucket| bucket.upper.as_slice() == value)
            {
                return bucket.repeats as f64;
            }
            let ndv = hist.ndv as f64 - self.topn.len() as f64;
            if ndv <= 0.0 {
                return 0.0;
            }
            return hist.not_null_count() / ndv;
        }
        self.equal_row_count(value)
    }

    fn equal_row_count(&self, value: &[u8]) -> f64 {
        let Some(hist) = &self.histogram else {
            return 0.0;
        };
        for (i, bucket) in hist.buckets.iter().enumerate() {
            if bucket.upper.as_slice() < value {
                continue;
            }
            if bucket.lower.as_slice() > value {
                return 0.0;
            }
            if bucket.upper.as_slice() == value {
                return bucket.repeats as f64;
            }
            let prev = if i == 0 {
                0.0
            } else {
                hist.buckets[i - 1].count as f64
            };
            if bucket.ndv > 1 {
                return (bucket.count as f64 - prev - bucket.repeats as f64)
                    / (bucket.ndv as f64 - 1.0);
            }
            return bucket.count as f64 - prev;
        }
        0.0
    }
}

impl ColumnStats {
    /// TiDB `GetColumnRowCount` for one range (stats version 2).
    pub fn range_row_count(
        &self,
        kind: HistValueKind,
        range: &StatsRange,
        realtime: f64,
    ) -> Option<f64> {
        let hist = self.histogram.as_ref()?;
        if hist.buckets.is_empty() {
            // TopN-only statistics (`BuildHistAndTopN` skips bucket building
            // when the TopN holds every distinct value): the range estimate is
            // `TopN.BetweenCount`, clamped like TiDB
            // (`GetColumnRowCount` keeps at least one row).
            let count = self.topn_range_row_count(
                range.low.as_ref().map(|(d, inc)| (d, *inc)),
                range.high.as_ref().map(|(d, inc)| (d, *inc)),
            );
            return Some(count.clamp(1.0, realtime.max(1.0)));
        }
        let notnull = hist.not_null_count();
        let equal = |value: &Datum| -> f64 {
            if let Some(count) = self.eq_from_topn(value) {
                return count;
            }
            hist.equal_row_count(kind, value).unwrap_or(0.0)
        };
        let mut count: f64 = match (&range.low, &range.high) {
            (None, None) => notnull,
            (None, Some((high, high_inc))) => {
                let mut count = hist.less_row_count(kind, high);
                if *high_inc {
                    count += equal(high);
                }
                count
            }
            (Some((low, low_inc)), None) => {
                let mut count = notnull - hist.less_row_count(kind, low);
                if !*low_inc {
                    count -= equal(low);
                }
                count
            }
            (Some((low, low_inc)), Some((high, high_inc))) => {
                if low.compare(high) == std::cmp::Ordering::Equal && *low_inc && *high_inc {
                    equal(low)
                } else {
                    let mut count = hist.between_row_count(kind, low, high);
                    if !*low_inc {
                        count -= equal(low);
                    }
                    if *high_inc {
                        count += equal(high);
                    }
                    count
                }
            }
        };
        count = count.clamp(0.0, notnull + self.null_count as f64);
        let total = notnull + self.null_count as f64;
        if total > 0.0 && realtime > 0.0 {
            count *= realtime / total;
        }
        Some(count.clamp(0.0, realtime.max(count)))
    }

    fn eq_from_topn(&self, value: &Datum) -> Option<f64> {
        let topn = self.topn.as_ref()?;
        let key = crate::table::datum_key(value);
        topn.lookup(&key).map(|c| c as f64)
    }
}

impl Table {
    /// Rows of `column` that survive a null-rejecting predicate, i.e.
    /// `count - null_count`. `None` when the column has no histogram.
    pub fn column_not_null_count(&self, column: &str) -> Option<f64> {
        let col = self.column(column)?;
        if col.histogram.is_none() {
            return None;
        }
        Some(self.count.saturating_sub(col.null_count) as f64)
    }

    /// TiDB `Column.NDV`: the column's distinct count. ANALYZE (`distinct.len()`)
    /// and the statistics JSON already carry the total, so TopN entries are not
    /// added on top; only a histogram without a column NDV falls back to its
    /// last bucket plus the TopN rows it holds out.
    pub fn column_ndv(&self, column: &str) -> Option<f64> {
        let col = self.column(column)?;
        if col.ndv > 0 {
            return Some(col.ndv as f64);
        }
        let topn = col
            .topn
            .as_ref()
            .map(|t| t.items.len() as f64)
            .unwrap_or(0.0);
        let hist = col.histogram.as_ref()?;
        if hist.buckets.is_empty() {
            return None;
        }
        let hist_ndv = hist.buckets.last().map(|b| b.ndv as f64).unwrap_or(0.0);
        Some((hist_ndv + topn).max(1.0))
    }

    /// `col <> value` cardinality (TiDB `GetRowCountByColumnRanges` over the two
    /// disjoint ranges `(-inf,value)` / `(value,+inf)`).
    ///
    /// Both ranges touch every non-NULL row, so the excluded point only costs
    /// the rows the histogram can pin on it: the bucket `Repeat` when `value`
    /// is a recorded boundary, otherwise nothing (see
    /// [`Histogram::boundary_repeats`]).
    pub fn column_ne_estimate(
        &self,
        column: &str,
        kind: HistValueKind,
        value: &Datum,
    ) -> Option<f64> {
        let col = self.column(column)?;
        let hist = col.histogram.as_ref()?;
        if hist.buckets.is_empty() {
            return None;
        }
        let notnull = self.column_not_null_count(column)?;
        Some((notnull - hist.boundary_repeats(kind, value)).max(0.0))
    }

    /// Column range estimate for a list of ranges, or `None` without stats.
    pub fn column_range_estimate(
        &self,
        column: &str,
        kind: HistValueKind,
        ranges: &[StatsRange],
    ) -> Option<f64> {
        let col = self.column(column)?;
        if col.histogram.is_none() {
            return None;
        }
        let realtime = self.count as f64;
        let mut total = 0.0;
        for range in ranges {
            total += col.range_row_count(kind, range, realtime)?;
        }
        Some(total.clamp(1.0, realtime))
    }

    /// Index range estimate using the index histogram (TiDB `GetRowCountByIndexRanges`).
    pub fn index_range_estimate(
        &self,
        index: &str,
        field_type: &FieldType,
        ranges: &[StatsRange],
    ) -> Option<f64> {
        let idx = self.index(index)?;
        let hist = idx.histogram.as_ref()?;
        if hist.buckets.is_empty() {
            return None;
        }
        let realtime = self.count as f64;
        let total = idx.total_row_count();
        let increase = if total > 0.0 { realtime / total } else { 1.0 };
        let notnull = hist.not_null_count();
        let mut count = 0.0;
        for range in ranges {
            let counted = match (&range.low, &range.high) {
                // A full scan reads the histogram plus every TopN row.
                (None, None) => notnull + idx.topn_between(None, None),
                // Point query: TiDB `Index.equalRowCount`, not the `BetweenRowCount` delta.
                (Some((low_val, true)), Some((high_val, true)))
                    if low_val.compare(high_val) == std::cmp::Ordering::Equal =>
                {
                    let key = encode_index_key(field_type, low_val)?;
                    match idx.topn_lookup(&key) {
                        Some(count) => count,
                        // TiDB `equalRowCountOnIndex` for stats version 1 reads
                        // the CM sketch before the histogram: a value the sketch
                        // puts at zero estimates no rows even inside a bucket.
                        None => match &idx.cm_sketch {
                            Some(sketch) if idx.stats_ver < 2 => sketch.query_bytes(&key) as f64,
                            _ => idx.point_row_count(&key),
                        },
                    }
                }
                (low, high) => {
                    // TiDB turns the range into `[low, high)` on encoded keys:
                    // an exclusive low starts after the value (`PrefixNext`),
                    // an inclusive high ends after it.
                    let start_key = match low {
                        None => None,
                        Some((value, inc)) => {
                            let key = encode_index_key(field_type, value)?;
                            Some(if *inc { key } else { prefix_next(&key) })
                        }
                    };
                    let end_key = match high {
                        None => None,
                        Some((value, inc)) => {
                            let key = encode_index_key(field_type, value)?;
                            Some(if *inc { prefix_next(&key) } else { key })
                        }
                    };
                    let start = match &start_key {
                        None => 0.0,
                        Some(key) => idx.less_row_count(key),
                    };
                    let end = match &end_key {
                        // An unbounded upper end covers every histogram value.
                        None => notnull,
                        Some(key) => idx.less_row_count(key),
                    };
                    // TiDB `betweenRowCountOnIndex` adds the TopN rows inside
                    // the encoded key range on top of the histogram count.
                    (end - start).max(0.0)
                        + idx.topn_between(start_key.as_deref(), end_key.as_deref())
                }
            };
            count += counted;
        }
        // TiDB `GetRowCountByIndexRanges` caps the estimate at the realtime row
        // count but never lifts a zero: an equality that falls in a gap between
        // two histogram buckets estimates no rows.
        Some((count * increase).min(realtime))
    }

    pub fn index(&self, name: &str) -> Option<&IndexStats> {
        self.indices
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, s)| s)
    }
}

/// Minimal `codec.EncodeKey` subset used to compare against index histogram
/// bounds. Returns `None` for values SealDB cannot encode yet, so callers fall
/// back to the column histogram.
pub fn encode_index_key(ft: &FieldType, value: &Datum) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    match value {
        Datum::Null => out.push(0x00),
        Datum::I64(v) => {
            out.push(0x03);
            out.extend_from_slice(&((*v as u64) ^ 0x8000_0000_0000_0000).to_be_bytes());
        }
        Datum::Year(v) => {
            out.push(0x04);
            out.extend_from_slice(&(*v as u64).to_be_bytes());
        }
        Datum::U64(v) => {
            out.push(0x04);
            out.extend_from_slice(&v.to_be_bytes());
        }
        Datum::F64(v) => {
            out.push(0x05);
            let bits = v.to_bits();
            let flipped = if v.is_sign_negative() {
                !bits
            } else {
                bits ^ 0x8000_0000_0000_0000
            };
            out.extend_from_slice(&flipped.to_be_bytes());
        }
        Datum::Time(t) => {
            out.push(0x04);
            out.extend_from_slice(&t.to_packed_u64().to_be_bytes());
        }
        Datum::Decimal(d) => {
            // `Flen` carries the decimal precision, `Decimal` the scale.
            let scale = if ft.decimal > 0 {
                ft.decimal as usize
            } else {
                0
            };
            let digits = int_digits(d) + scale;
            let precision = if ft.flen > 0 {
                (ft.flen as usize).max(digits)
            } else {
                digits.max(10)
            };
            let payload = d.encode_bin(precision, scale).ok()?;
            out.push(0x06);
            out.push(precision as u8);
            out.push(scale as u8);
            out.extend_from_slice(&payload);
        }
        Datum::Bytes(b) | Datum::Bit(b) => {
            out.push(0x01);
            out.extend_from_slice(b);
        }
        Datum::Enum(e) => {
            out.push(0x03);
            out.extend_from_slice(&(e.number ^ 0x8000_0000_0000_0000).to_be_bytes());
        }
    }
    Some(out)
}

/// Significant digits of a decimal's integer part (`-0.25` → 1, `4829` → 4).
fn int_digits(d: &sealdb_types::MyDecimal) -> usize {
    let s = d.to_string();
    let s = s.strip_prefix('-').unwrap_or(&s);
    let int = s.split('.').next().unwrap_or("0").trim_start_matches('0');
    int.len().max(1)
}

/// TiDB `kv.Key.PrefixNext`.
pub fn prefix_next(key: &[u8]) -> Vec<u8> {
    let mut out = key.to_vec();
    while let Some(last) = out.last_mut() {
        if *last < 0xff {
            *last += 1;
            return out;
        }
        out.pop();
    }
    vec![0xff]
}
