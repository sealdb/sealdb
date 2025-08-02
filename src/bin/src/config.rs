use serde::{Deserialize, Serialize};
use std::path::Path;
use anyhow::{Result, Context};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub max_connections: u32,
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
    pub max_memory_usage: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    pub level: String,
    pub console: bool,
    pub file: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub sql: SqlConfig,
    pub logging: LoggingConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig {
                host: "0.0.0.0".to_string(),
                port: 4000,
                max_connections: 1000,
            },
            storage: StorageConfig {
                tikv_pd_endpoints: vec!["127.0.0.1:2379".to_string()],
                tikv_connect_timeout: 6000,
                tikv_request_timeout: 12000,
            },
            sql: SqlConfig {
                max_query_time: 30000,
                max_memory_usage: 1073741824, // 1GB
            },
            logging: LoggingConfig {
                level: "info".to_string(),
                console: true,
                file: "logs/sealdb.log".to_string(),
            },
        }
    }
}

impl Config {
    /// 从文件加载配置，如果文件不存在则使用默认配置
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();

        // 检查文件是否存在
        if !path.exists() {
            eprintln!("警告: 配置文件 {} 不存在，使用默认配置", path.display());
            return Ok(Self::default());
        }

        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let extension = path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("toml")
            .to_lowercase();

        match extension.as_str() {
            "toml" => {
                let config: Config = toml::from_str(&content)
                    .with_context(|| format!("Failed to parse TOML config from {}", path.display()))?;
                Ok(config)
            }
            "json" => {
                let config: Config = serde_json::from_str(&content)
                    .with_context(|| format!("Failed to parse JSON config from {}", path.display()))?;
                Ok(config)
            }
            "yaml" | "yml" => {
                let config: Config = serde_yaml::from_str(&content)
                    .with_context(|| format!("Failed to parse YAML config from {}", path.display()))?;
                Ok(config)
            }
            _ => {
                // 默认尝试 TOML 格式
                let config: Config = toml::from_str(&content)
                    .with_context(|| format!("Failed to parse config from {}", path.display()))?;
                Ok(config)
            }
        }
    }

    /// 保存配置到文件
    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let path = path.as_ref();
        let extension = path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("toml")
            .to_lowercase();

        let content = match extension.as_str() {
            "toml" => {
                toml::to_string_pretty(self)
                    .with_context(|| "Failed to serialize config to TOML")?
            }
            "json" => {
                serde_json::to_string_pretty(self)
                    .with_context(|| "Failed to serialize config to JSON")?
            }
            "yaml" | "yml" => {
                serde_yaml::to_string(self)
                    .with_context(|| "Failed to serialize config to YAML")?
            }
            _ => {
                toml::to_string_pretty(self)
                    .with_context(|| "Failed to serialize config to TOML")?
            }
        };

        std::fs::write(path, content)
            .with_context(|| format!("Failed to write config to {}", path.display()))?;

        Ok(())
    }
}