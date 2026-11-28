//! `utf8mb4_0900_ai_ci` sort keys (UCA 9.0.0 primary weights).
//!
//! TiDB stores a string index key for a new-collation column as the collation
//! sort key, and `ranger` builds the seek bounds from those keys, so the range
//! bounds EXPLAIN prints are the encoded weights rather than the literal text
//! (`auth_value = "3104..."` shows `"\x1c@\x1c>..."`).
//!
//! The weights come from DUCET 9.0.0 (`allkeys.txt`), the same table TiDB
//! generates into `ucadata.DUCET0900Table`: `MapTable4` holds up to four 16-bit
//! weights per code point and `LongRuneMap` the rare code points with more than
//! four. `ucadata_0900.bin` is that table (`u32` length, `u64` weights, then
//! the long-rune entries) so the data stays out of the compiler's way.

/// `ucadata_0900.bin`: `[u32 n][u64; n]`, then `[u32 m][(u32 rune, u64 first,
/// u64 second); m]`.
const DATA: &[u8] = include_bytes!("ucadata_0900.bin");

/// Marker in `MapTable4` for a code point whose weights live in the long map.
const LONG_RUNE: u64 = 0xFFFD;

fn u32_at(offset: usize) -> u32 {
    u32::from_le_bytes(DATA[offset..offset + 4].try_into().expect("4 bytes"))
}

fn u64_at(offset: usize) -> u64 {
    u64::from_le_bytes(DATA[offset..offset + 8].try_into().expect("8 bytes"))
}

fn map_len() -> usize {
    u32_at(0) as usize
}

fn map_entry(cp: u32) -> u64 {
    u64_at(4 + cp as usize * 8)
}

/// `(first, second)` weight words of `cp` (each holds up to four 16-bit
/// primary weights, least significant first).
fn rune_weights(cp: u32) -> (u64, u64) {
    if cp as usize >= map_len() {
        // UCA implicit weights for code points outside the table.
        let first = (cp >> 15) as u64 + 0xFBC0 + ((((cp & 0x7FFF) as u64) | 0x8000) << 16);
        return (first, 0);
    }
    let first = map_entry(cp);
    if first != LONG_RUNE {
        return (first, 0);
    }
    let long_count = u32_at(4 + map_len() * 8) as usize;
    let base = 4 + map_len() * 8 + 4;
    for entry in 0..long_count {
        let at = base + entry * 20;
        if u32_at(at) == cp {
            return (u64_at(at + 4), u64_at(at + 12));
        }
    }
    (0, 0)
}

/// Sort key of `text` under `utf8mb4_0900_ai_ci`.
///
/// `0900_ai_ci` is a NO PAD collation, so trailing spaces stay in the key.
pub fn sort_key_0900_ai_ci(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 2);
    for ch in text.chars() {
        let (first, second) = rune_weights(ch as u32);
        for mut weights in [first, second] {
            while weights != 0 {
                let weight = (weights & 0xFFFF) as u16;
                weights >>= 16;
                out.extend_from_slice(&weight.to_be_bytes());
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::sort_key_0900_ai_ci;

    fn key(text: &str) -> Vec<u8> {
        sort_key_0900_ai_ci(text).expect("sort key")
    }

    #[test]
    fn digits_and_letters_use_the_ducet_weights() {
        assert_eq!(key("a"), vec![0x1C, 0x47]);
        assert_eq!(key("0"), vec![0x1C, 0x3D]);
        assert_eq!(key("z"), vec![0x1F, 0x21]);
        assert_eq!(key("A"), key("a"));
        assert_eq!(key("@"), vec![0x03, 0x8E]);
    }

    #[test]
    fn cjk_ideographs_use_implicit_weights() {
        assert_eq!(key("\u{6d4b}"), vec![0xFB, 0x40, 0xED, 0x4B]);
    }

    #[test]
    fn symbols_sort_before_letters_and_kana_after() {
        // UCA orders symbols (`[]`, emoji) before digits and letters, and the
        // kana block after the Latin letters.
        assert!(key("[") < key("W"));
        assert!(key("\u{1F363}") < key("abc"));
        assert!(key("abc") < key("\u{3042}"));
        assert!(key("abc") < key("\u{FF76}"));
    }

    #[test]
    fn case_width_and_accents_share_a_key() {
        assert_eq!(key("abc"), key("AbC"));
        assert_eq!(key("\u{3042}"), key("\u{30A2}"));
        assert_eq!(key("e"), key("\u{e9}"));
    }
}
