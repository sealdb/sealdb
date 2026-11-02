//! unicode_ci (UCA 4.0.0) weight lookup — TiDB `unicode_0400_ci_impl.go`.
//!
//! MVB ships a Latin/ASCII subset; uncovered BMP code points fall back to the
//! code point itself as a single weight.

use super::ucadata::WEIGHT_SUBSET;
use std::collections::HashMap;
use std::sync::OnceLock;

const LONG_RUNE: u64 = 0xFFFD;

fn weight_map() -> &'static HashMap<char, u64> {
    static MAP: OnceLock<HashMap<char, u64>> = OnceLock::new();
    MAP.get_or_init(|| WEIGHT_SUBSET.iter().copied().collect())
}

/// Truncate trailing ASCII spaces (pad attribute for unicode_ci).
pub fn preprocess(s: &str) -> &str {
    s.trim_end_matches(' ')
}

/// First/second packed weights for a rune (TiDB `GetWeight`).
pub fn get_weight(r: char) -> (u64, u64) {
    if r as u32 > 0xFFFF {
        return (LONG_RUNE, 0);
    }
    match weight_map().get(&r).copied() {
        Some(w) if w == LONG_RUNE => (LONG_RUNE, 0),
        Some(w) => (w, 0),
        // Uncovered: identity weight so compare stays total-ordered.
        None => (u64::from(r as u32), 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latin_letters_share_case_weight() {
        assert_eq!(get_weight('A'), get_weight('a'));
        assert_eq!(get_weight('Ä'), get_weight('ä'));
        assert_eq!(get_weight('A'), get_weight('ä'));
    }

    #[test]
    fn sharp_s_expands_to_two_s_weights() {
        let (w, _) = get_weight('ß');
        assert_eq!(w & 0xFFFF, get_weight('s').0 & 0xFFFF);
        assert_eq!((w >> 16) & 0xFFFF, get_weight('s').0 & 0xFFFF);
    }
}
