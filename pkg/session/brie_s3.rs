//! S3-compatible object IO for BRIE (`s3://bucket/key`).
//!
//! Path-style URLs against `SEALDB_S3_ENDPOINT` (required). Optional
//! `SEALDB_S3_ACCESS_KEY` / `SEALDB_S3_SECRET_KEY` (or `AWS_*`) enable SigV4.
//! `SEALDB_S3_REGION` defaults to `us-east-1`.

use hmac::{Hmac, Mac};
use sealdb_errno::{ERR_UNKNOWN_ERROR, ERR_UNSUPPORTED, Error, Result};
use sha2::{Digest, Sha256};
use std::env;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct S3Uri {
    pub bucket: String,
    pub key: String,
}

impl S3Uri {
    pub fn display(&self) -> String {
        format!("s3://{}/{}", self.bucket, self.key)
    }
}

/// Parse `s3://bucket/key` (key may contain `/`).
pub fn parse_s3_uri(uri: &str) -> Result<S3Uri> {
    let rest = uri
        .strip_prefix("s3://")
        .ok_or_else(|| Error::new(ERR_UNKNOWN_ERROR, format!("not an s3 uri: {uri}")))?;
    let (bucket, key) = rest
        .split_once('/')
        .ok_or_else(|| Error::new(ERR_UNKNOWN_ERROR, format!("s3 uri missing key: {uri}")))?;
    if bucket.is_empty() || key.is_empty() {
        return Err(Error::new(
            ERR_UNKNOWN_ERROR,
            format!("s3 uri needs bucket and key: {uri}"),
        ));
    }
    Ok(S3Uri {
        bucket: bucket.to_string(),
        key: key.to_string(),
    })
}

fn s3_endpoint() -> Result<String> {
    env::var("SEALDB_S3_ENDPOINT")
        .map(|s| s.trim_end_matches('/').to_string())
        .map_err(|_| {
            Error::new(
                ERR_UNSUPPORTED,
                "s3:// requires SEALDB_S3_ENDPOINT (S3-compatible path-style base URL)",
            )
        })
}

fn s3_region() -> String {
    env::var("SEALDB_S3_REGION").unwrap_or_else(|_| "us-east-1".to_string())
}

fn s3_credentials() -> Option<(String, String)> {
    let access = env::var("SEALDB_S3_ACCESS_KEY")
        .or_else(|_| env::var("AWS_ACCESS_KEY_ID"))
        .ok()?;
    let secret = env::var("SEALDB_S3_SECRET_KEY")
        .or_else(|_| env::var("AWS_SECRET_ACCESS_KEY"))
        .ok()?;
    if access.is_empty() || secret.is_empty() {
        None
    } else {
        Some((access, secret))
    }
}

fn object_url(endpoint: &str, uri: &S3Uri) -> String {
    format!("{endpoint}/{}/{}", uri.bucket, uri.key)
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac key");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

fn amz_timestamps() -> Result<(String, String)> {
    let now = time::OffsetDateTime::now_utc();
    let date = format!(
        "{:04}{:02}{:02}",
        now.year(),
        u8::from(now.month()),
        now.day()
    );
    let datetime = format!(
        "{date}T{:02}{:02}{:02}Z",
        now.hour(),
        now.minute(),
        now.second()
    );
    Ok((date, datetime))
}

fn host_header(endpoint: &str) -> Result<String> {
    let without = endpoint
        .strip_prefix("https://")
        .or_else(|| endpoint.strip_prefix("http://"))
        .unwrap_or(endpoint);
    Ok(without.split('/').next().unwrap_or(without).to_string())
}

fn sign_headers(
    method: &str,
    endpoint: &str,
    uri: &S3Uri,
    payload: &[u8],
    access: &str,
    secret: &str,
) -> Result<Vec<(String, String)>> {
    let region = s3_region();
    let (date, datetime) = amz_timestamps()?;
    let host = host_header(endpoint)?;
    let canonical_uri = format!("/{}/{}", uri.bucket, uri.key);
    let payload_hash = sha256_hex(payload);
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical_headers =
        format!("host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{datetime}\n");
    let canonical_request = format!(
        "{method}\n{canonical_uri}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );
    let credential_scope = format!("{date}/{region}/s3/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{datetime}\n{credential_scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let key = signing_key(secret, &date, &region, "s3");
    let signature = hex::encode(hmac_sha256(&key, string_to_sign.as_bytes()));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={access}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}"
    );
    Ok(vec![
        ("Host".into(), host),
        ("x-amz-content-sha256".into(), payload_hash),
        ("x-amz-date".into(), datetime),
        ("Authorization".into(), authorization),
    ])
}

/// GET object body as UTF-8 text.
pub fn s3_get_object(uri: &S3Uri) -> Result<String> {
    let endpoint = s3_endpoint()?;
    let url = object_url(&endpoint, uri);
    let mut req = reqwest::blocking::Client::new().get(&url);
    if let Some((access, secret)) = s3_credentials() {
        for (k, v) in sign_headers("GET", &endpoint, uri, b"", &access, &secret)? {
            req = req.header(k, v);
        }
    }
    let resp = req
        .send()
        .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("s3 GET {}: {e}", uri.display())))?;
    if !resp.status().is_success() {
        return Err(Error::new(
            ERR_UNKNOWN_ERROR,
            format!("s3 GET {}: status {}", uri.display(), resp.status()),
        ));
    }
    resp.text()
        .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("s3 GET body: {e}")))
}

/// PUT object body.
pub fn s3_put_object(uri: &S3Uri, body: &[u8]) -> Result<()> {
    let endpoint = s3_endpoint()?;
    let url = object_url(&endpoint, uri);
    let mut req = reqwest::blocking::Client::new()
        .put(&url)
        .header("Content-Type", "application/json")
        .body(body.to_vec());
    if let Some((access, secret)) = s3_credentials() {
        for (k, v) in sign_headers("PUT", &endpoint, uri, body, &access, &secret)? {
            req = req.header(k, v);
        }
    }
    let resp = req
        .send()
        .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("s3 PUT {}: {e}", uri.display())))?;
    if !resp.status().is_success() {
        return Err(Error::new(
            ERR_UNKNOWN_ERROR,
            format!("s3 PUT {}: status {}", uri.display(), resp.status()),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_s3_uri() {
        let u = parse_s3_uri("s3://my-bucket/path/to/dump.json").unwrap();
        assert_eq!(u.bucket, "my-bucket");
        assert_eq!(u.key, "path/to/dump.json");
        assert_eq!(u.display(), "s3://my-bucket/path/to/dump.json");
        assert!(parse_s3_uri("s3://onlybucket").is_err());
        assert!(parse_s3_uri("http://x").is_err());
    }

    #[test]
    fn test_object_url_path_style() {
        let u = parse_s3_uri("s3://b/k.json").unwrap();
        assert_eq!(
            object_url("http://127.0.0.1:9000", &u),
            "http://127.0.0.1:9000/b/k.json"
        );
    }

    #[test]
    fn test_s3_get_requires_endpoint_when_unset() {
        if env::var("SEALDB_S3_ENDPOINT").is_ok() {
            return;
        }
        let err = s3_get_object(&parse_s3_uri("s3://b/k").unwrap()).unwrap_err();
        assert_eq!(err.code, ERR_UNSUPPORTED);
    }

    #[test]
    fn test_sign_headers_smoke() {
        let u = parse_s3_uri("s3://bucket/obj.json").unwrap();
        let headers =
            sign_headers("PUT", "http://127.0.0.1:9000", &u, b"{}", "AKIA", "secret").unwrap();
        assert!(headers.iter().any(
            |(k, v)| k == "Authorization" && v.starts_with("AWS4-HMAC-SHA256 Credential=AKIA/")
        ));
        assert!(headers.iter().any(|(k, _)| k == "x-amz-date"));
    }
}
