//! `utf8mb4_0900_ai_ci` sort keys (UCA 9.0.0 primary weights).
//!
//! TiDB stores a string index key for a new-collation column as the collation
//! sort key, and `ranger` builds the seek bounds from those keys, so the range
//! bounds EXPLAIN prints are the encoded weights rather than the literal text
//! (`auth_value = "3104..."` shows `"\x1c@\x1c>..."`).
//!
//! Only the parts needed to reproduce those keys are modelled here: the ASCII
//! weights from DUCET 9.0.0 (the table TiDB generates from
//! `allkeys.txt`) and the implicit CJK weights, which cover the ideograph
//! ranges UCA leaves unassigned. A character outside both returns `None` so the
//! caller keeps the literal text instead of printing a half-encoded key.

/// Primary weights of `U+0000..=U+007F`, packed four 16-bit weights to a word
/// in least-significant-first order (the layout of TiDB's `MapTable4`).
const ASCII_WEIGHTS: [u32; 128] = [
    0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0201, 0x0202, 0x0203,
    0x0204, 0x0205, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000,
    0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0209, 0x0260, 0x030C, 0x0398,
    0x1C12, 0x0399, 0x0396, 0x0305, 0x0317, 0x0318, 0x038F, 0x0616, 0x0222, 0x020D, 0x0277, 0x0394,
    0x1C3D, 0x1C3E, 0x1C3F, 0x1C40, 0x1C41, 0x1C42, 0x1C43, 0x1C44, 0x1C45, 0x1C46, 0x0239, 0x0234,
    0x061A, 0x061B, 0x061C, 0x0266, 0x038E, 0x1C47, 0x1C60, 0x1C7A, 0x1C8F, 0x1CAA, 0x1CE5, 0x1CF4,
    0x1D18, 0x1D32, 0x1D4C, 0x1D65, 0x1D77, 0x1DAA, 0x1DB9, 0x1DDD, 0x1E0C, 0x1E21, 0x1E33, 0x1E71,
    0x1E95, 0x1EB5, 0x1EE3, 0x1EF5, 0x1EFF, 0x1F0B, 0x1F21, 0x0319, 0x0395, 0x031A, 0x0485, 0x020B,
    0x0482, 0x1C47, 0x1C60, 0x1C7A, 0x1C8F, 0x1CAA, 0x1CE5, 0x1CF4, 0x1D18, 0x1D32, 0x1D4C, 0x1D65,
    0x1D77, 0x1DAA, 0x1DB9, 0x1DDD, 0x1E0C, 0x1E21, 0x1E33, 0x1E71, 0x1E95, 0x1EB5, 0x1EE3, 0x1EF5,
    0x1EFF, 0x1F0B, 0x1F21, 0x031B, 0x061E, 0x031C, 0x0620, 0x0000,
];

/// Sort key of `text` under `utf8mb4_0900_ai_ci`, or `None` when the text has a
/// character whose weight is not modelled.
///
/// `0900_ai_ci` is a NO PAD collation, so trailing spaces stay in the key.
pub fn sort_key_0900_ai_ci(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 2);
    for ch in text.chars() {
        let mut weights = rune_weights(ch as u32)?;
        while weights != 0 {
            let weight = (weights & 0xFFFF) as u16;
            weights >>= 16;
            out.extend_from_slice(&weight.to_be_bytes());
        }
    }
    Some(out)
}

/// Packed primary weights of one code point (`0` when it has none).
fn rune_weights(cp: u32) -> Option<u32> {
    if let Some(weight) = ASCII_WEIGHTS.get(cp as usize) {
        return Some(*weight);
    }
    // UCA implicit weights for the CJK ideograph ranges TiDB's table carries
    // with these values: `0xFB40 + plane`, then the low 15 bits with the high
    // bit set.
    if (0x3400..=0x9FFF).contains(&cp) || (0x20000..=0x2FFFF).contains(&cp) {
        let first = 0xFB40u32 + (cp >> 15);
        let second = (cp & 0x7FFF) | 0x8000;
        return Some(first | (second << 16));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::sort_key_0900_ai_ci;

    fn key(text: &str) -> Vec<u8> {
        sort_key_0900_ai_ci(text).expect("modelled code points")
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
    fn unmodelled_characters_are_rejected() {
        assert!(sort_key_0900_ai_ci("\u{3042}").is_none());
    }
}
