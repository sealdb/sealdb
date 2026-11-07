//! TiDB `KEY` partitioning hash.
//!
//! `PARTITION BY KEY` hashes the partition-key value with MD5 and takes the
//! first 8 digest bytes as a big-endian `u64`; the partition is `hash % n`.
//! Integers hash through their 8-byte big-endian two's-complement form, so a
//! signed and an unsigned column holding the same bit pattern land together.
//! (`9223372036854775807` and `9223372036854775808` both hash to `p2` of 5.)

/// MD5 digest of `bytes`.
pub fn md5(bytes: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let k: [u32; 64] = std::array::from_fn(|i| {
        ((i as f64 + 1.0).sin().abs() * 4_294_967_296.0) as u32
    });

    let mut msg = bytes.to_vec();
    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());

    let (mut a0, mut b0, mut c0, mut d0) =
        (0x6745_2301u32, 0xefcd_ab89u32, 0x98ba_dcfeu32, 0x1032_5476u32);
    for chunk in msg.chunks(64) {
        let m: [u32; 16] = std::array::from_fn(|i| {
            u32::from_le_bytes([chunk[i * 4], chunk[i * 4 + 1], chunk[i * 4 + 2], chunk[i * 4 + 3]])
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
            let sum = a
                .wrapping_add(f)
                .wrapping_add(k[i])
                .wrapping_add(m[g]);
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

/// `KEY` partition index for `value` among `num` partitions.
///
/// Returns `None` for datum kinds TiDB hashes differently (decimals, floats,
/// temporal values); callers fall back to scanning every partition.
pub fn key_partition_index(value: &sealdb_types::Datum, num: usize) -> Option<usize> {
    if num == 0 {
        return None;
    }
    let bytes: Vec<u8> = match value {
        sealdb_types::Datum::I64(v) => (*v as u64).to_be_bytes().to_vec(),
        sealdb_types::Datum::U64(v) => v.to_be_bytes().to_vec(),
        // TiDB feeds `codec.EncodeValue` output to the hash: integers are their
        // 8-byte big-endian form, while bytes carry a `u64` length prefix.
        sealdb_types::Datum::Bytes(v) => {
            let mut buf = Vec::with_capacity(8 + v.len());
            buf.extend_from_slice(&(v.len() as u64).to_be_bytes());
            buf.extend_from_slice(v);
            buf
        }
        _ => return None,
    };
    let digest = md5(&bytes);
    let hash = u64::from_be_bytes(digest[0..8].try_into().ok()?);
    Some((hash % num as u64) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn md5_known_vectors() {
        assert_eq!(hex(&md5(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(&md5(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            hex(&md5(b"The quick brown fox jumps over the lazy dog")),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
    }

    #[test]
    fn unsigned_boundary_keys_share_a_partition() {
        let lo = sealdb_types::Datum::U64(u64::MAX >> 1);
        let hi = sealdb_types::Datum::U64((u64::MAX >> 1) + 1);
        assert_eq!(key_partition_index(&lo, 5), Some(2));
        assert_eq!(key_partition_index(&hi, 5), Some(2));
    }

    #[test]
    fn string_keys_hash_with_a_length_prefix() {
        // TiDB `KEY(col2) PARTITIONS 4` with `col2 IN ('a', 'c')` prunes to p3.
        assert_eq!(
            key_partition_index(&sealdb_types::Datum::Bytes(b"a".to_vec()), 4),
            Some(3)
        );
        assert_eq!(
            key_partition_index(&sealdb_types::Datum::Bytes(b"c".to_vec()), 4),
            Some(3)
        );
    }
}
