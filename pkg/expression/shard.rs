//! `tidb_shard(x)`: TiDB's shard-index bucket hash.
//!
//! TiDB evaluates the function as `vitess.HashUint64(uint64(x)) % 256`
//! (`builtinTidbShardSig.evalInt`, `pkg/expression/builtin_miscellaneous.go`),
//! and `vitess.HashUint64` is one DES block encryption of the big-endian
//! `uint64` under the all-zero key (`pkg/util/vitess/vitess_hash.go`).
//!
//! With an all-zero key every round subkey is zero, so the cipher reduces to
//! the initial permutation, sixteen Feistel rounds of `S(E(R))` and `P`, and
//! the final permutation. The bucket is the last byte of the ciphertext:
//! `tidb_shard(100) = 8`, `tidb_shard(300) = 227`.
//!
//! Shard indexes rely on this exactly: `UNIQUE KEY uk((tidb_shard(a)), a)`
//! only answers `a = 100` through a point get when the planner can fold
//! `tidb_shard(100)` to the same key the DDL stored.

/// DES initial permutation (1-based bit positions).
const IP: [u8; 64] = [
    58, 50, 42, 34, 26, 18, 10, 2, 60, 52, 44, 36, 28, 20, 12, 4, 62, 54, 46, 38, 30, 22, 14, 6,
    64, 56, 48, 40, 32, 24, 16, 8, 57, 49, 41, 33, 25, 17, 9, 1, 59, 51, 43, 35, 27, 19, 11, 3,
    61, 53, 45, 37, 29, 21, 13, 5, 63, 55, 47, 39, 31, 23, 15, 7,
];

/// DES final permutation, the inverse of [`IP`].
const FP: [u8; 64] = [
    40, 8, 48, 16, 56, 24, 64, 32, 39, 7, 47, 15, 55, 23, 63, 31, 38, 6, 46, 14, 54, 22, 62, 30,
    37, 5, 45, 13, 53, 21, 61, 29, 36, 4, 44, 12, 52, 20, 60, 28, 35, 3, 43, 11, 51, 19, 59, 27,
    34, 2, 42, 10, 50, 18, 58, 26, 33, 1, 41, 9, 49, 17, 57, 25,
];

/// DES round expansion: 32 bits to 48.
const E: [u8; 48] = [
    32, 1, 2, 3, 4, 5, 4, 5, 6, 7, 8, 9, 8, 9, 10, 11, 12, 13, 12, 13, 14, 15, 16, 17, 16, 17, 18,
    19, 20, 21, 20, 21, 22, 23, 24, 25, 24, 25, 26, 27, 28, 29, 28, 29, 30, 31, 32, 1,
];

/// DES round permutation of the S-box output.
const P: [u8; 32] = [
    16, 7, 20, 21, 29, 12, 28, 17, 1, 15, 23, 26, 5, 18, 31, 10, 2, 8, 24, 14, 32, 27, 3, 9, 19,
    13, 30, 6, 22, 11, 4, 25,
];

/// The eight DES S-boxes, each 4 rows × 16 columns.
const S: [[u8; 64]; 8] = [
    [
        14, 4, 13, 1, 2, 15, 11, 8, 3, 10, 6, 12, 5, 9, 0, 7, 0, 15, 7, 4, 14, 2, 13, 1, 10, 6, 12,
        11, 9, 5, 3, 8, 4, 1, 14, 8, 13, 6, 2, 11, 15, 12, 9, 7, 3, 10, 5, 0, 15, 12, 8, 2, 4, 9,
        1, 7, 5, 11, 3, 14, 10, 0, 6, 13,
    ],
    [
        15, 1, 8, 14, 6, 11, 3, 4, 9, 7, 2, 13, 12, 0, 5, 10, 3, 13, 4, 7, 15, 2, 8, 14, 12, 0, 1,
        10, 6, 9, 11, 5, 0, 14, 7, 11, 10, 4, 13, 1, 5, 8, 12, 6, 9, 3, 2, 15, 13, 8, 10, 1, 3, 15,
        4, 2, 11, 6, 7, 12, 0, 5, 14, 9,
    ],
    [
        10, 0, 9, 14, 6, 3, 15, 5, 1, 13, 12, 7, 11, 4, 2, 8, 13, 7, 0, 9, 3, 4, 6, 10, 2, 8, 5, 14,
        12, 11, 15, 1, 13, 6, 4, 9, 8, 15, 3, 0, 11, 1, 2, 12, 5, 10, 14, 7, 1, 10, 13, 0, 6, 9, 8,
        7, 4, 15, 14, 3, 11, 5, 2, 12,
    ],
    [
        7, 13, 14, 3, 0, 6, 9, 10, 1, 2, 8, 5, 11, 12, 4, 15, 13, 8, 11, 5, 6, 15, 0, 3, 4, 7, 2,
        12, 1, 10, 14, 9, 10, 6, 9, 0, 12, 11, 7, 13, 15, 1, 3, 14, 5, 2, 8, 4, 3, 15, 0, 6, 10, 1,
        13, 8, 9, 4, 5, 11, 12, 7, 2, 14,
    ],
    [
        2, 12, 4, 1, 7, 10, 11, 6, 8, 5, 3, 15, 13, 0, 14, 9, 14, 11, 2, 12, 4, 7, 13, 1, 5, 0, 15,
        10, 3, 9, 8, 6, 4, 2, 1, 11, 10, 13, 7, 8, 15, 9, 12, 5, 6, 3, 0, 14, 11, 8, 12, 7, 1, 14,
        2, 13, 6, 15, 0, 9, 10, 4, 5, 3,
    ],
    [
        12, 1, 10, 15, 9, 2, 6, 8, 0, 13, 3, 4, 14, 7, 5, 11, 10, 15, 4, 2, 7, 12, 9, 5, 6, 1, 13,
        14, 0, 11, 3, 8, 9, 14, 15, 5, 2, 8, 12, 3, 7, 0, 4, 10, 1, 13, 11, 6, 4, 3, 2, 12, 9, 5,
        15, 10, 11, 14, 1, 7, 6, 0, 8, 13,
    ],
    [
        4, 11, 2, 14, 15, 0, 8, 13, 3, 12, 9, 7, 5, 10, 6, 1, 13, 0, 11, 7, 4, 9, 1, 10, 14, 3, 5,
        12, 2, 15, 8, 6, 1, 4, 11, 13, 12, 3, 7, 14, 10, 15, 6, 8, 0, 5, 9, 2, 6, 11, 13, 8, 1, 4,
        10, 7, 9, 5, 0, 15, 14, 2, 3, 12,
    ],
    [
        13, 2, 8, 4, 6, 15, 11, 1, 10, 9, 3, 14, 5, 0, 12, 7, 1, 15, 13, 8, 10, 3, 7, 4, 12, 5, 6,
        11, 0, 14, 9, 2, 7, 11, 4, 1, 9, 12, 14, 2, 0, 6, 10, 13, 15, 3, 5, 8, 2, 1, 14, 7, 4, 10,
        8, 13, 15, 12, 9, 0, 3, 5, 6, 11,
    ],
];

fn bits_of(block: u64) -> [u8; 64] {
    let mut bits = [0u8; 64];
    for (i, bit) in bits.iter_mut().enumerate() {
        *bit = ((block >> (63 - i)) & 1) as u8;
    }
    bits
}

fn block_of(bits: &[u8]) -> u64 {
    bits.iter()
        .fold(0u64, |acc, &bit| (acc << 1) | u64::from(bit))
}

/// Output of the DES round function for a zero round key.
fn feistel_zero_key(right: &[u8; 32]) -> [u8; 32] {
    debug_assert_eq!(right.len(), 32);
    let mut expanded = [0u8; 48];
    for (i, &pos) in E.iter().enumerate() {
        expanded[i] = right[(pos - 1) as usize];
    }
    let mut substituted = [0u8; 32];
    for (box_index, sbox) in S.iter().enumerate() {
        let chunk = &expanded[box_index * 6..box_index * 6 + 6];
        let row = ((chunk[0] << 1) | chunk[5]) as usize;
        let column = ((chunk[1] << 3) | (chunk[2] << 2) | (chunk[3] << 1) | chunk[4]) as usize;
        let value = sbox[row * 16 + column];
        for bit in 0..4 {
            substituted[box_index * 4 + bit] = (value >> (3 - bit)) & 1;
        }
    }
    let mut permuted = [0u8; 32];
    for (i, &pos) in P.iter().enumerate() {
        permuted[i] = substituted[(pos - 1) as usize];
    }
    permuted
}

/// `vitess.HashUint64`: one DES block encryption under the all-zero key.
pub fn hash_uint64(value: u64) -> u64 {
    let block = bits_of(value);
    let mut left = [0u8; 32];
    let mut right = [0u8; 32];
    for (i, &pos) in IP.iter().enumerate() {
        if i < 32 {
            left[i] = block[(pos - 1) as usize];
        } else {
            right[i - 32] = block[(pos - 1) as usize];
        }
    }
    for _ in 0..16 {
        let f = feistel_zero_key(&right);
        let mut next_right = [0u8; 32];
        for i in 0..32 {
            next_right[i] = left[i] ^ f[i];
        }
        left = right;
        right = next_right;
    }
    // The pre-output block is `R16 || L16`.
    let pre_output: Vec<u8> = right.iter().chain(left.iter()).copied().collect();
    let mut out = [0u8; 64];
    for (i, &pos) in FP.iter().enumerate() {
        out[i] = pre_output[(pos - 1) as usize];
    }
    block_of(&out)
}

/// TiDB `tidb_shard(x)`: `HashUint64(uint64(x)) % 256`.
pub fn tidb_shard(value: i64) -> i64 {
    (hash_uint64(value as u64) % 256) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Values pinned from TiDB `explain_shard_index.result`, where the shard
    /// rewrite renders `tidb_shard(a) = <bucket>`.
    #[test]
    fn test_tidb_shard_buckets_match_tidb() {
        assert_eq!(tidb_shard(100), 8);
        assert_eq!(tidb_shard(300), 227);
        assert_eq!(tidb_shard(200), 161);
    }

    #[test]
    fn test_hash_matches_null_key_des() {
        // `openssl enc -des-ecb -K 0000000000000000 -nopad` on the
        // big-endian uint64 blocks.
        assert_eq!(hash_uint64(100), 0x83aa_b156_9cbe_1b08);
        assert_eq!(hash_uint64(300), 0x8991_6b26_e4b5_4ce3);
        assert_eq!(hash_uint64(200), 0xc546_5cef_d139_3ca1);
    }
}
