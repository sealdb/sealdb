//! Collation dispatch — TiDB `pkg/util/collate/collate.go`.

use super::{gbk_bin, gbk_chinese_ci, general_ci, unicode_0400_ci_generated, unicode_0900};
use std::cmp::Ordering;

/// Session / expression collation mode.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CollateMode {
    /// `binary` / `utf8mb4_0900_bin`: raw bytes, NO PAD.
    #[default]
    Bin,
    /// Legacy `*_bin` collations (`utf8mb4_bin`, `latin1_bin`, ...): byte order
    /// after trailing ASCII spaces are ignored (PAD SPACE).
    BinPadded,
    /// ASCII case-insensitive (`utf8mb4_general_ci` and other non-unicode `*_ci`).
    GeneralCi,
    /// UCA 4.0.0 weight path (`utf8mb4_unicode_ci` / `utf8_unicode_ci`).
    UnicodeCi,
    /// UCA 9.0.0 sort key path (`utf8mb4_0900_ai_ci`), NO PAD.
    Unicode0900,
    /// GBK binary byte order after UTF-8→GBK encode (`gbk_bin`).
    GbkBin,
    /// GBK Chinese CI sort keys (`gbk_chinese_ci`).
    GbkChineseCi,
}

impl CollateMode {
    pub fn from_name(name: &str) -> Self {
        let lower = name
            .trim_matches(|c| c == '\'' || c == '"')
            .to_ascii_lowercase();
        if lower == "binary" || lower == "utf8mb4_0900_bin" {
            Self::Bin
        } else if lower == "gbk_chinese_ci" || (lower.starts_with("gbk") && lower.ends_with("_ci"))
        {
            Self::GbkChineseCi
        } else if lower == "gbk_bin" || (lower.starts_with("gbk") && lower.ends_with("_bin")) {
            Self::GbkBin
        } else if lower.contains("0900") {
            Self::Unicode0900
        } else if lower.contains("unicode") {
            Self::UnicodeCi
        } else if lower.ends_with("_ci") {
            Self::GeneralCi
        } else if lower.ends_with("_bin") {
            Self::BinPadded
        } else {
            Self::Bin
        }
    }

    pub fn is_ci(self) -> bool {
        matches!(
            self,
            Self::GeneralCi | Self::UnicodeCi | Self::Unicode0900 | Self::GbkChineseCi
        )
    }

    /// Whether trailing ASCII spaces are insignificant for this collation.
    pub fn is_pad_space(self) -> bool {
        matches!(
            self,
            Self::BinPadded | Self::GeneralCi | Self::UnicodeCi | Self::GbkBin | Self::GbkChineseCi
        )
    }
}

/// Strip trailing ASCII spaces (MySQL PAD SPACE semantics).
pub fn trim_trailing_space(s: &str) -> &str {
    s.trim_end_matches(' ')
}

/// Compare UTF-8 byte strings under `mode`.
pub fn compare_bytes(mode: CollateMode, left: &[u8], right: &[u8]) -> Ordering {
    match mode {
        CollateMode::Bin => left.cmp(right),
        CollateMode::BinPadded => {
            let l = trim_trailing_space(std::str::from_utf8(left).unwrap_or(""));
            let r = trim_trailing_space(std::str::from_utf8(right).unwrap_or(""));
            l.as_bytes().cmp(r.as_bytes())
        }
        CollateMode::GeneralCi => {
            let ls = std::str::from_utf8(left).unwrap_or("");
            let rs = std::str::from_utf8(right).unwrap_or("");
            general_ci::compare(ls, rs)
        }
        CollateMode::UnicodeCi => {
            let ls = std::str::from_utf8(left).unwrap_or("");
            let rs = std::str::from_utf8(right).unwrap_or("");
            unicode_0400_ci_generated::compare(ls, rs)
        }
        CollateMode::Unicode0900 => {
            let ls = std::str::from_utf8(left).unwrap_or("");
            let rs = std::str::from_utf8(right).unwrap_or("");
            unicode_0900::compare(ls, rs)
        }
        CollateMode::GbkBin => {
            let ls = std::str::from_utf8(left).unwrap_or("");
            let rs = std::str::from_utf8(right).unwrap_or("");
            gbk_bin::compare(ls, rs)
        }
        CollateMode::GbkChineseCi => {
            let ls = std::str::from_utf8(left).unwrap_or("");
            let rs = std::str::from_utf8(right).unwrap_or("");
            gbk_chinese_ci::compare(ls, rs)
        }
    }
}

/// Collation sort key bytes for a string value (TiDB `Collator.Key`).
///
/// Used for index keys / hash keys so that equality under the collation also
/// dedups index entries (e.g. `'a'` and `'a   '` in `utf8mb4_general_ci`).
/// `None` means "no transformation" (raw bytes are the key).
pub fn key_bytes(mode: CollateMode, bytes: &[u8]) -> Option<Vec<u8>> {
    let s = std::str::from_utf8(bytes).ok()?;
    match mode {
        CollateMode::Bin => None,
        CollateMode::BinPadded => {
            let t = trim_trailing_space(s);
            (t.len() != s.len()).then(|| t.as_bytes().to_vec())
        }
        CollateMode::GeneralCi => general_ci::key(s),
        CollateMode::UnicodeCi => Some(unicode_0400_ci_generated::key(s)),
        CollateMode::Unicode0900 => unicode_0900::key(s),
        CollateMode::GbkBin => Some(gbk_bin::key(s)),
        CollateMode::GbkChineseCi => Some(gbk_chinese_ci::key(s)),
    }
}

/// Collation sort key bytes **without** trimming trailing spaces (TiDB
/// `Collator.KeyWithoutTrimRightSpace`), used by `LOCATE`.
/// `None` means "no transformation" (raw bytes are the key).
pub fn key_bytes_no_trim(mode: CollateMode, bytes: &[u8]) -> Option<Vec<u8>> {
    let s = std::str::from_utf8(bytes).ok()?;
    match mode {
        CollateMode::Bin | CollateMode::BinPadded => None,
        CollateMode::GeneralCi => general_ci::key_no_trim(s),
        CollateMode::UnicodeCi => Some(unicode_0400_ci_generated::key_no_trim(s)),
        CollateMode::Unicode0900 => unicode_0900::key(s),
        CollateMode::GbkBin => Some(gbk_bin::key_no_trim(s)),
        CollateMode::GbkChineseCi => Some(gbk_chinese_ci::key_no_trim(s)),
    }
}

/// Case/weight equality used by LIKE when collation is CI.
pub fn bytes_eq(mode: CollateMode, a: u8, b: u8) -> bool {
    match mode {
        CollateMode::Bin | CollateMode::BinPadded | CollateMode::GbkBin => a == b,
        CollateMode::GeneralCi
        | CollateMode::UnicodeCi
        | CollateMode::Unicode0900
        | CollateMode::GbkChineseCi => a.eq_ignore_ascii_case(&b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_from_name() {
        assert_eq!(
            CollateMode::from_name("utf8mb4_bin"),
            CollateMode::BinPadded
        );
        assert_eq!(CollateMode::from_name("binary"), CollateMode::Bin);
        assert_eq!(CollateMode::from_name("utf8mb4_0900_bin"), CollateMode::Bin);
        assert_eq!(
            CollateMode::from_name("utf8mb4_general_ci"),
            CollateMode::GeneralCi
        );
        assert_eq!(
            CollateMode::from_name("utf8mb4_unicode_ci"),
            CollateMode::UnicodeCi
        );
        assert_eq!(
            CollateMode::from_name("utf8mb4_0900_ai_ci"),
            CollateMode::Unicode0900
        );
        assert_eq!(CollateMode::from_name("gbk_bin"), CollateMode::GbkBin);
        assert_eq!(
            CollateMode::from_name("gbk_chinese_ci"),
            CollateMode::GbkChineseCi
        );
        assert_eq!(CollateMode::from_name("latin1_bin"), CollateMode::BinPadded);
    }

    #[test]
    fn unicode_compare_bytes() {
        assert_eq!(
            compare_bytes(CollateMode::UnicodeCi, b"A", b"a"),
            Ordering::Equal
        );
        assert_eq!(compare_bytes(CollateMode::Bin, b"A", b"a"), Ordering::Less);
    }

    #[test]
    fn general_ci_pads_and_ignores_case() {
        assert_eq!(
            compare_bytes(CollateMode::GeneralCi, b"a", b"a   "),
            Ordering::Equal
        );
        assert_eq!(
            compare_bytes(CollateMode::GeneralCi, b"A", b"a"),
            Ordering::Equal
        );
        assert_eq!(
            compare_bytes(CollateMode::BinPadded, b"a", b"a   "),
            Ordering::Equal
        );
        assert_ne!(
            compare_bytes(CollateMode::Bin, b"a", b"a   "),
            Ordering::Equal
        );
    }

    #[test]
    fn gbk_modes_compare() {
        let a = "啊".as_bytes();
        let b = "阿".as_bytes();
        assert_eq!(compare_bytes(CollateMode::GbkBin, a, b), Ordering::Less);
        assert_eq!(
            compare_bytes(CollateMode::GbkChineseCi, b, a),
            Ordering::Less
        );
        assert_eq!(
            compare_bytes(CollateMode::GbkChineseCi, b"A", b"a"),
            Ordering::Equal
        );
    }
}
