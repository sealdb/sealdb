//! PD HTTP helpers for Region locate and store address resolution.

use sealdb_errno::{ERR_UNKNOWN_ERROR, Error, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct PdRegion {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub start_key: String,
    #[serde(default)]
    pub end_key: String,
    #[serde(default)]
    pub epoch: Option<PdRegionEpoch>,
    #[serde(default)]
    pub leader: Option<PdPeer>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PdRegionEpoch {
    #[serde(default)]
    pub conf_ver: u64,
    #[serde(default)]
    pub version: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PdPeer {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub store_id: u64,
}

#[derive(Debug, Deserialize)]
struct PdStoreWrap {
    store: PdStore,
}

#[derive(Debug, Deserialize)]
struct PdStore {
    #[serde(default)]
    address: String,
}

pub fn pd_get_region(pd_endpoints: &[String], key: &[u8]) -> Result<PdRegion> {
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

pub fn pd_store_address(pd_endpoints: &[String], store_id: u64) -> Result<String> {
    let mut last_err = None;
    for ep in pd_endpoints {
        let base = normalize_pd_http(ep);
        let url = format!("{base}/pd/api/v1/store/{store_id}");
        match reqwest::blocking::get(&url).and_then(|r| r.error_for_status()) {
            Ok(resp) => {
                let wrap = resp.json::<PdStoreWrap>().map_err(|e| {
                    Error::new(ERR_UNKNOWN_ERROR, format!("pd store decode: {e}"))
                })?;
                if wrap.store.address.is_empty() {
                    return Err(Error::new(
                        ERR_UNKNOWN_ERROR,
                        format!("empty address for store {store_id}"),
                    ));
                }
                return Ok(wrap.store.address);
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(Error::new(
        ERR_UNKNOWN_ERROR,
        format!(
            "pd get store failed: {}",
            last_err
                .map(|e| e.to_string())
                .unwrap_or_else(|| "no endpoints".into())
        ),
    ))
}

pub fn normalize_pd_http(endpoint: &str) -> String {
    let ep = endpoint.trim().trim_end_matches('/');
    if ep.starts_with("http://") || ep.starts_with("https://") {
        ep.to_string()
    } else {
        format!("http://{ep}")
    }
}

pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

pub fn decode_pd_key(hex: &str) -> Result<Vec<u8>> {
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
