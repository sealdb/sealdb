//! External BR agent IO for BRIE (`br://job-id`).
//!
//! Requires `SEALDB_BR_AGENT_ENDPOINT` (HTTP base). Backup POSTs JSON to
//! `{endpoint}/v1/backup/{job}`; restore GETs `{endpoint}/v1/restore/{job}`.

use sealdb_errno::{ERR_UNKNOWN_ERROR, ERR_UNSUPPORTED, Error, Result};
use std::env;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrAgentUri {
    pub job: String,
}

impl BrAgentUri {
    pub fn display(&self) -> String {
        format!("br://{}", self.job)
    }
}

/// Parse `br://job-id` (job may contain `/`).
pub fn parse_br_agent_uri(uri: &str) -> Result<BrAgentUri> {
    let rest = uri
        .strip_prefix("br://")
        .ok_or_else(|| Error::new(ERR_UNKNOWN_ERROR, format!("not a br uri: {uri}")))?;
    let job = rest.trim_matches('/');
    if job.is_empty() {
        return Err(Error::new(
            ERR_UNKNOWN_ERROR,
            format!("br uri needs a job id: {uri}"),
        ));
    }
    Ok(BrAgentUri {
        job: job.to_string(),
    })
}

fn agent_endpoint() -> Result<String> {
    env::var("SEALDB_BR_AGENT_ENDPOINT")
        .map(|s| s.trim_end_matches('/').to_string())
        .map_err(|_| {
            Error::new(
                ERR_UNSUPPORTED,
                "br:// requires SEALDB_BR_AGENT_ENDPOINT (HTTP base URL)",
            )
        })
}

fn backup_url(endpoint: &str, job: &str) -> String {
    format!("{endpoint}/v1/backup/{job}")
}

fn restore_url(endpoint: &str, job: &str) -> String {
    format!("{endpoint}/v1/restore/{job}")
}

/// POST backup JSON to the agent.
pub fn br_agent_put(uri: &BrAgentUri, body: &[u8]) -> Result<()> {
    let endpoint = agent_endpoint()?;
    let url = backup_url(&endpoint, &uri.job);
    let resp = reqwest::blocking::Client::new()
        .post(&url)
        .header("Content-Type", "application/json")
        .body(body.to_vec())
        .send()
        .map_err(|e| {
            Error::new(
                ERR_UNKNOWN_ERROR,
                format!("br agent PUT {}: {e}", uri.display()),
            )
        })?;
    if !resp.status().is_success() {
        return Err(Error::new(
            ERR_UNKNOWN_ERROR,
            format!("br agent PUT {}: status {}", uri.display(), resp.status()),
        ));
    }
    Ok(())
}

/// GET restore JSON from the agent.
pub fn br_agent_get(uri: &BrAgentUri) -> Result<String> {
    let endpoint = agent_endpoint()?;
    let url = restore_url(&endpoint, &uri.job);
    let resp = reqwest::blocking::Client::new()
        .get(&url)
        .send()
        .map_err(|e| {
            Error::new(
                ERR_UNKNOWN_ERROR,
                format!("br agent GET {}: {e}", uri.display()),
            )
        })?;
    if !resp.status().is_success() {
        return Err(Error::new(
            ERR_UNKNOWN_ERROR,
            format!("br agent GET {}: status {}", uri.display(), resp.status()),
        ));
    }
    resp.text()
        .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("br agent GET body: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_br_agent_uri() {
        let u = parse_br_agent_uri("br://cluster/job-1").unwrap();
        assert_eq!(u.job, "cluster/job-1");
        assert_eq!(u.display(), "br://cluster/job-1");
        assert!(parse_br_agent_uri("br://").is_err());
        assert!(parse_br_agent_uri("s3://b/k").is_err());
    }

    #[test]
    fn test_urls() {
        assert_eq!(
            backup_url("http://127.0.0.1:8080", "j1"),
            "http://127.0.0.1:8080/v1/backup/j1"
        );
        assert_eq!(restore_url("http://br", "a/b"), "http://br/v1/restore/a/b");
    }

    #[test]
    fn test_get_requires_endpoint_when_unset() {
        if env::var("SEALDB_BR_AGENT_ENDPOINT").is_ok() {
            return;
        }
        let err = br_agent_get(&parse_br_agent_uri("br://job").unwrap()).unwrap_err();
        assert_eq!(err.code, ERR_UNSUPPORTED);
    }
}
