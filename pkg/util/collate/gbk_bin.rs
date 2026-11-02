//! `gbk_bin` collator — TiDB `pkg/util/collate/gbk_bin.go`.
//!
//! Compare UTF-8 strings by converting each rune to GBK bytes (MVB subset in
//! `sealdb_parser::charset::encoding_gbk`), then lexicographically comparing
//! those bytes. Trailing ASCII spaces are trimmed (PAD SPACE).

use sealdb_parser::charset::encoding_gbk;
use std::cmp::Ordering;

fn truncate_trailing_space(s: &str) -> &str {
    s.trim_end_matches(' ')
}

fn rune_byte_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first < 0xe0 {
        2
    } else if first < 0xf0 {
        3
    } else {
        4
    }
}

/// Encode one UTF-8 rune slice to GBK; unknown → `?` (0x3F), matching TiDB.
fn encode_rune_slice(utf8: &str) -> Vec<u8> {
    let gbk = encoding_gbk::encode(utf8);
    if gbk.is_empty() { vec![0x3f] } else { gbk }
}

/// Compare under `gbk_bin`.
pub fn compare(a: &str, b: &str) -> Ordering {
    let mut a = truncate_trailing_space(a);
    let mut b = truncate_trailing_space(b);

    while !a.is_empty() && !b.is_empty() {
        let a_len = rune_byte_len(a.as_bytes()[0]).min(a.len());
        let b_len = rune_byte_len(b.as_bytes()[0]).min(b.len());
        let a_gbk = encode_rune_slice(&a[..a_len]);
        let b_gbk = encode_rune_slice(&b[..b_len]);
        let cmp = a_gbk.as_slice().cmp(b_gbk.as_slice());
        if cmp != Ordering::Equal {
            return cmp;
        }
        a = &a[a_len..];
        b = &b[b_len..];
    }
    a.len().cmp(&b.len())
}

/// Sort key under `gbk_bin` (GBK bytes, trailing spaces trimmed).
pub fn key(s: &str) -> Vec<u8> {
    let s = truncate_trailing_space(s);
    let mut buf = Vec::with_capacity(s.len());
    let mut rest = s;
    while !rest.is_empty() {
        let len = rune_byte_len(rest.as_bytes()[0]).min(rest.len());
        buf.extend_from_slice(&encode_rune_slice(&rest[..len]));
        rest = &rest[len..];
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chinese_order_by_gbk_bytes() {
        // 啊=B0A1, 阿=B0A2 → 啊 < 阿 under gbk_bin.
        assert_eq!(compare("啊", "阿"), Ordering::Less);
        assert_eq!(compare("阿", "啊"), Ordering::Greater);
    }

    #[test]
    fn ascii_binary() {
        assert_eq!(compare("A", "a"), Ordering::Less);
        assert_eq!(compare("a", "a"), Ordering::Equal);
    }
}
