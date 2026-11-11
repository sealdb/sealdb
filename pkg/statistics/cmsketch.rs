//! TiDB `statistics.CMSketch` subset — the count-min sketch stored in a
//! stats-version-1 index (`cm_sketch.rows` in the stats JSON).
//!
//! TiDB estimates a version-1 point equality from the sketch rather than from
//! the histogram (`equalRowCountOnIndex`), so a value that both the frequency
//! and the noise floor put at zero estimates `0.00` rows — the histogram bucket
//! it happens to fall into is ignored.
use crate::table::datum_key;
use sealdb_types::Datum;

/// TiDB `CMSketch`: `depth` counter rows of `width` cells, plus the sketch's
/// total count and noise `default_value` (sampled stats only).
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct CmSketch {
    pub depth: usize,
    pub width: usize,
    pub count: u64,
    pub default_value: u64,
    rows: Vec<Vec<u32>>,
}

impl CmSketch {
    /// Build from the JSON `rows[].counters` arrays. TiDB recomputes `count`
    /// from the last row (`DecodeCMSketch` overwrites it per row).
    pub fn from_rows(rows: Vec<Vec<u32>>, default_value: u64) -> Option<Self> {
        let depth = rows.len();
        let width = rows.first()?.len();
        if depth == 0 || width == 0 {
            return None;
        }
        let count = rows
            .last()
            .map(|row| row.iter().map(|c| u64::from(*c)).sum())?;
        Some(Self {
            depth,
            width,
            count,
            default_value,
            rows,
        })
    }

    /// TiDB `CMSketch.QueryBytes`: hash the encoded key, drop the per-row noise
    /// floor, then take the median of the remaining counters.
    pub fn query_bytes(&self, key: &[u8]) -> u64 {
        let (h1, h2) = murmur3_sum128(key);
        self.query_hash_value(h1, h2)
    }

    fn query_hash_value(&self, h1: u64, h2: u64) -> u64 {
        let width = self.width as u64;
        // `temp` distinguishes "zero before noise removal" from "zero after".
        let temp: u32 = 1;
        let mut vals = Vec::with_capacity(self.depth);
        let mut min_value = u32::MAX;
        for (i, row) in self.rows.iter().enumerate() {
            let j = ((h1.wrapping_add(h2.wrapping_mul(i as u64))) % width) as usize;
            let cell = row[j];
            min_value = min_value.min(cell);
            let noise = self.count.saturating_sub(u64::from(cell)) / width.saturating_sub(1).max(1);
            if cell == 0 {
                vals.push(0);
            } else if u64::from(cell) < noise {
                vals.push(temp);
            } else {
                vals.push(cell - noise as u32 + temp);
            }
        }
        vals.sort_unstable();
        let low = vals[(self.depth - 1) / 2];
        let high = vals[self.depth / 2];
        let mut res = low + (high - low) / 2;
        if res > min_value + temp {
            res = min_value + temp;
        }
        if res == 0 {
            return 0;
        }
        res -= temp;
        if self.consider_default_value(u64::from(res)) {
            return self.default_value;
        }
        u64::from(res)
    }

    /// TiDB `CMSketch.considerDefVal`: sampled stats replace a suspiciously low
    /// estimate with the sample's default value.
    fn consider_default_value(&self, count: u64) -> bool {
        if self.default_value > 0 {
            return count > 0 && count <= self.default_value / 2;
        }
        false
    }
}

impl CmSketch {
    /// The key TiDB hashes for a point equality on a single-column index.
    pub fn query_datum(&self, field_type: &sealdb_types::FieldType, value: &Datum) -> u64 {
        match crate::range_est::encode_index_key(field_type, value) {
            Some(key) => self.query_bytes(&key),
            None => self.query_bytes(datum_key(value).as_bytes()),
        }
    }
}

const C1: u64 = 0x87c3_7b91_1142_53d5;
const C2: u64 = 0x4cf5_ad43_2745_937f;

fn rotl(x: u64, r: u32) -> u64 {
    x.rotate_left(r)
}

fn fmix64(mut k: u64) -> u64 {
    k ^= k >> 33;
    k = k.wrapping_mul(0xff51_afd7_ed55_8ccd);
    k ^= k >> 33;
    k = k.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    k ^= k >> 33;
    k
}

/// `murmur3.Sum128` (x64 128-bit, seed 0) — the hash TiDB seeds its sketch with.
pub fn murmur3_sum128(data: &[u8]) -> (u64, u64) {
    let mut h1: u64 = 0;
    let mut h2: u64 = 0;
    let blocks = data.len() / 16;
    for i in 0..blocks {
        let block = &data[i * 16..i * 16 + 16];
        let mut k1 = u64::from_le_bytes(block[0..8].try_into().expect("k1"));
        let mut k2 = u64::from_le_bytes(block[8..16].try_into().expect("k2"));
        k1 = k1.wrapping_mul(C1);
        k1 = rotl(k1, 31);
        k1 = k1.wrapping_mul(C2);
        h1 ^= k1;
        h1 = rotl(h1, 27);
        h1 = h1.wrapping_add(h2);
        h1 = h1.wrapping_mul(5).wrapping_add(0x52dc_e729);
        k2 = k2.wrapping_mul(C2);
        k2 = rotl(k2, 33);
        k2 = k2.wrapping_mul(C1);
        h2 ^= k2;
        h2 = rotl(h2, 31);
        h2 = h2.wrapping_add(h1);
        h2 = h2.wrapping_mul(5).wrapping_add(0x3849_5ab5);
    }
    let tail = &data[blocks * 16..];
    let mut k1: u64 = 0;
    let mut k2: u64 = 0;
    for (i, byte) in tail.iter().enumerate().take(16) {
        if i < 8 {
            k1 |= u64::from(*byte) << (8 * i);
        } else {
            k2 |= u64::from(*byte) << (8 * (i - 8));
        }
    }
    if tail.len() > 8 {
        k2 = k2.wrapping_mul(C2);
        k2 = rotl(k2, 33);
        k2 = k2.wrapping_mul(C1);
        h2 ^= k2;
    }
    if !tail.is_empty() {
        k1 = k1.wrapping_mul(C1);
        k1 = rotl(k1, 31);
        k1 = k1.wrapping_mul(C2);
        h1 ^= k1;
    }
    let len = data.len() as u64;
    h1 ^= len;
    h2 ^= len;
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    h1 = fmix64(h1);
    h2 = fmix64(h2);
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    (h1, h2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sealdb_types::FieldType;
    use sealdb_types::field_type::TYPE_LONG;

    fn load(path: &str) -> crate::table::Table {
        let raw = std::fs::read_to_string(format!(
            "{}/../../tests/integrationtest/{path}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("stats json");
        crate::json::table_from_tidb_json(&raw).expect("parse").2
    }

    #[test]
    fn test_murmur3_vectors() {
        // Reference `murmur3.Sum128` (x64 128-bit, seed 0) vectors.
        assert_eq!(murmur3_sum128(b""), (0, 0));
        assert_eq!(
            murmur3_sum128(b"hello"),
            (0xcbd8_a7b3_41bd_9b02, 0x5b1e_906a_48ae_1d19)
        );
        assert_eq!(
            murmur3_sum128(b"hello, world"),
            (0x342f_ac62_3a5e_bc8e, 0x4cdc_bc07_9642_414d)
        );
    }

    #[test]
    fn test_tidb_gap_equality_estimates_zero() {
        // `t1.c2 = 1` on the version-1 index histogram: TiDB estimates 0.00
        // because the CM sketch never saw the value.
        let table = load("s/explain_easy_stats_t1.json");
        let idx = table.index("c2").expect("index stats");
        let sketch = idx.cm_sketch.as_ref().expect("cm sketch");
        let key = crate::range_est::encode_index_key(&FieldType::new_longlong(), &Datum::I64(1))
            .expect("key");
        assert_eq!(sketch.query_bytes(&key), 0);
        let _ = TYPE_LONG;
    }
}
