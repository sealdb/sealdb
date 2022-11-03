use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub sql: SqlConfig,
    pub log: LogConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub mysql_port: u16,
    pub http_port: u16,
    pub max_connections: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    pub tikv_pd_endpoints: Vec<String>,
    pub tikv_connect_timeout: u64,
    pub tikv_request_timeout: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SqlConfig {
    pub max_query_time: u64,
    pub max_memory_usage: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogConfig {
    pub level: String,
    pub file: Option<PathBuf>,
    pub console: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            storage: StorageConfig::default(),
            sql: SqlConfig::default(),
            log: LogConfig::default(),
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 4000,
            mysql_port: 3306,
            http_port: 8080,
            max_connections: 1000,
        }
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            tikv_pd_endpoints: vec!["127.0.0.1:2379".to_string()],
            tikv_connect_timeout: 5000,
            tikv_request_timeout: 10000,
        }
    }
}

impl Default for SqlConfig {
    fn default() -> Self {
        Self {
            max_query_time: 30000,
            max_memory_usage: 1024 * 1024 * 1024, // 1GB
        }
    }
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            file: None,
            console: true,
        }
    }
} 