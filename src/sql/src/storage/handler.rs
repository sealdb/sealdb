//! 存储处理器
//!
//! 作为 SQL 引擎与存储层之间的桥梁

use ::common::Result;

use crate::executor::execution_models::QueryResult;
use storage::*;
use storage::{StorageEngine, StorageEngineFactory};

/// 存储处理器
pub struct StorageHandler {
    factory: StorageEngineFactory,
    default_engine_type: EngineType,
}

impl StorageHandler {
    /// 创建新的存储处理器
    pub fn new() -> Self {
        Self {
            factory: StorageEngineFactory::new(),
            default_engine_type: EngineType::TiKV,
        }
    }

    /// 设置默认存储引擎
    pub fn set_default_engine(&mut self, engine_type: EngineType) {
        self.default_engine_type = engine_type;
    }

    /// 注册存储引擎
    pub async fn register_engine(&self, engine_type: EngineType, config: StorageConfig) -> Result<()> {
        self.factory.register_engine(engine_type, config).await
    }

    /// 获取存储引擎
    pub async fn get_engine(&self, engine_type: Option<EngineType>) -> Result<Box<dyn StorageEngine>> {
        let engine_type = engine_type.unwrap_or(self.default_engine_type);
        self.factory.get_engine(engine_type).await
    }

    /// 执行表扫描
    pub async fn scan_table(
        &self,
        table_name: &str,
        columns: &[String],
        limit: Option<u32>,
        engine_type: Option<EngineType>,
    ) -> Result<QueryResult> {
        let engine = self.get_engine(engine_type).await?;
        let context = StorageContext::default();
        let options = StorageOptions::default();

        // 构建扫描范围
        let start_key = self.build_table_prefix(table_name);
        let mut end_key = self.build_table_prefix(table_name);
        end_key.extend_from_slice(b"\xff");

        // 执行扫描
        let scan_result = engine.scan(&start_key, &end_key, limit.unwrap_or(1000), &context, &options).await
            .map_err(|e| ::common::Error::Storage(e.to_string()))?;

        // 转换为查询结果
        let rows = self.convert_key_values_to_rows(scan_result.value, columns);

        let row_count = rows.len();
        Ok(QueryResult {
            rows,
            columns: columns.to_vec(),
            affected_rows: row_count as u64,
            last_insert_id: None,
        })
    }

    /// 执行点查询
    pub async fn point_query(
        &self,
        table_name: &str,
        key_value: &str,
        engine_type: Option<EngineType>,
    ) -> Result<QueryResult> {
        let engine = self.get_engine(engine_type).await?;
        let context = StorageContext::default();
        let options = StorageOptions::default();

        // 构建主键
        let key = self.build_row_key(table_name, key_value);

        // 执行点查询
        let get_result = engine.get(&key, &context, &options).await
            .map_err(|e| ::common::Error::Storage(e.to_string()))?;

        if let Some(value) = get_result.value.as_ref() {
            // 解析值并转换为行
            let row = self.parse_value_to_row(&value);
            Ok(QueryResult {
                rows: vec![row],
                columns: vec!["value".to_string()],
                affected_rows: 1,
                last_insert_id: None,
            })
        } else {
            Ok(QueryResult {
                rows: vec![],
                columns: vec!["value".to_string()],
                affected_rows: 0,
                last_insert_id: None,
            })
        }
    }

    /// 执行插入操作
    pub async fn insert_row(
        &self,
        table_name: &str,
        key: &str,
        value: &str,
        engine_type: Option<EngineType>,
    ) -> Result<u64> {
        let engine = self.get_engine(engine_type).await?;
        let context = StorageContext::default();
        let options = StorageOptions::default();

        // 构建键
        let storage_key = self.build_row_key(table_name, key);
        let storage_value = value.as_bytes().to_vec();

        // 执行插入
        engine.put(&storage_key, &storage_value, &context, &options).await
            .map_err(|e| ::common::Error::Storage(e.to_string()))?;
        Ok(1)
    }

    /// 执行删除操作
    pub async fn delete_row(
        &self,
        table_name: &str,
        key: &str,
        engine_type: Option<EngineType>,
    ) -> Result<u64> {
        let engine = self.get_engine(engine_type).await?;
        let context = StorageContext::default();
        let options = StorageOptions::default();

        // 构建键
        let storage_key = self.build_row_key(table_name, key);

        // 执行删除
        engine.delete(&storage_key, &context, &options).await
            .map_err(|e| ::common::Error::Storage(e.to_string()))?;
        Ok(1)
    }

    /// 构建表前缀
    fn build_table_prefix(&self, table_name: &str) -> Vec<u8> {
        format!("table:{}:", table_name).into_bytes()
    }

    /// 构建行键
    fn build_row_key(&self, table_name: &str, key: &str) -> Vec<u8> {
        format!("{}:{}", table_name, key).into_bytes()
    }

    /// 将键值对转换为行数据
    fn convert_key_values_to_rows(&self, key_values: Vec<KeyValue>, _columns: &[String]) -> Vec<Vec<String>> {
        key_values.into_iter()
            .map(|(_, value)| self.parse_value_to_row(&value))
            .collect()
    }

    /// 解析值到行数据
    fn parse_value_to_row(&self, value: &[u8]) -> Vec<String> {
        vec![String::from_utf8_lossy(value).to_string()]
    }
}

impl Default for StorageHandler {
    fn default() -> Self {
        Self::new()
    }
}