//! `utf8mb4_0900_ai_ci` comparisons and keys — TiDB
//! `pkg/util/collate/unicode_0900_ai_ci.go`.
//!
//! NO PAD (trailing spaces are significant): comparison and the index key both
//! use the DUCET 9.0.0 primary-weight sort key.

use sealdb_parser::charset::collate_0900::sort_key_0900_ai_ci;
use std::cmp::Ordering;

/// Compare `a` and `b` under `utf8mb4_0900_ai_ci`.
pub fn compare(a: &str, b: &str) -> Ordering {
    match (sort_key_0900_ai_ci(a), sort_key_0900_ai_ci(b)) {
        (Some(ka), Some(kb)) => ka.cmp(&kb),
        // Unmodelled code points: fall back to a case-insensitive byte order
        // (better than claiming equality for distinct text).
        _ => a.to_lowercase().as_bytes().cmp(b.to_lowercase().as_bytes()),
    }
}

/// Sort-key bytes (`None` keeps the raw value for unmodelled code points).
pub fn key(s: &str) -> Option<Vec<u8>> {
    sort_key_0900_ai_ci(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_insensitive_no_pad() {
        assert_eq!(compare("A", "a"), Ordering::Equal);
        assert_ne!(compare("a", "a "), Ordering::Equal);
    }
}
