//! latin1 encoding — TiDB `pkg/parser/charset/encoding_latin1.go`.
//!
//! TiDB uses the UTF-8 implementation for latin1 for backward compatibility
//! (`encoding.Nop` transform). SealDB mirrors that: encode/decode are identity
//! on UTF-8 byte slices. A tiny Latin-1 letter subset is documented for tests
//! that exercise `SET NAMES latin1` + `latin1_bin` compares.

/// Encoding name — TiDB `CharsetLatin1`.
pub const NAME: &str = "latin1";

/// Latin-1 letters commonly used in C5 session tests (stored as UTF-8).
pub const LATIN1_TEST_SUBSET: &[char] = &['A', 'a', 'Z', 'z', 'é', 'É', 'ñ', 'Ñ'];

/// Encode: passthrough (TiDB latin1 ≡ utf8 storage).
pub fn encode(src: &str) -> Vec<u8> {
    src.as_bytes().to_vec()
}

/// Decode: interpret bytes as UTF-8 (lossy on invalid).
pub fn decode(src: &[u8]) -> String {
    String::from_utf8_lossy(src).into_owned()
}

/// Peek first character — always 1 byte for latin1.
pub fn peek(src: &[u8]) -> &[u8] {
    if src.is_empty() { src } else { &src[..1] }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_identity() {
        let s = "Aaéñ";
        let enc = encode(s);
        assert_eq!(enc, s.as_bytes());
        assert_eq!(decode(&enc), s);
    }

    #[test]
    fn peek_one_byte() {
        assert_eq!(peek(b"ab"), b"a");
        assert!(peek(b"").is_empty());
    }
}
