//! GCS object IO for BRIE (`gs://bucket/object`).
//!
//! Requires `SEALDB_GCS_ENDPOINT` (JSON API base, e.g. fake-gcs-server).
//! Optional `SEALDB_GCS_ACCESS_TOKEN` sends `Authorization: Bearer …`.

use sealdb_errno::{ERR_UNKNOWN_ERROR, ERR_UNSUPPORTED, Error, Result};
use std::env;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GcsUri {
    pub bucket: String,
    pub object: String,
}

impl GcsUri {
    pub fn display(&self) -> String {
        format!("gs://{}/{}", self.bucket, self.object)
    }
}

/// Parse `gs://bucket/object` (object may contain `/`).
pub fn parse_gcs_uri(uri: &str) -> Result<GcsUri> {
    let rest = uri
        .strip_prefix("gs://")
        .ok_or_else(|| Error::new(ERR_UNKNOWN_ERROR, format!("not a gs uri: {uri}")))?;
    let (bucket, object) = rest
        .split_once('/')
        .ok_or_else(|| Error::new(ERR_UNKNOWN_ERROR, format!("gs uri missing object: {uri}")))?;
    if bucket.is_empty() || object.is_empty() {
        return Err(Error::new(
            ERR_UNKNOWN_ERROR,
            format!("gs uri needs bucket and object: {uri}"),
        ));
    }
    Ok(GcsUri {
        bucket: bucket.to_string(),
        object: object.to_string(),
    })
}

fn gcs_endpoint() -> Result<String> {
    env::var("SEALDB_GCS_ENDPOINT")
        .map(|s| s.trim_end_matches('/').to_string())
        .map_err(|_| {
            Error::new(
                ERR_UNSUPPORTED,
                "gs:// requires SEALDB_GCS_ENDPOINT (GCS JSON API base URL)",
            )
        })
}

fn bearer_headers() -> Vec<(String, String)> {
    match env::var("SEALDB_GCS_ACCESS_TOKEN") {
        Ok(tok) if !tok.is_empty() => vec![("Authorization".into(), format!("Bearer {tok}"))],
        _ => Vec::new(),
    }
}

fn media_download_url(endpoint: &str, uri: &GcsUri) -> String {
    let object = urlencoding_encode(&uri.object);
    format!(
        "{endpoint}/storage/v1/b/{}/o/{object}?alt=media",
        uri.bucket
    )
}

fn media_upload_url(endpoint: &str, uri: &GcsUri) -> String {
    let name = urlencoding_encode(&uri.object);
    format!(
        "{endpoint}/upload/storage/v1/b/{}/o?uploadType=media&name={name}",
        uri.bucket
    )
}

/// Minimal path-segment encoding (RFC 3986 unreserved left alone).
fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b'/' => out.push_str("%2F"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// GET object body as UTF-8 text.
pub fn gcs_get_object(uri: &GcsUri) -> Result<String> {
    let endpoint = gcs_endpoint()?;
    let url = media_download_url(&endpoint, uri);
    let mut req = reqwest::blocking::Client::new().get(&url);
    for (k, v) in bearer_headers() {
        req = req.header(k, v);
    }
    let resp = req
        .send()
        .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("gcs GET {}: {e}", uri.display())))?;
    if !resp.status().is_success() {
        return Err(Error::new(
            ERR_UNKNOWN_ERROR,
            format!("gcs GET {}: status {}", uri.display(), resp.status()),
        ));
    }
    resp.text()
        .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("gcs GET body: {e}")))
}

/// Upload object body (JSON media upload).
pub fn gcs_put_object(uri: &GcsUri, body: &[u8]) -> Result<()> {
    let endpoint = gcs_endpoint()?;
    let url = media_upload_url(&endpoint, uri);
    let mut req = reqwest::blocking::Client::new()
        .post(&url)
        .header("Content-Type", "application/json")
        .body(body.to_vec());
    for (k, v) in bearer_headers() {
        req = req.header(k, v);
    }
    let resp = req
        .send()
        .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("gcs PUT {}: {e}", uri.display())))?;
    if !resp.status().is_success() {
        return Err(Error::new(
            ERR_UNKNOWN_ERROR,
            format!("gcs PUT {}: status {}", uri.display(), resp.status()),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_gcs_uri() {
        let u = parse_gcs_uri("gs://my-bucket/path/to/dump.json").unwrap();
        assert_eq!(u.bucket, "my-bucket");
        assert_eq!(u.object, "path/to/dump.json");
        assert_eq!(u.display(), "gs://my-bucket/path/to/dump.json");
        assert!(parse_gcs_uri("gs://onlybucket").is_err());
        assert!(parse_gcs_uri("s3://b/k").is_err());
    }

    #[test]
    fn test_media_urls() {
        let u = parse_gcs_uri("gs://b/a/b.json").unwrap();
        assert_eq!(
            media_download_url("http://127.0.0.1:4443", &u),
            "http://127.0.0.1:4443/storage/v1/b/b/o/a%2Fb.json?alt=media"
        );
        assert_eq!(
            media_upload_url("http://127.0.0.1:4443", &u),
            "http://127.0.0.1:4443/upload/storage/v1/b/b/o?uploadType=media&name=a%2Fb.json"
        );
    }

    #[test]
    fn test_gcs_get_requires_endpoint_when_unset() {
        if env::var("SEALDB_GCS_ENDPOINT").is_ok() {
            return;
        }
        let err = gcs_get_object(&parse_gcs_uri("gs://b/k").unwrap()).unwrap_err();
        assert_eq!(err.code, ERR_UNSUPPORTED);
    }
}
