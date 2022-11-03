use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_config_default() {
        let config = ServerConfig::default();
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 4000);
        assert_eq!(config.max_connections, 1000);
    }

    #[test]
    fn test_storage_config_default() {
        let config = StorageConfig::default();
        assert_eq!(config.tikv_pd_endpoints, vec!["127.0.0.1:2379"]);
    }

    #[test]
    fn test_sql_config_default() {
        let config = SqlConfig::default();
        assert_eq!(config.max_query_time, 30000);
        assert_eq!(config.max_memory_usage, 1024 * 1024 * 1024); // 1GB
    }

    #[test]
    fn test_logging_config_default() {
        let config = LogConfig::default();
        assert_eq!(config.level, "info");
        assert!(config.console);
    }

    #[test]
    fn test_config_default() {
        let config = Config::default();
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.port, 4000);
        assert_eq!(config.storage.tikv_pd_endpoints, vec!["127.0.0.1:2379"]);
        assert_eq!(config.sql.max_query_time, 30000);
        assert_eq!(config.log.level, "info");
    }

    #[test]
    fn test_config_creation() {
        let mut config = Config::default();
        config.server.host = "0.0.0.0".to_string();
        config.server.port = 3307;
        config.server.max_connections = 2000;
        config.storage.tikv_pd_endpoints =
            vec!["127.0.0.1:2379".to_string(), "127.0.0.1:2380".to_string()];
        config.sql.max_query_time = 600;
        config.sql.max_memory_usage = 209715200; // 200MB
        config.log.level = "debug".to_string();

        assert_eq!(config.server.host, "0.0.0.0");
        assert_eq!(config.server.port, 3307);
        assert_eq!(config.server.max_connections, 2000);
        assert_eq!(config.storage.tikv_pd_endpoints.len(), 2);
        assert_eq!(config.sql.max_query_time, 600);
        assert_eq!(config.sql.max_memory_usage, 209715200);
        assert_eq!(config.log.level, "debug");
    }
}
