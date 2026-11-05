//! PD HTTP helpers for Region locate, store listing, and region scan.

use sealdb_errno::{ERR_UNKNOWN_ERROR, Error, Result};
use serde::Deserialize;
use std::time::Duration;

fn pd_http_client() -> reqwest::Result<reqwest::blocking::Client> {
    // Bypass env/system proxies — Clash/SOCKS on macOS otherwise hijacks 127.0.0.1.
    reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .pool_max_idle_per_host(4)
        .build()
}

fn pd_http_get(url: &str) -> reqwest::Result<reqwest::blocking::Response> {
    let client = pd_http_client()?;
    let mut last = None;
    for attempt in 0..3 {
        match client.get(url).send() {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                last = Some(e);
                if attempt + 1 < 3 {
                    std::thread::sleep(Duration::from_millis(50 * (attempt + 1) as u64));
                }
            }
        }
    }
    Err(last.expect("pd_http_get retry"))
}

#[derive(Debug, Deserialize, Clone)]
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
    store: PdStoreInfo,
}

#[derive(Debug, Deserialize)]
struct PdStoresWrap {
    #[serde(default)]
    stores: Vec<PdStoreEntry>,
}

#[derive(Debug, Deserialize)]
struct PdStoreEntry {
    store: PdStoreInfo,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PdStoreLabel {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub value: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PdStoreInfo {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub state_name: String,
    #[serde(default)]
    pub labels: Vec<PdStoreLabel>,
}

impl PdStoreInfo {
    /// TiFlash stores carry label `engine=tiflash` (TiDB / PD convention).
    pub fn is_tiflash(&self) -> bool {
        self.labels.iter().any(|l| {
            l.key.eq_ignore_ascii_case("engine") && l.value.eq_ignore_ascii_case("tiflash")
        })
    }

    pub fn is_up(&self) -> bool {
        self.state_name.is_empty()
            || self.state_name.eq_ignore_ascii_case("Up")
            || self.state_name.eq_ignore_ascii_case("UP")
    }
}

#[derive(Debug, Deserialize)]
struct PdRegionsWrap {
    #[serde(default)]
    count: u64,
    #[serde(default)]
    regions: Vec<PdRegion>,
}

/// Thin PD HTTP client used by Cop / admin surfaces.
#[derive(Clone, Debug, Default)]
pub struct PdClient {
    pub endpoints: Vec<String>,
}

impl PdClient {
    pub fn new(endpoints: Vec<String>) -> Self {
        Self { endpoints }
    }

    pub fn get_region(&self, key: &[u8]) -> Result<PdRegion> {
        pd_get_region(&self.endpoints, key)
    }

    pub fn store_address(&self, store_id: u64) -> Result<String> {
        pd_store_address(&self.endpoints, store_id)
    }

    pub fn list_stores(&self) -> Result<Vec<PdStoreInfo>> {
        pd_list_stores(&self.endpoints)
    }

    pub fn scan_regions(&self, start_key: &[u8], limit: u32) -> Result<Vec<PdRegion>> {
        pd_scan_regions(&self.endpoints, start_key, limit)
    }
}

pub fn pd_get_region(pd_endpoints: &[String], key: &[u8]) -> Result<PdRegion> {
    // Empty key → `/region/key/` 404s on PD; use regions scan instead.
    if key.is_empty() {
        let mut regions = pd_scan_regions(pd_endpoints, b"", 1)?;
        return regions.pop().ok_or_else(|| {
            Error::new(
                ERR_UNKNOWN_ERROR,
                "pd get region: empty cluster has no regions",
            )
        });
    }
    let hex = hex_encode(key);
    let mut last_err = None;
    for ep in pd_endpoints {
        let base = normalize_pd_http(ep);
        let url = format!("{base}/pd/api/v1/region/key/{hex}");
        match pd_http_get(&url).and_then(|r| r.error_for_status()) {
            Ok(resp) => {
                return resp
                    .json::<PdRegion>()
                    .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("pd region decode: {e}")));
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
    let store = pd_get_store(pd_endpoints, store_id)?;
    if store.address.is_empty() {
        return Err(Error::new(
            ERR_UNKNOWN_ERROR,
            format!("empty address for store {store_id}"),
        ));
    }
    Ok(store.address)
}

pub fn pd_get_store(pd_endpoints: &[String], store_id: u64) -> Result<PdStoreInfo> {
    let mut last_err = None;
    for ep in pd_endpoints {
        let base = normalize_pd_http(ep);
        let url = format!("{base}/pd/api/v1/store/{store_id}");
        match pd_http_get(&url).and_then(|r| r.error_for_status()) {
            Ok(resp) => {
                let wrap = resp
                    .json::<PdStoreWrap>()
                    .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("pd store decode: {e}")))?;
                return Ok(wrap.store);
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

/// `GET /pd/api/v1/stores`
pub fn pd_list_stores(pd_endpoints: &[String]) -> Result<Vec<PdStoreInfo>> {
    let mut last_err = None;
    for ep in pd_endpoints {
        let base = normalize_pd_http(ep);
        let url = format!("{base}/pd/api/v1/stores");
        match pd_http_get(&url).and_then(|r| r.error_for_status()) {
            Ok(resp) => {
                let wrap = resp
                    .json::<PdStoresWrap>()
                    .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("pd stores decode: {e}")))?;
                return Ok(wrap.stores.into_iter().map(|e| e.store).collect());
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(Error::new(
        ERR_UNKNOWN_ERROR,
        format!(
            "pd list stores failed: {}",
            last_err
                .map(|e| e.to_string())
                .unwrap_or_else(|| "no endpoints".into())
        ),
    ))
}

/// `GET /pd/api/v1/regions/key?key={hex}&limit=N` (falls back to `/regions?limit=`).
pub fn pd_scan_regions(
    pd_endpoints: &[String],
    start_key: &[u8],
    limit: u32,
) -> Result<Vec<PdRegion>> {
    let limit = limit.max(1);
    let hex = hex_encode(start_key);
    let mut last_err = None;
    for ep in pd_endpoints {
        let base = normalize_pd_http(ep);
        let url = if start_key.is_empty() {
            format!("{base}/pd/api/v1/regions?limit={limit}")
        } else {
            format!("{base}/pd/api/v1/regions/key?key={hex}&limit={limit}")
        };
        match pd_http_get(&url).and_then(|r| r.error_for_status()) {
            Ok(resp) => {
                let wrap = resp.json::<PdRegionsWrap>().map_err(|e| {
                    Error::new(ERR_UNKNOWN_ERROR, format!("pd regions decode: {e}"))
                })?;
                let _ = wrap.count;
                return Ok(wrap.regions);
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(Error::new(
        ERR_UNKNOWN_ERROR,
        format!(
            "pd scan regions failed: {}",
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
    if !h.len().is_multiple_of(2) {
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
    fn test_pd_list_stores_and_regions_if_available() {
        let pd = vec!["127.0.0.1:2379".to_string()];
        let stores = match pd_list_stores(&pd) {
            Ok(s) => s,
            Err(err) => {
                eprintln!("skip pd list stores: {err}");
                return;
            }
        };
        assert!(!stores.is_empty(), "expected at least one store");
        assert!(!stores[0].address.is_empty());

        let regions = pd_scan_regions(&pd, b"", 8).expect("regions after stores ok");
        assert!(!regions.is_empty());
        let by_empty = pd_get_region(&pd, &[]).expect("region by empty key");
        assert!(
            regions.iter().any(|r| r.id == by_empty.id),
            "empty-key region {} should appear in scan",
            by_empty.id
        );
        let by_t = pd_get_region(&pd, b"t").expect("region by key t");
        assert!(by_t.id > 0);
    }
}
