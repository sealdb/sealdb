//! TiDB `KEY` partitioning hash.
//!
//! `PARTITION BY KEY` feeds every key column through the collation's sort key
//! (plain bytes for binary collations) into a CRC-32/IEEE checksum and takes
//! `crc32 % n` as the partition. `NULL` contributes a single `0x00` byte.
//! Non-string columns hash through their text form, so `9223372036854775807`
//! and `9223372036854775808` both hash to `p2` of 5.

/// MD5 digest of `bytes` (used by the SQL `MD5()` function).
pub fn md5(bytes: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let k: [u32; 64] =
        std::array::from_fn(|i| ((i as f64 + 1.0).sin().abs() * 4_294_967_296.0) as u32);

    let mut msg = bytes.to_vec();
    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());

    let (mut a0, mut b0, mut c0, mut d0) = (
        0x6745_2301u32,
        0xefcd_ab89u32,
        0x98ba_dcfeu32,
        0x1032_5476u32,
    );
    for chunk in msg.chunks(64) {
        let m: [u32; 16] = std::array::from_fn(|i| {
            u32::from_le_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ])
        });
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let tmp = d;
            d = c;
            c = b;
            let sum = a.wrapping_add(f).wrapping_add(k[i]).wrapping_add(m[g]);
            b = b.wrapping_add(sum.rotate_left(S[i]));
            a = tmp;
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut out = [0u8; 16];
    for (i, word) in [a0, b0, c0, d0].iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    out
}

/// CRC-32/IEEE (the polynomial used by `crc32.NewIEEE` in TiDB).
pub fn crc32_ieee(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in bytes {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Hash key bytes TiDB feeds to the CRC-32 checksum for one `KEY` column:
/// the collation sort key of the value's text form, or `0x00` for `NULL`.
fn key_bytes(value: &sealdb_types::Datum, ci: bool) -> Option<Vec<u8>> {
    let text = match value {
        sealdb_types::Datum::Null => return Some(vec![0]),
        sealdb_types::Datum::I64(v) => v.to_string().into_bytes(),
        sealdb_types::Datum::U64(v) => v.to_string().into_bytes(),
        sealdb_types::Datum::Year(v) => v.to_string().into_bytes(),
        sealdb_types::Datum::F64(v) => sealdb_types::shortest_f64(*v).into_bytes(),
        sealdb_types::Datum::Bytes(v) => v.clone(),
        sealdb_types::Datum::Bit(v) => v.clone(),
        sealdb_types::Datum::Decimal(d) => d.to_string().into_bytes(),
        sealdb_types::Datum::Time(t) => t.to_string().into_bytes(),
        sealdb_types::Datum::Enum(e) => e.label.clone(),
    };
    // TiDB runs every value through `collator.Key`; `utf8mb4_bin` (and the
    // other `_bin` collations) return the input bytes unchanged.
    if ci {
        Some(
            super::partition::ci_sort_key(&String::from_utf8_lossy(&text))
                .into_iter()
                .collect::<String>()
                .into_bytes(),
        )
    } else {
        Some(text)
    }
}

/// `KEY` partition index for `value` among `num` partitions.
///
/// Returns `None` for datum kinds TiDB hashes differently; callers fall back to
/// scanning every partition.
pub fn key_partition_index(value: &sealdb_types::Datum, num: usize, ci: bool) -> Option<usize> {
    if num == 0 {
        return None;
    }
    let bytes = key_bytes(value, ci)?;
    Some((crc32_ieee(&bytes) as usize) % num)
}

/// Multi-column `PARTITION BY KEY` hash over the concatenated column keys.
pub fn key_partition_index_multi(
    values: &[sealdb_types::Datum],
    num: usize,
    ci: bool,
) -> Option<usize> {
    if num == 0 {
        return None;
    }
    let mut bytes = Vec::new();
    for v in values {
        bytes.extend(key_bytes(v, ci)?);
    }
    Some((crc32_ieee(&bytes) as usize) % num)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_known_vectors() {
        let hex = |bytes: &[u8]| -> String { bytes.iter().map(|b| format!("{b:02x}")).collect() };
        assert_eq!(hex(&md5(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(&md5(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            hex(&md5(b"The quick brown fox jumps over the lazy dog")),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
    }

    #[test]
    fn crc32_known_vectors() {
        assert_eq!(crc32_ieee(b""), 0);
        assert_eq!(crc32_ieee(b"abc"), 0x3524_41c2);
        assert_eq!(
            crc32_ieee(b"The quick brown fox jumps over the lazy dog"),
            0x414f_a339
        );
    }

    #[test]
    fn unsigned_boundary_keys_share_a_partition() {
        let lo = sealdb_types::Datum::U64(u64::MAX >> 1);
        let hi = sealdb_types::Datum::U64((u64::MAX >> 1) + 1);
        assert_eq!(key_partition_index(&lo, 5, false), Some(2));
        assert_eq!(key_partition_index(&hi, 5, false), Some(2));
    }

    #[test]
    fn string_keys_hash_through_text_form() {
        // TiDB `KEY(col2) PARTITIONS 4` with `col2 IN ('a', 'c')` prunes to p3.
        assert_eq!(
            key_partition_index(&sealdb_types::Datum::Bytes(b"a".to_vec()), 4, false),
            Some(3)
        );
        assert_eq!(
            key_partition_index(&sealdb_types::Datum::Bytes(b"c".to_vec()), 4, false),
            Some(3)
        );
    }

    #[test]
    fn multi_column_key_hashes_concatenated_text() {
        // TiDB `KEY(signed, fname) PARTITIONS 8` over 30 dates 3 days apart.
        fn date_at(step: usize) -> sealdb_types::Time {
            const LEN: [u8; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
            let mut month = 3usize;
            let mut day = 30u32 + 3 * step as u32;
            while day > LEN[month - 1] as u32 {
                day -= LEN[month - 1] as u32;
                month += 1;
            }
            let mut t = sealdb_types::Time::new(2023, month as u8, day as u8, 0, 0, 0);
            t.tp = sealdb_types::TimeType::Date;
            t
        }
        for (i, want) in [4, 5, 3, 3, 5, 4, 3, 3].into_iter().enumerate() {
            let count = (0..30)
                .filter(|k| {
                    key_partition_index_multi(
                        &[
                            sealdb_types::Datum::Time(date_at(*k)),
                            sealdb_types::Datum::Bytes(b"Joe".to_vec()),
                        ],
                        8,
                        false,
                    ) == Some(i)
                })
                .count();
            assert_eq!(count, want, "partition p{i}");
        }
    }
}
