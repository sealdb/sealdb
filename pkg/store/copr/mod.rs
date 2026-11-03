//! TiDB `pkg/store/copr`: Region-aware key-range split + Cop task scheduling.
//!
//! MVB: ranges are returned as a single contiguous `KeyRange`. RegionCache-
//! backed multi-region split lands when the driver exposes locate APIs (C6-C).

/// Half-open key range `[start, end)`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyRange {
    pub start: Vec<u8>,
    pub end: Vec<u8>,
}

impl KeyRange {
    pub fn new(start: impl Into<Vec<u8>>, end: impl Into<Vec<u8>>) -> Self {
        Self {
            start: start.into(),
            end: end.into(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

/// Split `[start, end)` into Region-aligned ranges for Cop / distsql.
///
/// Until Region locate is wired through `pkg/store/driver`, this returns at
/// most one range covering the full span (empty when `end <= start`).
pub fn split_key_ranges(start: &[u8], end: &[u8]) -> Vec<KeyRange> {
    if end <= start {
        return Vec::new();
    }
    vec![KeyRange::new(start.to_vec(), end.to_vec())]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_key_ranges_mvb_single() {
        let ranges = split_key_ranges(b"a", b"z");
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].start, b"a");
        assert_eq!(ranges[0].end, b"z");
        assert!(split_key_ranges(b"z", b"a").is_empty());
        assert!(split_key_ranges(b"a", b"a").is_empty());
    }
}
