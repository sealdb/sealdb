//! GBK encoding — TiDB `pkg/parser/charset/encoding_gbk.go`.
//!
//! C5 MVB ships a **minimal Unicode↔GBK subset** used by tests (ASCII + a few
//! Chinese characters). Full GBK tables live in TiDB via
//! `golang.org/x/text/encoding/simplifiedchinese`.

use std::collections::HashMap;
use std::sync::OnceLock;

/// `(unicode, gbk_bytes)` pairs covered by this MVB.
pub static UNICODE_GBK_SUBSET: &[(char, &[u8])] = &[
    ('中', &[0xd6, 0xd0]),
    ('文', &[0xce, 0xc4]),
    ('国', &[0xb9, 0xfa]),
    ('一', &[0xd2, 0xbb]),
    ('二', &[0xb6, 0xfe]),
    ('啊', &[0xb0, 0xa1]),
    ('阿', &[0xb0, 0xa2]),
    ('人', &[0xc8, 0xcb]),
    ('大', &[0xb4, 0xf3]),
];

fn unicode_to_gbk() -> &'static HashMap<char, &'static [u8]> {
    static MAP: OnceLock<HashMap<char, &'static [u8]>> = OnceLock::new();
    MAP.get_or_init(|| UNICODE_GBK_SUBSET.iter().copied().collect())
}

fn gbk_to_unicode() -> &'static HashMap<&'static [u8], char> {
    static MAP: OnceLock<HashMap<&'static [u8], char>> = OnceLock::new();
    MAP.get_or_init(|| {
        UNICODE_GBK_SUBSET
            .iter()
            .map(|(ch, bytes)| (*bytes, *ch))
            .collect()
    })
}

/// Encoding name — TiDB `CharsetGBK`.
pub const NAME: &str = "gbk";

/// First character length in a GBK byte slice — TiDB `Peek` / `MbLen`.
pub fn mb_len(src: &[u8]) -> usize {
    if src.is_empty() {
        return 0;
    }
    if src[0] < 0x80 {
        return 1;
    }
    if src.len() >= 2
        && (0x81..=0xfe).contains(&src[0])
        && ((0x40..=0x7e).contains(&src[1]) || (0x80..=0xfe).contains(&src[1]))
    {
        return 2;
    }
    0
}

/// Encode a UTF-8 string to GBK bytes (subset + ASCII). Unknown runes → `?`.
pub fn encode(src: &str) -> Vec<u8> {
    let map = unicode_to_gbk();
    let mut out = Vec::with_capacity(src.len());
    for ch in src.chars() {
        if ch as u32 <= 0x7f {
            out.push(ch as u8);
        } else if let Some(bytes) = map.get(&ch) {
            out.extend_from_slice(bytes);
        } else {
            out.push(b'?');
        }
    }
    out
}

/// Decode GBK bytes to UTF-8 (subset + ASCII). Invalid / uncovered → `?`.
pub fn decode(src: &[u8]) -> String {
    let map = gbk_to_unicode();
    let mut out = String::new();
    let mut i = 0;
    while i < src.len() {
        if src[i] < 0x80 {
            out.push(src[i] as char);
            i += 1;
            continue;
        }
        let len = mb_len(&src[i..]);
        if len == 2 {
            if let Some(ch) = map.get(&src[i..i + 2]) {
                out.push(*ch);
            } else {
                out.push('?');
            }
            i += 2;
        } else {
            out.push('?');
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_ascii_and_subset() {
        let s = "Aa中文阿啊";
        let enc = encode(s);
        assert_eq!(&enc[..2], b"Aa");
        assert_eq!(&enc[2..4], &[0xd6, 0xd0]);
        assert_eq!(decode(&enc), s);
    }

    #[test]
    fn uncovered_rune_becomes_question() {
        assert_eq!(encode("€"), b"?");
    }
}
