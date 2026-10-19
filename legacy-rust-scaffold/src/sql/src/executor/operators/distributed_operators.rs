use common::Result;
use std::sync::Arc;
use std::collections::HashMap;
use async_trait::async_trait;
use tracing::{debug, info, warn};
use std::time::Duration;
use tokio::time;

use crate::executor::execution_models::QueryResult;
use crate::storage::buffer_pool::{BufferPool, PageId};
use crate::storage::memory::MemoryManager;
use crate::storage::worker_pool::WorkerPool;
use super::operator_trait::Operator;

/// 分片扫描操作符
#[derive(Debug)]
pub struct ShardScanOperator {
    pub table: String,
    pub columns: Vec<String>,
    pub buffer_pool: Arc<BufferPool>,
    pub memory_manager: Arc<MemoryManager>,
    pub shard_info: ShardInfo,
    pub shard_nodes: Vec<ShardNode>,
}

#[derive(Debug)]
pub struct ShardInfo {
    pub shard_key: String,
    pub num_shards: usize,
    pub shard_strategy: String, // Hash, Range, etc.
}

#[derive(Debug)]
pub struct ShardNode {
    pub node_id: String,
    pub host: String,
    pub port: u16,
    pub shard_ranges: Vec<(String, String)>, // (start_key, end_key)
}

impl ShardScanOperator {
    pub fn new(
        table: String,
        columns: Vec<String>,
        buffer_pool: Arc<BufferPool>,
        memory_manager: Arc<MemoryManager>,
    ) -> Self {
        Self {
            table,
            columns,
            buffer_pool,
            memory_manager,
            shard_info: ShardInfo {
                shard_key: "id".to_string(),
                num_shards: 4,
                shard_strategy: "Hash".to_string(),
            },
            shard_nodes: Vec::new(),
        }
    }

    pub fn set_shard_info(&mut self, shard_key: String, num_shards: usize, strategy: String) {
        self.shard_info = ShardInfo {
            shard_key,
            num_shards,
            shard_strategy: strategy,
        };
    }

    pub fn add_shard_node(&mut self, node_id: String, host: String, port: u16, ranges: Vec<(String, String)>) {
        self.shard_nodes.push(ShardNode {
            node_id,
            host,
            port,
            shard_ranges: ranges,
        });
    }

    async fn perform_shard_scan(&self) -> Result<Vec<Vec<String>>> {
        info!("Performing shard scan with {} shards, strategy: {}",
              self.shard_info.num_shards, self.shard_info.shard_strategy);

        // 分配工作内存
        let work_memory = self.memory_manager.allocate_work_memory(1024 * 1024)?;

        let mut all_rows = Vec::new();

        // 并行扫描所有分片节点
        let mut scan_tasks = Vec::new();
        for node in &self.shard_nodes {
            let node_id = node.node_id.clone();
            let host = node.host.clone();
            let port = node.port;
            let ranges = node.shard_ranges.clone();
            let columns = self.columns.clone();

            let task = async move {
                Self::scan_shard_node(node_id, host, port, ranges, columns)
            };
            scan_tasks.push(task);
        }

        // 等待所有分片扫描完成
        for task_result in futures::future::join_all(scan_tasks).await {
            match task_result {
                Ok(rows) => all_rows.extend(rows),
                Err(e) => {
                    warn!("Shard scan failed: {:?}", e);
                }
            }
        }

        // 释放工作内存
        self.memory_manager.free_memory(work_memory);

        Ok(all_rows)
    }

    fn scan_shard_node(
        node_id: String,
        host: String,
        port: u16,
        ranges: Vec<(String, String)>,
        columns: Vec<String>,
    ) -> Result<Vec<Vec<String>>> {
        let mut rows = Vec::new();

        // 模拟扫描分片节点的数据
        for (start_key, end_key) in ranges {
            let shard_rows = Self::fetch_shard_data(&node_id, &host, port, &start_key, &end_key, &columns)?;
            rows.extend(shard_rows);
        }

        Ok(rows)
    }

    fn fetch_shard_data(
        node_id: &str,
        host: &str,
        port: u16,
        start_key: &str,
        end_key: &str,
        columns: &[String],
    ) -> Result<Vec<Vec<String>>> {
        // 模拟从分片节点获取数据
        let mut rows = Vec::new();

        // 模拟根据键范围生成数据
        if let (Ok(start), Ok(end)) = (start_key.parse::<u32>(), end_key.parse::<u32>()) {
            for i in start..end {
                let mut row = Vec::new();
                for column in columns {
                    let value = match column.as_str() {
                        "id" => i.to_string(),
                        "name" => format!("shard_row_{}", i),
                        "value" => format!("shard_val_{}", i),
                        _ => format!("shard_col_{}", column),
                    };
                    row.push(value);
                }
                rows.push(row);
            }
        }

        Ok(rows)
    }
}

#[async_trait]
impl Operator for ShardScanOperator {
    async fn execute(&self) -> Result<QueryResult> {
        debug!("Executing shard scan operation on table: {}", self.table);

        // 添加一些模拟的分片节点
        let mut operator = self.clone();
        operator.add_shard_node(
            "node_1".to_string(),
            "192.168.1.1".to_string(),
            3306,
            vec![("0".to_string(), "25".to_string())],
        );
        operator.add_shard_node(
            "node_2".to_string(),
            "192.168.1.2".to_string(),
            3306,
            vec![("25".to_string(), "50".to_string())],
        );

        let rows = operator.perform_shard_scan().await?;

        let mut result = QueryResult::new();
        result.columns = self.columns.clone();
        result.rows = rows;
        result.affected_rows = result.rows.len() as u64;

        info!("Shard scan completed, returned {} rows", result.affected_rows);
        Ok(result)
    }
}

impl Clone for ShardScanOperator {
    fn clone(&self) -> Self {
        Self {
            table: self.table.clone(),
            columns: self.columns.clone(),
            buffer_pool: self.buffer_pool.clone(),
            memory_manager: self.memory_manager.clone(),
            shard_info: ShardInfo {
                shard_key: self.shard_info.shard_key.clone(),
                num_shards: self.shard_info.num_shards,
                shard_strategy: self.shard_info.shard_strategy.clone(),
            },
            shard_nodes: self.shard_nodes.clone(),
        }
    }
}

impl Clone for ShardNode {
    fn clone(&self) -> Self {
        Self {
            node_id: self.node_id.clone(),
            host: self.host.clone(),
            port: self.port,
            shard_ranges: self.shard_ranges.clone(),
        }
    }
}

/// 分布式聚合操作符
#[derive(Debug)]
pub struct DistributedAggOperator {
    pub input: crate::optimizer::PlanNode,
    pub group_by: Vec<String>,
    pub aggregates: Vec<String>,
    pub memory_manager: Arc<MemoryManager>,
    pub buffer_pool: Arc<BufferPool>,
    pub worker_pool: Arc<WorkerPool>,
    pub num_partitions: usize,
    pub partition_keys: Vec<String>,
}

impl DistributedAggOperator {
    pub fn new(
        input: crate::optimizer::PlanNode,
        group_by: Vec<String>,
        aggregates: Vec<String>,
        memory_manager: Arc<MemoryManager>,
        buffer_pool: Arc<BufferPool>,
        worker_pool: Arc<WorkerPool>,
    ) -> Self {
        Self {
            input,
            group_by,
            aggregates,
            memory_manager,
            buffer_pool,
            worker_pool,
            num_partitions: 4,
            partition_keys: vec!["name".to_string()],
        }
    }

    pub fn set_num_partitions(&mut self, num_partitions: usize) {
        self.num_partitions = num_partitions;
    }

    pub fn set_partition_keys(&mut self, keys: Vec<String>) {
        self.partition_keys = keys;
    }

    async fn perform_distributed_aggregation(&self, input_data: QueryResult) -> Result<Vec<Vec<String>>> {
        info!("Performing distributed aggregation with {} partitions", self.num_partitions);

        // 分配工作内存
        let work_memory = self.memory_manager.allocate_work_memory(1024 * 1024)?;

        // 将数据分区
        let partitions = self.partition_data(input_data.rows, &input_data.columns)?;

        // 并行处理每个分区
        let mut agg_tasks = Vec::new();
        for partition in partitions {
            let group_by = self.group_by.clone();
            let aggregates = self.aggregates.clone();
            let columns = input_data.columns.clone();

            let task = async move {
                Self::aggregate_partition(partition, group_by, aggregates, columns)
            };
            agg_tasks.push(task);
        }

        // 等待所有聚合任务完成
        let mut partition_results = Vec::new();
        for task_result in futures::future::join_all(agg_tasks).await {
            match task_result {
                Ok(result) => partition_results.push(result),
                Err(e) => {
                    warn!("Distributed aggregation task failed: {:?}", e);
                }
            }
        }

        // 合并分区结果
        let merged_rows = self.merge_partition_results(partition_results, &input_data.columns)?;

        // 释放工作内存
        self.memory_manager.free_memory(work_memory);

        Ok(merged_rows)
    }

    fn partition_data(&self, rows: Vec<Vec<String>>, columns: &[String]) -> Result<Vec<Vec<Vec<String>>>> {
        let mut partitions = vec![Vec::new(); self.num_partitions];

        for row in rows {
            let partition_id = self.get_partition_id(&row, columns)?;
            partitions[partition_id].push(row);
        }

        Ok(partitions)
    }

    fn get_partition_id(&self, row: &[String], columns: &[String]) -> Result<usize> {
        let partition_key = self.extract_partition_key(row, columns)?;
        let hash = self.hash_key(&partition_key);
        Ok(hash % self.num_partitions)
    }

    fn extract_partition_key(&self, row: &[String], columns: &[String]) -> Result<String> {
        let mut key_parts = Vec::new();

        for partition_key in &self.partition_keys {
            if let Some(col_index) = columns.iter().position(|c| c == partition_key) {
                if col_index < row.len() {
                    key_parts.push(row[col_index].clone());
                }
            }
        }

        Ok(key_parts.join("|"))
    }

    fn hash_key(&self, key: &str) -> usize {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        hasher.finish() as usize
    }

    fn aggregate_partition(
        partition_data: Vec<Vec<String>>,
        group_by: Vec<String>,
        aggregates: Vec<String>,
        columns: Vec<String>,
    ) -> Result<Vec<Vec<String>>> {
        let mut groups: HashMap<String, Vec<Vec<String>>> = HashMap::new();

        // 按分组键分组数据
        for row in &partition_data {
            let group_key = Self::extract_group_key_static(row, &columns, &group_by)?;
            groups.entry(group_key).or_insert_with(Vec::new).push(row.clone());
        }

        // 对每个分组进行聚合
        let mut result_rows = Vec::new();
        for (group_key, rows) in groups {
            let aggregated_row = Self::aggregate_group_static(&group_key, &rows, &columns, &aggregates)?;
            result_rows.push(aggregated_row);
        }

        Ok(result_rows)
    }

    fn extract_group_key_static(row: &[String], columns: &[String], group_by: &[String]) -> Result<String> {
        let mut key_parts = Vec::new();

        for group_col in group_by {
            if let Some(col_index) = columns.iter().position(|c| c == group_col) {
                if col_index < row.len() {
                    key_parts.push(row[col_index].clone());
                }
            }
        }

        Ok(key_parts.join("|"))
    }

    fn aggregate_group_static(
        group_key: &str,
        rows: &[Vec<String>],
        columns: &[String],
        aggregates: &[String],
    ) -> Result<Vec<String>> {
        let mut result_row = Vec::new();

        // 添加分组键
        for key_part in group_key.split('|') {
            result_row.push(key_part.to_string());
        }

        // 计算聚合函数
        for aggregate in aggregates {
            let aggregate_value = Self::calculate_aggregate_static(aggregate, rows, columns)?;
            result_row.push(aggregate_value);
        }

        Ok(result_row)
    }

    fn calculate_aggregate_static(aggregate: &str, rows: &[Vec<String>], columns: &[String]) -> Result<String> {
        if rows.is_empty() {
            return Ok("0".to_string());
        }

        match aggregate.to_lowercase().as_str() {
            "count" => Ok(rows.len().to_string()),
            "sum" => {
                let mut sum = 0.0;
                for row in rows {
                    if let Some(col_index) = columns.iter().position(|c| c == "value") {
                        if col_index < row.len() {
                            if let Ok(val) = row[col_index].parse::<f64>() {
                                sum += val;
                            }
                        }
                    }
                }
                Ok(sum.to_string())
            }
            "avg" => {
                let mut sum = 0.0;
                let mut count = 0;
                for row in rows {
                    if let Some(col_index) = columns.iter().position(|c| c == "value") {
                        if col_index < row.len() {
                            if let Ok(val) = row[col_index].parse::<f64>() {
                                sum += val;
                                count += 1;
                            }
                        }
                    }
                }
                if count > 0 {
                    Ok((sum / count as f64).to_string())
                } else {
                    Ok("0".to_string())
                }
            }
            "max" => {
                let mut max_val = f64::NEG_INFINITY;
                for row in rows {
                    if let Some(col_index) = columns.iter().position(|c| c == "value") {
                        if col_index < row.len() {
                            if let Ok(val) = row[col_index].parse::<f64>() {
                                max_val = max_val.max(val);
                            }
                        }
                    }
                }
                if max_val == f64::NEG_INFINITY {
                    Ok("0".to_string())
                } else {
                    Ok(max_val.to_string())
                }
            }
            "min" => {
                let mut min_val = f64::INFINITY;
                for row in rows {
                    if let Some(col_index) = columns.iter().position(|c| c == "value") {
                        if col_index < row.len() {
                            if let Ok(val) = row[col_index].parse::<f64>() {
                                min_val = min_val.min(val);
                            }
                        }
                    }
                }
                if min_val == f64::INFINITY {
                    Ok("0".to_string())
                } else {
                    Ok(min_val.to_string())
                }
            }
            _ => {
                warn!("Unknown aggregate function: {}", aggregate);
                Ok("0".to_string())
            }
        }
    }

    fn merge_partition_results(&self, partition_results: Vec<Vec<Vec<String>>>, columns: &[String]) -> Result<Vec<Vec<String>>> {
        let mut merged_groups: HashMap<String, Vec<Vec<String>>> = HashMap::new();

        // 合并所有分区的结果
        for partition_result in partition_results {
            for row in partition_result {
                let group_key = self.extract_group_key(&row, columns)?;
                merged_groups.entry(group_key).or_insert_with(Vec::new).push(row);
            }
        }

        // 对合并后的分组进行最终聚合
        let mut final_results = Vec::new();
        for (group_key, rows) in merged_groups {
            let aggregated_row = self.aggregate_group(&group_key, &rows, columns)?;
            final_results.push(aggregated_row);
        }

        Ok(final_results)
    }

    fn extract_group_key(&self, row: &[String], columns: &[String]) -> Result<String> {
        let mut key_parts = Vec::new();

        for group_key in &self.group_by {
            if let Some(col_index) = columns.iter().position(|c| c == group_key) {
                if col_index < row.len() {
                    key_parts.push(row[col_index].clone());
                }
            }
        }

        Ok(key_parts.join("|"))
    }

    fn aggregate_group(&self, group_key: &str, rows: &[Vec<String>], columns: &[String]) -> Result<Vec<String>> {
        let mut result_row = Vec::new();

        // 添加分组键
        for key_part in group_key.split('|') {
            result_row.push(key_part.to_string());
        }

        // 计算聚合函数
        for aggregate in &self.aggregates {
            let aggregate_value = self.calculate_aggregate(aggregate, rows, columns)?;
            result_row.push(aggregate_value);
        }

        Ok(result_row)
    }

    fn calculate_aggregate(&self, aggregate: &str, rows: &[Vec<String>], columns: &[String]) -> Result<String> {
        if rows.is_empty() {
            return Ok("0".to_string());
        }

        match aggregate.to_lowercase().as_str() {
            "count" => Ok(rows.len().to_string()),
            "sum" => {
                let mut sum = 0.0;
                for row in rows {
                    if let Some(col_index) = columns.iter().position(|c| c == "value") {
                        if col_index < row.len() {
                            if let Ok(val) = row[col_index].parse::<f64>() {
                                sum += val;
                            }
                        }
                    }
                }
                Ok(sum.to_string())
            }
            "avg" => {
                let mut sum = 0.0;
                let mut count = 0;
                for row in rows {
                    if let Some(col_index) = columns.iter().position(|c| c == "value") {
                        if col_index < row.len() {
                            if let Ok(val) = row[col_index].parse::<f64>() {
                                sum += val;
                                count += 1;
                            }
                        }
                    }
                }
                if count > 0 {
                    Ok((sum / count as f64).to_string())
                } else {
                    Ok("0".to_string())
                }
            }
            "max" => {
                let mut max_val = f64::NEG_INFINITY;
                for row in rows {
                    if let Some(col_index) = columns.iter().position(|c| c == "value") {
                        if col_index < row.len() {
                            if let Ok(val) = row[col_index].parse::<f64>() {
                                max_val = max_val.max(val);
                            }
                        }
                    }
                }
                if max_val == f64::NEG_INFINITY {
                    Ok("0".to_string())
                } else {
                    Ok(max_val.to_string())
                }
            }
            "min" => {
                let mut min_val = f64::INFINITY;
                for row in rows {
                    if let Some(col_index) = columns.iter().position(|c| c == "value") {
                        if col_index < row.len() {
                            if let Ok(val) = row[col_index].parse::<f64>() {
                                min_val = min_val.min(val);
                            }
                        }
                    }
                }
                if min_val == f64::INFINITY {
                    Ok("0".to_string())
                } else {
                    Ok(min_val.to_string())
                }
            }
            _ => {
                warn!("Unknown aggregate function: {}", aggregate);
                Ok("0".to_string())
            }
        }
    }
}

#[async_trait]
impl Operator for DistributedAggOperator {
    async fn execute(&self) -> Result<QueryResult> {
        debug!("Executing distributed aggregate operation");

        // 模拟输入数据
        let input_data = QueryResult {
            columns: vec!["id".to_string(), "name".to_string(), "value".to_string()],
            rows: vec![
                vec!["1".to_string(), "Alice".to_string(), "100".to_string()],
                vec!["2".to_string(), "Bob".to_string(), "200".to_string()],
                vec!["3".to_string(), "Alice".to_string(), "150".to_string()],
                vec!["4".to_string(), "Charlie".to_string(), "300".to_string()],
                vec!["5".to_string(), "Bob".to_string(), "250".to_string()],
            ],
            affected_rows: 5,
            last_insert_id: None,
        };

        let aggregated_rows = self.perform_distributed_aggregation(input_data).await?;

        let mut result = QueryResult::new();
        result.columns = vec!["name".to_string(), "count".to_string(), "sum".to_string(), "avg".to_string()];
        result.rows = aggregated_rows;
        result.affected_rows = result.rows.len() as u64;

        info!("Distributed aggregation completed, returned {} rows", result.affected_rows);
        Ok(result)
    }
}