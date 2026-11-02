//! `gbk_chinese_ci` collator — TiDB `pkg/util/collate/gbk_chinese_ci.go`.
//!
//! Sort keys come from the MVB subset in `gbk_chinese_ci_data`.

use super::gbk_chinese_ci_data::SORT_KEY_SUBSET;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::OnceLock;

fn sort_key_map() -> &'static HashMap<char, u16> {
    static MAP: OnceLock<HashMap<char, u16>> = OnceLock::new();
    MAP.get_or_init(|| SORT_KEY_SUBSET.iter().copied().collect())
}

fn truncate_trailing_space(s: &str) -> &str {
    s.trim_end_matches(' ')
}

/// TiDB `gbkChineseCISortKey`.
pub fn sort_key(r: char) -> u32 {
    if r as u32 > 0xffff {
        return 0x3f;
    }
    u32::from(sort_key_map().get(&r).copied().unwrap_or(0x3f))
}

/// Compare under `gbk_chinese_ci` (TiDB `compareCommon` + sort key).
pub fn compare(a: &str, b: &str) -> Ordering {
    let a = truncate_trailing_space(a);
    let b = truncate_trailing_space(b);
    let mut ai = a.chars();
    let mut bi = b.chars();
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ra), Some(rb)) => {
                let ka = sort_key(ra);
                let kb = sort_key(rb);
                if ka != kb {
                    return ka.cmp(&kb);
                }
            }
        }
    }
}

/// Sort key bytes (TiDB `KeyWithoutTrimRightSpace` after trim).
pub fn key(s: &str) -> Vec<u8> {
    let s = truncate_trailing_space(s);
    let mut buf = Vec::with_capacity(s.len() * 2);
    for r in s.chars() {
        let u16 = sort_key(r);
        if u16 > 0xff {
            buf.push((u16 >> 8) as u8);
        }
        buf.push(u16 as u8);
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_insensitive_ascii() {
        assert_eq!(compare("A", "a"), Ordering::Equal);
        assert_eq!(sort_key('A'), sort_key('a'));
    }

    #[test]
    fn chinese_ci_order_differs_from_gbk_bin() {
        // 阿 key 0x8453 < 啊 key 0x8454 under gbk_chinese_ci.
        assert_eq!(compare("阿", "啊"), Ordering::Less);
        assert_eq!(compare("啊", "阿"), Ordering::Greater);
    }
}
