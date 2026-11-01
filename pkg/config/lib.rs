#![forbid(unsafe_code)]

use sealdb_errno::{ERR_UNKNOWN_ERROR, Error, Result};

pub const DEFAULT_PD_ENDPOINTS: &str = "127.0.0.1:2379";
pub const ENV_STORE: &str = "SEALDB_STORE";
pub const ENV_PD_ENDPOINTS: &str = "SEALDB_PD_ENDPOINTS";

/// Storage backend selected by `--store` / `SEALDB_STORE`.
/// Completeness wave: toml config file, unistore, TiFlash.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StoreKind {
    #[default]
    Memory,
    Tikv,
}

impl StoreKind {
    pub fn parse(name: &str) -> Result<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "" | "memory" | "mem" | "mocktikv" | "unistore" => Ok(Self::Memory),
            "tikv" => Ok(Self::Tikv),
            other => Err(Error::new(
                ERR_UNKNOWN_ERROR,
                format!("unknown store {other:?}; expected memory or tikv"),
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Tikv => "tikv",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub store: StoreKind,
    pub pd_endpoints: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            store: StoreKind::Memory,
            pd_endpoints: parse_endpoints(DEFAULT_PD_ENDPOINTS),
        }
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let mut cfg = Self::default();
        if let Ok(store) = std::env::var(ENV_STORE) {
            cfg.store = StoreKind::parse(&store)?;
        }
        if let Ok(pd) = std::env::var(ENV_PD_ENDPOINTS) {
            cfg.pd_endpoints = parse_endpoints(&pd);
        }
        Ok(cfg)
    }

    pub fn with_store_name(mut self, name: Option<&str>) -> Result<Self> {
        if let Some(name) = name {
            self.store = StoreKind::parse(name)?;
        }
        Ok(self)
    }

    pub fn with_pd(mut self, pd: Option<&str>) -> Self {
        if let Some(pd) = pd {
            self.pd_endpoints = parse_endpoints(pd);
        }
        self
    }
}

pub fn parse_endpoints(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_store_kind_parse() {
        assert_eq!(StoreKind::parse("memory").unwrap(), StoreKind::Memory);
        assert_eq!(StoreKind::parse("MEM").unwrap(), StoreKind::Memory);
        assert_eq!(StoreKind::parse("tikv").unwrap(), StoreKind::Tikv);
        assert!(StoreKind::parse("rocks").is_err());
    }

    #[test]
    fn test_parse_endpoints() {
        assert_eq!(
            parse_endpoints("127.0.0.1:2379, 127.0.0.1:2378"),
            vec!["127.0.0.1:2379".to_string(), "127.0.0.1:2378".to_string()]
        );
    }
}
