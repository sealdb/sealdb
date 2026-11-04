//! HDFS BRIE via WebHDFS (`hdfs://[namenode[:port]]/path`).
//!
//! When the URI omits a host, `SEALDB_HDFS_NAMENODE` must be set to the
//! WebHDFS HTTP base (e.g. `http://nn:9870`). Host-only URIs are rewritten to
//! `http://{host}/webhdfs/v1…` (port defaults to 9870).

use sealdb_errno::{ERR_UNKNOWN_ERROR, ERR_UNSUPPORTED, Error, Result};
use std::env;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HdfsUri {
    /// Optional host:port from the URI (without scheme).
    pub namenode: Option<String>,
    /// Absolute HDFS path starting with `/`.
    pub path: String,
}

impl HdfsUri {
    pub fn display(&self) -> String {
        match &self.namenode {
            Some(nn) => format!("hdfs://{nn}{}", self.path),
            None => format!("hdfs://{}", self.path),
        }
    }
}

/// Parse `hdfs://host:port/path` or `hdfs:///path` / `hdfs://path`.
pub fn parse_hdfs_uri(uri: &str) -> Result<HdfsUri> {
    let rest = uri
        .strip_prefix("hdfs://")
        .ok_or_else(|| Error::new(ERR_UNKNOWN_ERROR, format!("not an hdfs uri: {uri}")))?;
    if rest.is_empty() {
        return Err(Error::new(
            ERR_UNKNOWN_ERROR,
            format!("empty hdfs uri: {uri}"),
        ));
    }
    // `hdfs:///abs/path` → no authority; rest begins with `/`.
    if let Some(path) = rest.strip_prefix('/') {
        let path = format!("/{path}");
        if path == "/" {
            return Err(Error::new(
                ERR_UNKNOWN_ERROR,
                format!("hdfs uri missing path: {uri}"),
            ));
        }
        return Ok(HdfsUri {
            namenode: None,
            path,
        });
    }
    let (authority, path) = rest
        .split_once('/')
        .ok_or_else(|| Error::new(ERR_UNKNOWN_ERROR, format!("hdfs uri missing path: {uri}")))?;
    if authority.is_empty() {
        return Err(Error::new(
            ERR_UNKNOWN_ERROR,
            format!("hdfs uri empty authority: {uri}"),
        ));
    }
    let path = format!("/{path}");
    if path == "/" {
        return Err(Error::new(
            ERR_UNKNOWN_ERROR,
            format!("hdfs uri missing path: {uri}"),
        ));
    }
    Ok(HdfsUri {
        namenode: Some(authority.to_string()),
        path,
    })
}

fn namenode_base(uri: &HdfsUri) -> Result<String> {
    if let Some(nn) = &uri.namenode {
        let with_scheme = if nn.starts_with("http://") || nn.starts_with("https://") {
            nn.clone()
        } else if nn.contains(':') {
            format!("http://{nn}")
        } else {
            format!("http://{nn}:9870")
        };
        return Ok(with_scheme.trim_end_matches('/').to_string());
    }
    env::var("SEALDB_HDFS_NAMENODE")
        .map(|s| s.trim_end_matches('/').to_string())
        .map_err(|_| {
            Error::new(
                ERR_UNSUPPORTED,
                "hdfs:// without host requires SEALDB_HDFS_NAMENODE (WebHDFS HTTP base)",
            )
        })
}

fn webhdfs_url(base: &str, path: &str, op: &str, extra: &str) -> String {
    let extra = if extra.is_empty() {
        String::new()
    } else {
        format!("&{extra}")
    };
    format!("{base}/webhdfs/v1{path}?op={op}{extra}")
}

/// OPEN file contents as UTF-8 (follows redirect).
pub fn hdfs_get_file(uri: &HdfsUri) -> Result<String> {
    let base = namenode_base(uri)?;
    let url = webhdfs_url(&base, &uri.path, "OPEN", "");
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("hdfs client: {e}")))?;
    let resp = client.get(&url).send().map_err(|e| {
        Error::new(
            ERR_UNKNOWN_ERROR,
            format!("hdfs OPEN {}: {e}", uri.display()),
        )
    })?;
    if !resp.status().is_success() {
        return Err(Error::new(
            ERR_UNKNOWN_ERROR,
            format!("hdfs OPEN {}: status {}", uri.display(), resp.status()),
        ));
    }
    resp.text()
        .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("hdfs OPEN body: {e}")))
}

/// CREATE (overwrite) file via two-step WebHDFS CREATE.
pub fn hdfs_put_file(uri: &HdfsUri, body: &[u8]) -> Result<()> {
    let base = namenode_base(uri)?;
    let url = webhdfs_url(&base, &uri.path, "CREATE", "overwrite=true&noredirect=true");
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("hdfs client: {e}")))?;
    let resp = client.put(&url).send().map_err(|e| {
        Error::new(
            ERR_UNKNOWN_ERROR,
            format!("hdfs CREATE {}: {e}", uri.display()),
        )
    })?;
    let status = resp.status();
    // Namenode returns 307 with Location, or some gateways accept body on first hop.
    let location = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if let Some(loc) = location {
        let put = client
            .put(&loc)
            .header("Content-Type", "application/octet-stream")
            .body(body.to_vec())
            .send()
            .map_err(|e| {
                Error::new(
                    ERR_UNKNOWN_ERROR,
                    format!("hdfs CREATE data {}: {e}", uri.display()),
                )
            })?;
        if !put.status().is_success() {
            return Err(Error::new(
                ERR_UNKNOWN_ERROR,
                format!(
                    "hdfs CREATE data {}: status {}",
                    uri.display(),
                    put.status()
                ),
            ));
        }
        return Ok(());
    }
    if status.is_success() {
        // Uncommon: single-hop create accepted empty; retry with body.
        let put = client
            .put(&url)
            .header("Content-Type", "application/octet-stream")
            .body(body.to_vec())
            .send()
            .map_err(|e| {
                Error::new(
                    ERR_UNKNOWN_ERROR,
                    format!("hdfs CREATE body {}: {e}", uri.display()),
                )
            })?;
        if !put.status().is_success() {
            return Err(Error::new(
                ERR_UNKNOWN_ERROR,
                format!(
                    "hdfs CREATE body {}: status {}",
                    uri.display(),
                    put.status()
                ),
            ));
        }
        return Ok(());
    }
    Err(Error::new(
        ERR_UNKNOWN_ERROR,
        format!(
            "hdfs CREATE {}: status {status} (no Location)",
            uri.display()
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hdfs_uri_with_host() {
        let u = parse_hdfs_uri("hdfs://nn:9870/user/seal/dump.json").unwrap();
        assert_eq!(u.namenode.as_deref(), Some("nn:9870"));
        assert_eq!(u.path, "/user/seal/dump.json");
        assert_eq!(u.display(), "hdfs://nn:9870/user/seal/dump.json");
    }

    #[test]
    fn test_parse_hdfs_uri_default_nn() {
        let u = parse_hdfs_uri("hdfs:///user/seal/dump.json").unwrap();
        assert!(u.namenode.is_none());
        assert_eq!(u.path, "/user/seal/dump.json");
        assert_eq!(u.display(), "hdfs:///user/seal/dump.json");
    }

    #[test]
    fn test_webhdfs_url() {
        assert_eq!(
            webhdfs_url("http://nn:9870", "/a/b.json", "OPEN", ""),
            "http://nn:9870/webhdfs/v1/a/b.json?op=OPEN"
        );
    }

    #[test]
    fn test_hdfs_get_requires_namenode_when_unset() {
        if env::var("SEALDB_HDFS_NAMENODE").is_ok() {
            return;
        }
        let u = parse_hdfs_uri("hdfs:///tmp/x.json").unwrap();
        let err = hdfs_get_file(&u).unwrap_err();
        assert_eq!(err.code, ERR_UNSUPPORTED);
    }
}
