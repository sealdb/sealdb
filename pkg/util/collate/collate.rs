//! Collation dispatch — TiDB `pkg/util/collate/collate.go` (unicode path MVB).

use super::unicode_0400_ci_generated;
use std::cmp::Ordering;

/// Session / expression collation mode.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CollateMode {
    #[default]
    Bin,
    /// ASCII case-insensitive (`utf8mb4_general_ci` and other non-unicode `*_ci`).
    GeneralCi,
    /// UCA 4.0.0 weight path (`utf8mb4_unicode_ci` / `utf8_unicode_ci`).
    UnicodeCi,
}

impl CollateMode {
    pub fn from_name(name: &str) -> Self {
        let lower = name
            .trim_matches(|c| c == '\'' || c == '"')
            .to_ascii_lowercase();
        if lower.contains("unicode") {
            Self::UnicodeCi
        } else if lower.ends_with("_ci") {
            Self::GeneralCi
        } else {
            Self::Bin
        }
    }

    pub fn is_ci(self) -> bool {
        !matches!(self, Self::Bin)
    }
}

/// Compare UTF-8 byte strings under `mode`.
pub fn compare_bytes(mode: CollateMode, left: &[u8], right: &[u8]) -> Ordering {
    match mode {
        CollateMode::Bin => left.cmp(right),
        CollateMode::GeneralCi => {
            if left.eq_ignore_ascii_case(right) {
                Ordering::Equal
            } else {
                left.to_ascii_lowercase().cmp(&right.to_ascii_lowercase())
            }
        }
        CollateMode::UnicodeCi => {
            let ls = std::str::from_utf8(left).unwrap_or("");
            let rs = std::str::from_utf8(right).unwrap_or("");
            unicode_0400_ci_generated::compare(ls, rs)
        }
    }
}

/// Case/weight equality used by LIKE when collation is CI.
pub fn bytes_eq(mode: CollateMode, a: u8, b: u8) -> bool {
    match mode {
        CollateMode::Bin => a == b,
        CollateMode::GeneralCi | CollateMode::UnicodeCi => a.eq_ignore_ascii_case(&b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_from_name() {
        assert_eq!(CollateMode::from_name("utf8mb4_bin"), CollateMode::Bin);
        assert_eq!(
            CollateMode::from_name("utf8mb4_general_ci"),
            CollateMode::GeneralCi
        );
        assert_eq!(
            CollateMode::from_name("utf8mb4_unicode_ci"),
            CollateMode::UnicodeCi
        );
    }

    #[test]
    fn unicode_compare_bytes() {
        assert_eq!(
            compare_bytes(CollateMode::UnicodeCi, b"A", b"a"),
            Ordering::Equal
        );
        assert_eq!(compare_bytes(CollateMode::Bin, b"A", b"a"), Ordering::Less);
    }
}
