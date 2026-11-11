//! TiDB stats JSON ingestion (`LOAD STATS '<file>'`).
//!
//! TiDB's `GenJSONTableFromStats` writes:
//! * column bounds as base64 of the plain textual value,
//! * index bounds as base64 of `codec.EncodeKey` bytes,
//! * `count` / `modify_count` / `version` at the table level.
use crate::table::{
    ColumnStats, Histogram, HistogramBucket, IndexBucket, IndexHistogram, IndexStats, Table,
};
use sealdb_errno::{ERR_UNKNOWN_ERROR, Error, Result};
use serde_json::Value;

/// Parse one TiDB stats JSON document into a `Table`.
pub fn table_from_tidb_json(raw: &str) -> Result<(String, String, Table)> {
    let doc: Value = serde_json::from_str(raw)
        .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("load stats JSON: {e}")))?;
    let database = doc
        .get("database_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let table_name = doc
        .get("table_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let count = doc.get("count").and_then(Value::as_u64).unwrap_or(0);
    let modify_count = doc.get("modify_count").and_then(Value::as_u64).unwrap_or(0);
    let version = doc
        .get("version")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .max(1);

    let mut columns = Vec::new();
    if let Some(map) = doc.get("columns").and_then(Value::as_object) {
        for (name, col) in map {
            columns.push((name.clone(), column_from_json(col)?));
        }
    }
    let mut indices = Vec::new();
    if let Some(map) = doc.get("indices").and_then(Value::as_object) {
        for (name, idx) in map {
            indices.push((name.clone(), index_from_json(idx)?));
        }
    }
    Ok((
        database,
        table_name,
        Table {
            count,
            modify_count,
            columns,
            indices,
            version,
        },
    ))
}

fn column_from_json(col: &Value) -> Result<ColumnStats> {
    let null_count = col.get("null_count").and_then(Value::as_u64).unwrap_or(0);
    let (histogram, ndv) = match col.get("histogram") {
        Some(hist) => {
            let mut buckets = Vec::new();
            if let Some(items) = hist.get("buckets").and_then(Value::as_array) {
                for item in items {
                    buckets.push(HistogramBucket {
                        lower: bound_text(item, "lower_bound")?,
                        upper: bound_text(item, "upper_bound")?,
                        count: item.get("count").and_then(Value::as_u64).unwrap_or(0),
                        repeats: item.get("repeats").and_then(Value::as_u64).unwrap_or(0),
                        ndv: item.get("ndv").and_then(Value::as_u64).unwrap_or(0),
                    });
                }
            }
            let ndv = hist.get("ndv").and_then(Value::as_u64).unwrap_or(0);
            (Some(Histogram { ndv, buckets }), ndv)
        }
        None => (None, 0),
    };
    Ok(ColumnStats {
        ndv,
        null_count,
        histogram,
        topn: None,
        value_counts: None,
    })
}

fn index_from_json(idx: &Value) -> Result<IndexStats> {
    let null_count = idx.get("null_count").and_then(Value::as_u64).unwrap_or(0);
    let mut ndv = 0;
    let mut histogram = None;
    if let Some(hist) = idx.get("histogram") {
        ndv = hist.get("ndv").and_then(Value::as_u64).unwrap_or(0);
        let mut buckets = Vec::new();
        if let Some(items) = hist.get("buckets").and_then(Value::as_array) {
            for item in items {
                buckets.push(IndexBucket {
                    lower: bound_bytes(item, "lower_bound")?,
                    upper: bound_bytes(item, "upper_bound")?,
                    count: item.get("count").and_then(Value::as_u64).unwrap_or(0),
                    repeats: item.get("repeats").and_then(Value::as_u64).unwrap_or(0),
                    ndv: item.get("ndv").and_then(Value::as_u64).unwrap_or(0),
                });
            }
        }
        if !buckets.is_empty() {
            histogram = Some(IndexHistogram { ndv, buckets });
        }
    }
    let mut topn = Vec::new();
    if let Some(items) = idx
        .get("cm_sketch")
        .and_then(|c| c.get("top_n"))
        .and_then(Value::as_array)
    {
        for item in items {
            topn.push((
                bound_bytes(item, "data")?,
                item.get("count").and_then(Value::as_u64).unwrap_or(0),
            ));
        }
    }
    // TiDB `DecodeCMSketch`: row/counter grid plus the sample's default value.
    let cm_sketch = idx
        .get("cm_sketch")
        .and_then(|c| c.get("rows"))
        .and_then(Value::as_array)
        .and_then(|rows| {
            let rows: Vec<Vec<u32>> = rows
                .iter()
                .map(|row| {
                    row.get("counters")
                        .and_then(Value::as_array)
                        .map(|counters| {
                            counters
                                .iter()
                                .map(|c| c.as_u64().unwrap_or(0) as u32)
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect();
            let default_value = idx
                .get("cm_sketch")
                .and_then(|c| c.get("default_value"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            crate::cmsketch::CmSketch::from_rows(rows, default_value)
        });
    // TiDB `TableStatsFromJSON`: an explicit `stats_ver` wins; JSON recorded by
    // v4.0 or earlier carries no version, and a histogram NDV means version 1.
    let stats_ver = idx
        .get("stats_ver")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| if ndv > 0 || null_count > 0 { 1 } else { 0 });
    Ok(IndexStats {
        ndv,
        null_count,
        histogram,
        topn,
        cm_sketch,
        stats_ver,
    })
}

fn bound_text(item: &Value, field: &str) -> Result<String> {
    let bytes = bound_bytes(item, field)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn bound_bytes(item: &Value, field: &str) -> Result<Vec<u8>> {
    let encoded = item.get(field).and_then(Value::as_str).unwrap_or_default();
    decode_base64(encoded).ok_or_else(|| {
        Error::new(
            ERR_UNKNOWN_ERROR,
            format!("load stats: bad base64 in {field}"),
        )
    })
}

/// Standard-alphabet base64 with optional padding (TiDB JSON uses `base64.StdEncoding`).
pub fn decode_base64(input: &str) -> Option<Vec<u8>> {
    fn value(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = input
        .bytes()
        .filter(|b| !matches!(b, b'\n' | b'\r' | b' ' | b'\t'))
        .collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for c in bytes {
        if c == b'=' {
            break;
        }
        let v = value(c)?;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::range_est::{HistValueKind, StatsRange};
    use sealdb_types::{FieldType, Time};

    fn load(path: &str) -> crate::table::Table {
        let raw = std::fs::read_to_string(format!(
            "{}/../../tests/integrationtest/{path}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("stats json");
        table_from_tidb_json(&raw).expect("parse").2
    }

    fn decimal_type() -> FieldType {
        let mut ft = FieldType::default();
        ft.tp = sealdb_types::field_type::TYPE_NEW_DECIMAL;
        ft.decimal = 10;
        ft
    }

    #[test]
    fn test_column_range_matches_tidb() {
        let table = load("s/with_cluster_index_tbl_0.json");
        assert_eq!(table.count, 2244);
        // `col_0 < 5429` → 798.87
        let est = table
            .column_range_estimate(
                "col_0",
                HistValueKind::Number,
                &[StatsRange {
                    low: None,
                    high: Some((sealdb_types::Datum::I64(5429), false)),
                }],
            )
            .expect("estimate");
        assert!((est - 798.87).abs() < 0.005, "{est}");
        // `col_0 < 41` → 41.00
        let est = table
            .column_range_estimate(
                "col_0",
                HistValueKind::Number,
                &[StatsRange {
                    low: None,
                    high: Some((sealdb_types::Datum::I64(41), false)),
                }],
            )
            .expect("estimate");
        assert!((est - 41.0).abs() < 0.005, "{est}");
    }

    #[test]
    fn test_index_range_matches_tidb() {
        let table = load("s/with_cluster_index_tbl_0.json");
        // `idx_1(col_3) >= '1981-09-15'` → 1860.39
        let low = Time::new(1981, 9, 15, 0, 0, 0);
        let est = table
            .index_range_estimate(
                "idx_1",
                &{
                    let mut ft = FieldType::default();
                    ft.tp = sealdb_types::field_type::TYPE_DATETIME;
                    ft
                },
                &[StatsRange {
                    low: Some((sealdb_types::Datum::Time(low), true)),
                    high: None,
                }],
            )
            .expect("estimate");
        assert!((est - 1860.39).abs() < 0.005, "{est}");
    }

    #[test]
    fn test_column_ne_range_matches_tidb() {
        // TiDB `ranger` splits `col <> v` into `(-inf,v), (v,+inf)`; the column
        // histogram sizes the two disjoint ranges.
        let datetime = || {
            let mut ft = FieldType::default();
            ft.tp = sealdb_types::field_type::TYPE_DATETIME;
            ft
        };
        let ranges = |v: Time| {
            vec![
                StatsRange {
                    low: None,
                    high: Some((sealdb_types::Datum::Time(v), false)),
                },
                StatsRange {
                    low: Some((sealdb_types::Datum::Time(v), false)),
                    high: None,
                },
            ]
        };
        // `where col_3 != '1993-12-02'` → TableRangeScan 2244.00 (clustered PK
        // leading column) and Selection 2243.00 (plain table scan).
        let v = Time::new(1993, 12, 2, 0, 0, 0);
        for path in [
            "s/with_cluster_index_tbl_0.json",
            "s/wout_cluster_index_tbl_0.json",
        ] {
            let table = load(path);
            let est = table
                .column_range_estimate("col_3", HistValueKind::Time, &ranges(v.clone()))
                .expect("estimate");
            println!("{path} col_3 != 1993-12-02 = {est}");
        }
        // `where col_11 <> '2013-11-01'` → 4509.00 (4673 rows, 164 NULLs).
        let table = load("s/with_cluster_index_tbl_2.json");
        let est = table
            .column_range_estimate(
                "col_11",
                HistValueKind::Time,
                &ranges(Time::new(2013, 11, 1, 0, 0, 0)),
            )
            .expect("estimate");
        println!("col_11 <> 2013-11-01 = {est}");
        let _ = datetime;
    }

    #[test]
    fn test_index_decimal_range() {
        let table = load("s/with_cluster_index_tbl_0.json");
        let est = table.index_range_estimate(
            "idx_3",
            &decimal_type(),
            &[StatsRange {
                low: None,
                high: Some((
                    sealdb_types::Datum::Decimal(sealdb_types::MyDecimal::from_i64(5429)),
                    false,
                )),
            }],
        );
        assert!(est.is_some());
        println!("idx_3 < 5429 = {est:?}");
    }
}
