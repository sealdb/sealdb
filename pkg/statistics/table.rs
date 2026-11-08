/// TiDB `statistics.Table` / `HistColl` fields for .
/// Histograms + TopN feed selectivity; CMSketch / correlation deferred.
use std::cmp::Ordering;

use sealdb_types::Datum;
use sealdb_types::time::TimeType;

/// One equi-depth histogram bucket (TiDB `Bucket`).
#[derive(Clone, Debug, Default)]
pub struct HistogramBucket {
    pub lower: String,
    pub upper: String,
    /// Cumulative row count through this bucket.
    pub count: u64,
    pub repeats: u64,
    pub ndv: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Histogram {
    /// Column NDV from the statistics JSON (`Histogram.NDV`). v1 histograms
    /// carry no per-bucket NDV, so point estimates fall back to `rows / NDV`.
    pub ndv: u64,
    pub buckets: Vec<HistogramBucket>,
}

impl Histogram {
    /// Build an equi-depth histogram from sorted non-null value strings.
    pub fn from_sorted(values: &[String], max_buckets: usize) -> Self {
        if values.is_empty() || max_buckets == 0 {
            return Self::default();
        }
        let n = values.len();
        let bucket_count = max_buckets.min(n).max(1);
        let mut buckets = Vec::with_capacity(bucket_count);
        let mut start = 0usize;
        for b in 0..bucket_count {
            let end = ((b + 1) * n) / bucket_count;
            let end = end.max(start + 1).min(n);
            let slice = &values[start..end];
            let mut distinct = 0u64;
            let mut prev: Option<&str> = None;
            let mut repeats = 1u64;
            let mut max_repeats = 1u64;
            for v in slice {
                if prev == Some(v.as_str()) {
                    repeats += 1;
                    max_repeats = max_repeats.max(repeats);
                } else {
                    distinct += 1;
                    repeats = 1;
                    prev = Some(v.as_str());
                }
            }
            buckets.push(HistogramBucket {
                lower: slice.first().cloned().unwrap_or_default(),
                upper: slice.last().cloned().unwrap_or_default(),
                count: end as u64,
                repeats: max_repeats,
                ndv: distinct,
            });
            start = end;
            if start >= n {
                break;
            }
        }
        let mut ndv = 0u64;
        for (i, v) in values.iter().enumerate() {
            if i == 0 || values[i - 1] != *v {
                ndv += 1;
            }
        }
        Self { ndv, buckets }
    }

    /// Fraction of rows with value in `[lower, upper]` (inclusive string compare).
    pub fn range_fraction(&self, lower: Option<&str>, upper: Option<&str>) -> f64 {
        if self.buckets.is_empty() {
            return 1.0;
        }
        let total = self.buckets.last().map(|b| b.count).unwrap_or(0) as f64;
        if total == 0.0 {
            return 0.0;
        }
        let mut prev = 0u64;
        let mut matched = 0u64;
        for bucket in &self.buckets {
            let bucket_rows = bucket.count.saturating_sub(prev);
            let overlaps = lower_ok(lower, &bucket.upper) && upper_ok(upper, &bucket.lower);
            if overlaps {
                matched += bucket_rows;
            }
            prev = bucket.count;
        }
        (matched as f64 / total).clamp(0.0, 1.0)
    }
}

fn lower_ok(lower: Option<&str>, bucket_upper: &str) -> bool {
    match lower {
        None => true,
        Some(l) => l.as_bytes() <= bucket_upper.as_bytes(),
    }
}

fn upper_ok(upper: Option<&str>, bucket_lower: &str) -> bool {
    match upper {
        None => true,
        Some(u) => u.as_bytes() >= bucket_lower.as_bytes(),
    }
}

/// Top-N frequent values (TiDB `TopN`).
#[derive(Clone, Debug, Default)]
pub struct TopN {
    pub items: Vec<(String, u64)>,
}

/// One index histogram bucket (TiDB `Bucket` whose bounds are the raw
/// `codec.EncodeKey` bytes stored in TiDB's stats JSON).
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct IndexBucket {
    pub lower: Vec<u8>,
    pub upper: Vec<u8>,
    pub count: u64,
    pub repeats: u64,
    pub ndv: u64,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct IndexHistogram {
    pub ndv: u64,
    pub buckets: Vec<IndexBucket>,
}

impl IndexHistogram {
    /// TiDB `Histogram.NotNullCount`.
    pub fn not_null_count(&self) -> f64 {
        self.buckets.last().map(|b| b.count as f64).unwrap_or(0.0)
    }
}

/// TiDB `statistics.Index`.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct IndexStats {
    pub ndv: u64,
    pub null_count: u64,
    #[serde(default)]
    pub histogram: Option<IndexHistogram>,
    /// TiDB `TopN` entries as `(encoded key, count)`.
    #[serde(default)]
    pub topn: Vec<(Vec<u8>, u64)>,
    /// TiDB `CMSketch`: the point-frequency sketch of a version-1 index.
    #[serde(default)]
    pub cm_sketch: Option<crate::cmsketch::CmSketch>,
    /// TiDB `Statistics.StatsVer`. `LOAD STATS` JSON without the field but with
    /// a histogram NDV was collected by v4.0 or earlier, i.e. version 1, whose
    /// histogram still contains the TopN rows (`TotalRowCount` excludes them
    /// from the sum so the same rows are not counted twice).
    #[serde(default)]
    pub stats_ver: u64,
}

impl IndexStats {
    /// TiDB `Index.TotalRowCount`: version 2 histograms hold TopN rows out, so
    /// they are added back; version 1 histograms already cover them.
    pub fn total_row_count(&self) -> f64 {
        let hist = self
            .histogram
            .as_ref()
            .map(|h| h.not_null_count())
            .unwrap_or(0.0)
            + self.null_count as f64;
        if self.stats_ver >= 2 {
            hist + self.topn.iter().map(|(_, c)| *c as f64).sum::<f64>()
        } else {
            hist
        }
    }
}

impl TopN {
    pub fn from_counts(counts: &[(String, u64)], max_items: usize) -> Self {
        let mut items = counts.to_vec();
        items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        items.truncate(max_items);
        Self { items }
    }

    pub fn lookup(&self, value: &str) -> Option<u64> {
        self.items.iter().find(|(v, _)| v == value).map(|(_, c)| *c)
    }
}

#[derive(Clone, Debug, Default)]
pub struct ColumnStats {
    pub ndv: u64,
    pub null_count: u64,
    pub histogram: Option<Histogram>,
    pub topn: Option<TopN>,
    /// Exact per-value counts, kept for small columns so equality / IN-list /
    /// range selectivity is exact (TiDB leans on CMSketch here). `None` when
    /// the column is too wide to retain.
    pub value_counts: Option<Vec<(String, u64)>>,
}

impl ColumnStats {
    /// Exact rows equal to `value` (integer, string and temporal keys alike —
    /// the store keeps per-value counts keyed by `datum_key`).
    pub fn exact_point_count(&self, value: &Datum) -> Option<u64> {
        let key = datum_key(value);
        let raw = self.value_counts.as_ref()?;
        Some(
            raw.iter()
                .filter(|(k, _)| *k == key)
                .map(|(_, c)| *c)
                .sum(),
        )
    }

    /// Exact rows whose value lies inside `[lower, upper]` with the given
    /// inclusivity. Integer keys compare numerically; datetime keys compare
    /// lexicographically (their canonical form sorts chronologically).
    pub fn exact_range_count(
        &self,
        lower: Option<(&Datum, bool)>,
        upper: Option<(&Datum, bool)>,
    ) -> Option<u64> {
        let raw = self.value_counts.as_ref()?;
        let lo = lower.map(|(d, inc)| (datum_key(d), inc));
        let hi = upper.map(|(d, inc)| (datum_key(d), inc));
        let order = key_order(raw, lo.as_ref(), hi.as_ref());
        let mut total = 0u64;
        for (key, count) in raw {
            if let Some((lo_key, inc)) = lo.as_ref() {
                let ord = compare_keys(order, key, lo_key);
                if ord == Ordering::Less || (ord == Ordering::Equal && !*inc) {
                    continue;
                }
            }
            if let Some((hi_key, inc)) = hi.as_ref() {
                let ord = compare_keys(order, key, hi_key);
                if ord == Ordering::Greater || (ord == Ordering::Equal && !*inc) {
                    continue;
                }
            }
            total += *count;
        }
        Some(total)
    }

    /// Exact rows for a set of point values.
    pub fn exact_points_count(&self, values: &[Datum]) -> Option<u64> {
        let raw = self.value_counts.as_ref()?;
        let mut total = 0u64;
        for value in values {
            let key = datum_key(value);
            total += raw.iter().filter(|(k, _)| *k == key).map(|(_, c)| *c).sum::<u64>();
        }
        Some(total)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyOrder {
    Int,
    Text,
}

fn key_order(
    raw: &[(String, u64)],
    lower: Option<&(String, bool)>,
    upper: Option<&(String, bool)>,
) -> KeyOrder {
    let numeric = raw.iter().all(|(k, _)| k.parse::<i64>().is_ok())
        && lower.is_none_or(|(k, _)| k.parse::<i64>().is_ok())
        && upper.is_none_or(|(k, _)| k.parse::<i64>().is_ok());
    if numeric {
        return KeyOrder::Int;
    }
    // Temporal keys and arbitrary strings both sort by their canonical text.
    KeyOrder::Text
}

fn compare_keys(order: KeyOrder, left: &str, right: &str) -> Ordering {
    match order {
        KeyOrder::Int => left
            .parse::<i64>()
            .map(|l| (l, right.parse::<i64>()))
            .ok()
            .and_then(|(l, r)| r.ok().map(|r| l.cmp(&r)))
            .unwrap_or_else(|| left.as_bytes().cmp(right.as_bytes())),
        KeyOrder::Text => left.as_bytes().cmp(right.as_bytes()),
    }
}

#[derive(Clone, Debug, Default)]
pub struct Table {
    pub count: u64,
    /// Rows changed since last ANALYZE (TiDB `ModifyCount`).
    pub modify_count: u64,
    pub columns: Vec<(String, ColumnStats)>,
    /// Index histograms (TiDB `statistics.Index`), keyed by index name.
    /// `LOAD STATS` fills these from the JSON `indices` map; ANALYZE keeps them
    /// empty because the planner falls back to the leading column's histogram.
    pub indices: Vec<(String, IndexStats)>,
    pub version: u64,
}

impl Table {
    pub const PSEUDO_ROW_COUNT: u64 = 10_000;
    pub const DEFAULT_HIST_BUCKETS: usize = 8;
    pub const DEFAULT_TOPN: usize = 10;
    /// Exact per-value counts are retained up to this many distinct values.
    pub const MAX_VALUE_COUNTS: usize = 10_000;
    /// TiDB `AutoAnalyzeMinCnt` default is 1000; SealDB keeps no floor so
    /// MemStorage tests can exercise auto-analyze on small tables.
    pub const AUTO_ANALYZE_MIN_CNT: u64 = 0;

    /// TiDB `IsEligibleForAnalysis` + modify/count > ratio.
    /// Requires a prior ANALYZE (`version > 0`) so cold tables stay on pseudo-stats
    /// until the first explicit analyze (matches common test setup).
    pub fn needs_auto_analyze(&self, ratio: f64) -> bool {
        if ratio <= 0.0 || self.version == 0 {
            return false;
        }
        let _ = Self::AUTO_ANALYZE_MIN_CNT;
        let base = self.count.max(1) as f64;
        (self.modify_count as f64) / base > ratio
    }

    pub fn column(&self, name: &str) -> Option<&ColumnStats> {
        self.columns
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, s)| s)
    }

    pub fn eq_row_count(&self, column: &str) -> u64 {
        match self.column(column) {
            Some(col) if col.ndv > 0 => self.count.div_ceil(col.ndv),
            _ => self.count,
        }
    }

    /// Equality estimate preferring TopN when the constant is a frequent value.
    pub fn eq_row_count_value(&self, column: &str, value: &Datum) -> u64 {
        let key = datum_key(value);
        if let Some(col) = self.column(column)
            && let Some(topn) = &col.topn
            && let Some(cnt) = topn.lookup(&key)
        {
            return cnt;
        }
        self.eq_row_count(column)
    }

    /// Range estimate via histogram; falls back to half the table.
    pub fn range_row_count(
        &self,
        column: &str,
        lower: Option<&Datum>,
        upper: Option<&Datum>,
    ) -> u64 {
        let Some(col) = self.column(column) else {
            return self.count / 2;
        };
        let Some(hist) = &col.histogram else {
            return self.count / 2;
        };
        let lo = lower.map(datum_key);
        let hi = upper.map(datum_key);
        let frac = hist.range_fraction(lo.as_deref(), hi.as_deref());
        ((self.count as f64) * frac).round() as u64
    }
}

pub fn datum_key(value: &Datum) -> String {
    match value {
        Datum::Null => "NULL".into(),
        Datum::I64(v) => v.to_string(),
        Datum::U64(v) => v.to_string(),
        Datum::F64(v) => v.to_string(),
        Datum::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        Datum::Time(t) => match t.tp {
            TimeType::Date => format!("{:04}-{:02}-{:02}", t.year, t.month, t.day),
            _ => format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:06}",
                t.year, t.month, t.day, t.hour, t.minute, t.second, t.micro
            ),
        },
        other => format!("{other:?}"),
    }
}

#[cfg(test)]
mod histogram_tests {
    use super::*;

    #[test]
    fn test_histogram_equi_depth() {
        let values: Vec<String> = (0..16).map(|i| format!("{i:02}")).collect();
        let hist = Histogram::from_sorted(&values, 4);
        assert_eq!(hist.buckets.len(), 4);
        assert_eq!(hist.buckets.last().unwrap().count, 16);
        let frac = hist.range_fraction(Some("00"), Some("03"));
        assert!(frac > 0.0 && frac <= 1.0);
    }

    #[test]
    fn test_topn_lookup() {
        let topn = TopN::from_counts(&[("a".into(), 5), ("b".into(), 2), ("c".into(), 9)], 2);
        assert_eq!(topn.items.len(), 2);
        assert_eq!(topn.lookup("c"), Some(9));
        assert_eq!(topn.lookup("a"), Some(5));
        assert_eq!(topn.lookup("b"), None);
    }
}

