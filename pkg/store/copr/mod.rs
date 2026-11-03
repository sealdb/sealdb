//! TiDB `pkg/store/copr`: Region-aware key-range split + Cop task scheduling.
//!
//! `split_key_ranges` is the Mem / no-PD fallback (single span).
//! `split_key_ranges_via_pd` walks PD HTTP `/pd/api/v1/region/key/{hex}` to
//! produce Region-aligned half-open ranges for Cop / TableReader.
//! `split_key_ranges_for_scan` uses process PD endpoints set by `store::open`.

use sealdb_errno::{ERR_UNKNOWN_ERROR, Error, Result};
use serde::Deserialize;
use std::sync::Mutex;

static PD_ENDPOINTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Record PD HTTP endpoints used by `split_key_ranges_for_scan` (called from `store::open`).
pub fn set_pd_endpoints(endpoints: Vec<String>) {
    if let Ok(mut guard) = PD_ENDPOINTS.lock() {
        *guard = endpoints;
    }
}

/// Clear process PD endpoints (Mem store / tests).
pub fn clear_pd_endpoints() {
    set_pd_endpoints(Vec::new());
}

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

/// Split `[start, end)` into a single span (MemStorage / offline fallback).
pub fn split_key_ranges(start: &[u8], end: &[u8]) -> Vec<KeyRange> {
    if end <= start {
        return Vec::new();
    }
    vec![KeyRange::new(start.to_vec(), end.to_vec())]
}

/// Prefer PD Region locate when endpoints are configured; otherwise single span.
/// On PD errors, falls back to a single contiguous range so scans stay available.
pub fn split_key_ranges_for_scan(start: &[u8], end: &[u8]) -> Vec<KeyRange> {
    let endpoints = PD_ENDPOINTS
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default();
    if endpoints.is_empty() {
        return split_key_ranges(start, end);
    }
    match split_key_ranges_via_pd(&endpoints, start, end) {
        Ok(ranges) if !ranges.is_empty() => ranges,
        _ => split_key_ranges(start, end),
    }
}

/// Split `[start, end)` by querying PD for the Region containing each cursor.
///
/// Empty `end_key` from PD means the last Region (extends to `end`).
pub fn split_key_ranges_via_pd(
    pd_endpoints: &[String],
    start: &[u8],
    end: &[u8],
) -> Result<Vec<KeyRange>> {
    if end <= start {
        return Ok(Vec::new());
    }
    if pd_endpoints.is_empty() {
        return Err(Error::new(ERR_UNKNOWN_ERROR, "empty PD endpoints"));
    }
    let mut out = Vec::new();
    let mut cursor = start.to_vec();
    // Cap iterations so a bad PD cannot loop forever.
    for _ in 0..10_000 {
        if cursor.as_slice() >= end {
            break;
        }
        let region = pd_get_region(pd_endpoints, &cursor)?;
        let region_start = decode_pd_key(&region.start_key)?;
        let region_end = if region.end_key.is_empty() {
            end.to_vec()
        } else {
            decode_pd_key(&region.end_key)?
        };
        let range_start = if cursor.as_slice() > region_start.as_slice() {
            cursor.clone()
        } else {
            region_start
        };
        let range_end = if region_end.as_slice() < end {
            region_end.clone()
        } else {
            end.to_vec()
        };
        if range_start.as_slice() < range_end.as_slice() {
            out.push(KeyRange::new(range_start, range_end.clone()));
        }
        if region.end_key.is_empty() || region_end.as_slice() >= end {
            break;
        }
        cursor = region_end;
    }
    if out.is_empty() {
        // PD unreachable / empty meta: fall back to a single span.
        out.push(KeyRange::new(start.to_vec(), end.to_vec()));
    }
    Ok(out)
}

#[derive(Debug, Deserialize)]
struct PdRegion {
    #[serde(default)]
    start_key: String,
    #[serde(default)]
    end_key: String,
}

fn pd_get_region(pd_endpoints: &[String], key: &[u8]) -> Result<PdRegion> {
    let hex = hex_encode(key);
    let mut last_err = None;
    for ep in pd_endpoints {
        let base = normalize_pd_http(ep);
        let url = format!("{base}/pd/api/v1/region/key/{hex}");
        match reqwest::blocking::get(&url).and_then(|r| r.error_for_status()) {
            Ok(resp) => {
                return resp.json::<PdRegion>().map_err(|e| {
                    Error::new(ERR_UNKNOWN_ERROR, format!("pd region decode: {e}"))
                });
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(Error::new(
        ERR_UNKNOWN_ERROR,
        format!(
            "pd get region failed: {}",
            last_err
                .map(|e| e.to_string())
                .unwrap_or_else(|| "no endpoints".into())
        ),
    ))
}

fn normalize_pd_http(endpoint: &str) -> String {
    let ep = endpoint.trim().trim_end_matches('/');
    if ep.starts_with("http://") || ep.starts_with("https://") {
        ep.to_string()
    } else {
        format!("http://{ep}")
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

fn decode_pd_key(hex: &str) -> Result<Vec<u8>> {
    if hex.is_empty() {
        return Ok(Vec::new());
    }
    let h = hex.as_bytes();
    if h.len() % 2 != 0 {
        return Err(Error::new(
            ERR_UNKNOWN_ERROR,
            format!("odd-length PD key hex: {hex}"),
        ));
    }
    let mut out = Vec::with_capacity(h.len() / 2);
    let mut i = 0;
    while i < h.len() {
        let hi = hex_nibble(h[i])?;
        let lo = hex_nibble(h[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

fn hex_nibble(c: u8) -> Result<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(Error::new(
            ERR_UNKNOWN_ERROR,
            format!("invalid hex digit {}", c as char),
        )),
    }
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

    #[test]
    fn test_hex_roundtrip() {
        let raw = b"\x01\xab\xff";
        let hex = hex_encode(raw);
        assert_eq!(hex, "01abff");
        assert_eq!(decode_pd_key(&hex).unwrap(), raw);
        assert!(decode_pd_key("").unwrap().is_empty());
    }

    #[test]
    fn test_split_key_ranges_via_pd_if_available() {
        let ranges = match split_key_ranges_via_pd(
            &["127.0.0.1:2379".to_string()],
            b"t",
            b"u",
        ) {
            Ok(r) => r,
            Err(err) => {
                eprintln!("skip pd region split: {err}");
                return;
            }
        };
        assert!(!ranges.is_empty());
        assert_eq!(ranges.first().unwrap().start, b"t");
        assert_eq!(ranges.last().unwrap().end, b"u");
        for w in ranges.windows(2) {
            assert_eq!(w[0].end, w[1].start);
        }
    }
}
