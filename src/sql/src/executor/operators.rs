use common::Result;
use std::sync::Arc;
use std::collections::HashMap;
use async_trait::async_trait;
use tracing::{debug, info, warn};
use std::sync::mpsc::{channel, Sender, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use crate::executor::execution_models::QueryResult;
use crate::storage::buffer_pool::{BufferPool, PageId};
use crate::storage::memory::MemoryManager;
use crate::storage::worker_pool::{WorkerPool, TaskInfo, TaskPriority, TaskType};

/// 基础操作符 trait
#[async_trait]
pub trait Operator {
    async fn execute(&self) -> Result<QueryResult>;
}

/// 扫描操作符
#[derive(Debug)]
pub struct ScanOperator {
    pub table: String,
    pub columns: Vec<String>,
    pub buffer_pool: Arc<BufferPool>,
    pub memory_manager: Arc<MemoryManager>,
}

impl ScanOperator {
    pub fn new(table: String, columns: Vec<String>, buffer_pool: Arc<BufferPool>, memory_manager: Arc<MemoryManager>) -> Self {
        Self { table, columns, buffer_pool, memory_manager }
    }

    /// 执行表扫描
    async fn scan_table(&self) -> Result<Vec<Vec<String>>> {
        info!("Scanning table: {} with columns: {:?}", self.table, self.columns);

        // 分配工作内存用于存储扫描结果
        let work_memory = self.memory_manager.allocate_work_memory(1024 * 1024)?; // 1MB

        // 模拟从磁盘读取数据
        let mut rows = Vec::new();

        // 模拟读取多个页面
        for page_id in 0..10 {
            let page = self.buffer_pool.get_buffer(PageId(page_id))?;

            // 模拟从页面中解析行数据
            let page_rows = self.parse_page_data(&page.data)?;
            rows.extend(page_rows);
        }

        // 释放工作内存
        self.memory_manager.free_memory(work_memory);

        Ok(rows)
    }

    /// 解析页面数据
    fn parse_page_data(&self, data: &[u8]) -> Result<Vec<Vec<String>>> {
        // 模拟解析页面数据为行
        let mut rows = Vec::new();

        // 假设每行数据大小为100字节
        let row_size = 100;
        let num_rows = data.len() / row_size;

        for i in 0..num_rows {
            let start = i * row_size;
            let end = start + row_size;
            if end <= data.len() {
                let row_data = &data[start..end];
                let row = self.parse_row_data(row_data)?;
                rows.push(row);
            }
        }

        Ok(rows)
    }

    /// 解析行数据
    fn parse_row_data(&self, data: &[u8]) -> Result<Vec<String>> {
        // 模拟解析行数据为列值
        let mut row = Vec::new();

        for column in &self.columns {
            // 模拟根据列名生成值
            let value = match column.as_str() {
                "id" => format!("{}", data.len()),
                "name" => format!("row_{}", data.len()),
                "value" => format!("val_{}", data.len()),
                _ => format!("col_{}", column),
            };
            row.push(value);
        }

        Ok(row)
    }
}

#[async_trait]
impl Operator for ScanOperator {
    async fn execute(&self) -> Result<QueryResult> {
        debug!("Executing table scan on table: {}", self.table);

        let rows = self.scan_table().await?;

        let mut result = QueryResult::new();
        result.columns = self.columns.clone();
        result.rows = rows;
        result.affected_rows = result.rows.len() as u64;

        info!("Table scan completed, returned {} rows", result.affected_rows);
        Ok(result)
    }
}

/// 索引扫描操作符
#[derive(Debug)]
pub struct IndexScanOperator {
    pub table: String,
    pub index: String,
    pub columns: Vec<String>,
    pub buffer_pool: Arc<BufferPool>,
    pub memory_manager: Arc<MemoryManager>,
    pub index_conditions: HashMap<String, String>, // 索引条件
}

impl IndexScanOperator {
    pub fn new(
        table: String,
        index: String,
        columns: Vec<String>,
        buffer_pool: Arc<BufferPool>,
        memory_manager: Arc<MemoryManager>
    ) -> Self {
        Self {
            table,
            index,
            columns,
            buffer_pool,
            memory_manager,
            index_conditions: HashMap::new(),
        }
    }

    /// 设置索引条件
    pub fn set_index_condition(&mut self, column: String, value: String) {
        self.index_conditions.insert(column, value);
    }

    /// 执行索引扫描
    async fn scan_index(&self) -> Result<Vec<Vec<String>>> {
        info!("Scanning index: {} on table: {} with conditions: {:?}",
              self.index, self.table, self.index_conditions);

        // 分配工作内存
        let work_memory = self.memory_manager.allocate_work_memory(512 * 1024)?; // 512KB

        // 模拟索引查找
        let mut rows = Vec::new();

        // 根据索引条件过滤数据
        for page_id in 0..5 {
            let page = self.buffer_pool.get_buffer(PageId(page_id))?;
            let page_rows = self.parse_page_data(&page.data)?;

            // 应用索引条件过滤
            let filtered_rows = self.apply_index_conditions(page_rows)?;
            rows.extend(filtered_rows);
        }

        // 释放工作内存
        self.memory_manager.free_memory(work_memory);

        Ok(rows)
    }

    /// 应用索引条件
    fn apply_index_conditions(&self, rows: Vec<Vec<String>>) -> Result<Vec<Vec<String>>> {
        if self.index_conditions.is_empty() {
            return Ok(rows);
        }

        let mut filtered_rows = Vec::new();

        for row in rows {
            let mut matches = true;

            for (column, expected_value) in &self.index_conditions {
                if let Some(column_index) = self.columns.iter().position(|c| c == column) {
                    if column_index < row.len() && row[column_index] != *expected_value {
                        matches = false;
                        break;
                    }
                }
            }

            if matches {
                filtered_rows.push(row);
            }
        }

        Ok(filtered_rows)
    }

    /// 解析页面数据（复用表扫描的逻辑）
    fn parse_page_data(&self, data: &[u8]) -> Result<Vec<Vec<String>>> {
        let mut rows = Vec::new();
        let row_size = 100;
        let num_rows = data.len() / row_size;

        for i in 0..num_rows {
            let start = i * row_size;
            let end = start + row_size;
            if end <= data.len() {
                let row_data = &data[start..end];
                let row = self.parse_row_data(row_data)?;
                rows.push(row);
            }
        }

        Ok(rows)
    }

    /// 解析行数据（复用表扫描的逻辑）
    fn parse_row_data(&self, data: &[u8]) -> Result<Vec<String>> {
        let mut row = Vec::new();

        for column in &self.columns {
            let value = match column.as_str() {
                "id" => format!("{}", data.len()),
                "name" => format!("row_{}", data.len()),
                "value" => format!("val_{}", data.len()),
                _ => format!("col_{}", column),
            };
            row.push(value);
        }

        Ok(row)
    }
}

#[async_trait]
impl Operator for IndexScanOperator {
    async fn execute(&self) -> Result<QueryResult> {
        debug!("Executing index scan on index: {} of table: {}", self.index, self.table);

        let rows = self.scan_index().await?;

        let mut result = QueryResult::new();
        result.columns = self.columns.clone();
        result.rows = rows;
        result.affected_rows = result.rows.len() as u64;

        info!("Index scan completed, returned {} rows", result.affected_rows);
        Ok(result)
    }
}

/// 连接操作符
#[derive(Debug)]
pub struct JoinOperator {
    pub left: crate::optimizer::PlanNode,
    pub right: crate::optimizer::PlanNode,
    pub join_type: String,
    pub condition: String,
    pub memory_manager: Arc<MemoryManager>,
}

impl JoinOperator {
    pub fn new(
        left: crate::optimizer::PlanNode,
        right: crate::optimizer::PlanNode,
        join_type: String,
        condition: String,
        memory_manager: Arc<MemoryManager>,
    ) -> Self {
        Self {
            left,
            right,
            join_type,
            condition,
            memory_manager,
        }
    }

    /// 执行连接操作
    async fn perform_join(&self, left_data: QueryResult, right_data: QueryResult) -> Result<Vec<Vec<String>>> {
        info!("Performing {} join with condition: {}", self.join_type, self.condition);

        // 分配共享内存用于连接操作
        let shared_memory = self.memory_manager.allocate_shared_memory(2 * 1024 * 1024)?; // 2MB

        let joined_rows = match self.join_type.as_str() {
            "INNER" => {
                self.inner_join(&left_data, &right_data).await?
            }
            "LEFT" => {
                self.left_join(&left_data, &right_data).await?
            }
            "RIGHT" => {
                self.right_join(&left_data, &right_data).await?
            }
            "FULL" => {
                self.full_join(&left_data, &right_data).await?
            }
            _ => {
                warn!("Unknown join type: {}, using INNER join", self.join_type);
                self.inner_join(&left_data, &right_data).await?
            }
        };

        // 释放共享内存
        self.memory_manager.free_memory(shared_memory);

        Ok(joined_rows)
    }

    /// 内连接
    async fn inner_join(&self, left: &QueryResult, right: &QueryResult) -> Result<Vec<Vec<String>>> {
        let mut joined_rows = Vec::new();

        for left_row in &left.rows {
            for right_row in &right.rows {
                if self.matches_join_condition(left_row, right_row)? {
                    let mut joined_row = left_row.clone();
                    joined_row.extend(right_row.clone());
                    joined_rows.push(joined_row);
                }
            }
        }

        Ok(joined_rows)
    }

    /// 左连接
    async fn left_join(&self, left: &QueryResult, right: &QueryResult) -> Result<Vec<Vec<String>>> {
        let mut joined_rows = Vec::new();

        for left_row in &left.rows {
            let mut matched = false;

            for right_row in &right.rows {
                if self.matches_join_condition(left_row, right_row)? {
                    let mut joined_row = left_row.clone();
                    joined_row.extend(right_row.clone());
                    joined_rows.push(joined_row);
                    matched = true;
                }
            }

            if !matched {
                let mut joined_row = left_row.clone();
                // 添加空值
                for _ in 0..right.columns.len() {
                    joined_row.push("NULL".to_string());
                }
                joined_rows.push(joined_row);
            }
        }

        Ok(joined_rows)
    }

    /// 右连接
    async fn right_join(&self, left: &QueryResult, right: &QueryResult) -> Result<Vec<Vec<String>>> {
        let mut joined_rows = Vec::new();

        for right_row in &right.rows {
            let mut matched = false;

            for left_row in &left.rows {
                if self.matches_join_condition(left_row, right_row)? {
                    let mut joined_row = left_row.clone();
                    joined_row.extend(right_row.clone());
                    joined_rows.push(joined_row);
                    matched = true;
                }
            }

            if !matched {
                let mut joined_row = Vec::new();
                // 添加空值
                for _ in 0..left.columns.len() {
                    joined_row.push("NULL".to_string());
                }
                joined_row.extend(right_row.clone());
                joined_rows.push(joined_row);
            }
        }

        Ok(joined_rows)
    }

    /// 全连接
    async fn full_join(&self, left: &QueryResult, right: &QueryResult) -> Result<Vec<Vec<String>>> {
        let mut joined_rows = Vec::new();

        // 执行左连接
        let left_joined = self.left_join(left, right).await?;
        joined_rows.extend(left_joined);

        // 执行右连接，但只添加未匹配的右表行
        for right_row in &right.rows {
            let mut matched = false;

            for left_row in &left.rows {
                if self.matches_join_condition(left_row, right_row)? {
                    matched = true;
                    break;
                }
            }

            if !matched {
                let mut joined_row = Vec::new();
                // 添加空值
                for _ in 0..left.columns.len() {
                    joined_row.push("NULL".to_string());
                }
                joined_row.extend(right_row.clone());
                joined_rows.push(joined_row);
            }
        }

        Ok(joined_rows)
    }

    /// 检查连接条件是否匹配
    fn matches_join_condition(&self, left_row: &[String], right_row: &[String]) -> Result<bool> {
        // 简化的连接条件检查
        // 在实际实现中，需要解析连接条件并执行相应的比较
        if self.condition.contains("=") {
            // 模拟等值连接
            if !left_row.is_empty() && !right_row.is_empty() {
                return Ok(left_row[0] == right_row[0]);
            }
        }

        // 默认返回 true（用于演示）
        Ok(true)
    }
}

#[async_trait]
impl Operator for JoinOperator {
    async fn execute(&self) -> Result<QueryResult> {
        debug!("Executing {} join", self.join_type);

        // 这里需要实际执行左右子计划
        // 为了演示，我们创建模拟的查询结果
        let left_result = QueryResult {
            columns: vec!["id".to_string(), "name".to_string()],
            rows: vec![
                vec!["1".to_string(), "Alice".to_string()],
                vec!["2".to_string(), "Bob".to_string()],
            ],
            affected_rows: 2,
            last_insert_id: None,
        };

        let right_result = QueryResult {
            columns: vec!["id".to_string(), "value".to_string()],
            rows: vec![
                vec!["1".to_string(), "100".to_string()],
                vec!["2".to_string(), "200".to_string()],
            ],
            affected_rows: 2,
            last_insert_id: None,
        };

        let joined_rows = self.perform_join(left_result, right_result).await?;

        let mut result = QueryResult::new();
        result.columns = vec!["id".to_string(), "name".to_string(), "id".to_string(), "value".to_string()];
        result.rows = joined_rows;
        result.affected_rows = result.rows.len() as u64;

        info!("Join completed, returned {} rows", result.affected_rows);
        Ok(result)
    }
}

/// 聚合操作符
#[derive(Debug)]
pub struct AggregateOperator {
    pub input: crate::optimizer::PlanNode,
    pub group_by: Vec<String>,
    pub aggregates: Vec<String>,
    pub memory_manager: Arc<MemoryManager>,
}

impl AggregateOperator {
    pub fn new(
        input: crate::optimizer::PlanNode,
        group_by: Vec<String>,
        aggregates: Vec<String>,
        memory_manager: Arc<MemoryManager>,
    ) -> Self {
        Self {
            input,
            group_by,
            aggregates,
            memory_manager,
        }
    }

    /// 执行聚合操作
    async fn perform_aggregation(&self, input_data: QueryResult) -> Result<Vec<Vec<String>>> {
        info!("Performing aggregation with group by: {:?}, aggregates: {:?}",
              self.group_by, self.aggregates);

        // 分配工作内存用于聚合计算
        let work_memory = self.memory_manager.allocate_work_memory(1024 * 1024)?; // 1MB

        let mut grouped_data: HashMap<String, Vec<Vec<String>>> = HashMap::new();

        // 分组数据
        for row in &input_data.rows {
            let group_key = self.create_group_key(row, &input_data.columns)?;
            grouped_data.entry(group_key).or_insert_with(Vec::new).push(row.clone());
        }

        // 对每个组执行聚合
        let mut aggregated_rows = Vec::new();
        for (group_key, group_rows) in grouped_data {
            let aggregated_row = self.aggregate_group(&group_key, &group_rows, &input_data.columns)?;
            aggregated_rows.push(aggregated_row);
        }

        // 释放工作内存
        self.memory_manager.free_memory(work_memory);

        Ok(aggregated_rows)
    }

    /// 创建分组键
    fn create_group_key(&self, row: &[String], columns: &[String]) -> Result<String> {
        let mut key_parts = Vec::new();

        for group_col in &self.group_by {
            if let Some(col_index) = columns.iter().position(|c| c == group_col) {
                if col_index < row.len() {
                    key_parts.push(row[col_index].clone());
                }
            }
        }

        Ok(key_parts.join("|"))
    }

    /// 聚合一个组的数据
    fn aggregate_group(&self, group_key: &str, rows: &[Vec<String>], columns: &[String]) -> Result<Vec<String>> {
        let mut aggregated_row = Vec::new();

        // 添加分组列的值
        let group_parts: Vec<&str> = group_key.split('|').collect();
        aggregated_row.extend(group_parts.iter().map(|s| s.to_string()));

        // 计算聚合值
        for aggregate in &self.aggregates {
            let value = self.calculate_aggregate(aggregate, rows, columns)?;
            aggregated_row.push(value);
        }

        Ok(aggregated_row)
    }

    /// 计算聚合值
    fn calculate_aggregate(&self, aggregate: &str, rows: &[Vec<String>], columns: &[String]) -> Result<String> {
        if aggregate.starts_with("COUNT") {
            return Ok(rows.len().to_string());
        }

        if aggregate.starts_with("SUM") || aggregate.starts_with("AVG") {
            // 提取列名
            let column_name = aggregate
                .trim_start_matches("SUM(")
                .trim_start_matches("AVG(")
                .trim_end_matches(")");

            if let Some(col_index) = columns.iter().position(|c| c == column_name) {
                let mut sum = 0.0;
                let mut count = 0;

                for row in rows {
                    if col_index < row.len() {
                        if let Ok(value) = row[col_index].parse::<f64>() {
                            sum += value;
                            count += 1;
                        }
                    }
                }

                if aggregate.starts_with("SUM") {
                    return Ok(sum.to_string());
                } else if aggregate.starts_with("AVG") {
                    if count > 0 {
                        return Ok((sum / count as f64).to_string());
                    }
                }
            }
        }

        if aggregate.starts_with("MAX") || aggregate.starts_with("MIN") {
            let column_name = aggregate
                .trim_start_matches("MAX(")
                .trim_start_matches("MIN(")
                .trim_end_matches(")");

            if let Some(col_index) = columns.iter().position(|c| c == column_name) {
                let mut values = Vec::new();

                for row in rows {
                    if col_index < row.len() {
                        values.push(row[col_index].clone());
                    }
                }

                if !values.is_empty() {
                    if aggregate.starts_with("MAX") {
                        values.sort();
                        return Ok(values.last().unwrap().clone());
                    } else if aggregate.starts_with("MIN") {
                        values.sort();
                        return Ok(values.first().unwrap().clone());
                    }
                }
            }
        }

        // 默认返回第一个值
        if !rows.is_empty() && !rows[0].is_empty() {
            Ok(rows[0][0].clone())
        } else {
            Ok("0".to_string())
        }
    }
}

#[async_trait]
impl Operator for AggregateOperator {
    async fn execute(&self) -> Result<QueryResult> {
        debug!("Executing aggregation with group by: {:?}", self.group_by);

        // 模拟输入数据
        let input_data = QueryResult {
            columns: vec!["id".to_string(), "name".to_string(), "value".to_string()],
            rows: vec![
                vec!["1".to_string(), "Alice".to_string(), "100".to_string()],
                vec!["2".to_string(), "Bob".to_string(), "200".to_string()],
                vec!["3".to_string(), "Alice".to_string(), "150".to_string()],
            ],
            affected_rows: 3,
            last_insert_id: None,
        };

        let aggregated_rows = self.perform_aggregation(input_data).await?;

        let mut result = QueryResult::new();
        result.columns = self.group_by.clone();
        result.columns.extend(self.aggregates.clone());
        result.rows = aggregated_rows;
        result.affected_rows = result.rows.len() as u64;

        info!("Aggregation completed, returned {} rows", result.affected_rows);
        Ok(result)
    }
}

/// 排序操作符
#[derive(Debug)]
pub struct SortOperator {
    pub input: crate::optimizer::PlanNode,
    pub order_by: Vec<String>,
    pub memory_manager: Arc<MemoryManager>,
}

impl SortOperator {
    pub fn new(input: crate::optimizer::PlanNode, order_by: Vec<String>, memory_manager: Arc<MemoryManager>) -> Self {
        Self { input, order_by, memory_manager }
    }

    /// 执行排序操作
    async fn perform_sort(&self, input_data: QueryResult) -> Result<Vec<Vec<String>>> {
        info!("Performing sort by: {:?}", self.order_by);

        // 分配工作内存用于排序
        let work_memory = self.memory_manager.allocate_work_memory(1024 * 1024)?; // 1MB

        let mut sorted_rows = input_data.rows;

        // 根据指定的列进行排序
        sorted_rows.sort_by(|a, b| {
            for order_col in &self.order_by {
                if let Some(col_index) = input_data.columns.iter().position(|c| c == order_col) {
                    if col_index < a.len() && col_index < b.len() {
                        let comparison = a[col_index].cmp(&b[col_index]);
                        if comparison != std::cmp::Ordering::Equal {
                            return comparison;
                        }
                    }
                }
            }
            std::cmp::Ordering::Equal
        });

        // 释放工作内存
        self.memory_manager.free_memory(work_memory);

        Ok(sorted_rows)
    }
}

#[async_trait]
impl Operator for SortOperator {
    async fn execute(&self) -> Result<QueryResult> {
        debug!("Executing sort by: {:?}", self.order_by);

        // 模拟输入数据
        let input_data = QueryResult {
            columns: vec!["id".to_string(), "name".to_string(), "value".to_string()],
            rows: vec![
                vec!["3".to_string(), "Charlie".to_string(), "300".to_string()],
                vec!["1".to_string(), "Alice".to_string(), "100".to_string()],
                vec!["2".to_string(), "Bob".to_string(), "200".to_string()],
            ],
            affected_rows: 3,
            last_insert_id: None,
        };

        let sorted_rows = self.perform_sort(input_data).await?;

        let mut result = QueryResult::new();
        result.columns = vec!["id".to_string(), "name".to_string(), "value".to_string()];
        result.rows = sorted_rows;
        result.affected_rows = result.rows.len() as u64;

        info!("Sort completed, returned {} rows", result.affected_rows);
        Ok(result)
    }
}

// 向量化操作符
#[derive(Debug)]
pub struct BatchScanOperator {
    pub table: String,
    pub columns: Vec<String>,
}

impl BatchScanOperator {
    pub fn new(table: String, columns: Vec<String>) -> Self {
        Self { table, columns }
    }

    pub async fn execute_batch(&self) -> Result<QueryResult> {
        // TODO: 实现批量扫描逻辑
        let mut result = QueryResult::new();
        result.columns = self.columns.clone();
        result.rows = vec![vec!["1".to_string(), "test".to_string()]];
        result.affected_rows = 1;
        Ok(result)
    }
}

#[derive(Debug)]
pub struct BatchIndexScanOperator {
    pub table: String,
    pub index: String,
    pub columns: Vec<String>,
}

impl BatchIndexScanOperator {
    pub fn new(table: String, index: String, columns: Vec<String>) -> Self {
        Self { table, index, columns }
    }

    pub async fn execute_batch(&self) -> Result<QueryResult> {
        // TODO: 实现批量索引扫描逻辑
        let mut result = QueryResult::new();
        result.columns = self.columns.clone();
        result.rows = vec![vec!["1".to_string(), "test".to_string()]];
        result.affected_rows = 1;
        Ok(result)
    }
}

#[derive(Debug)]
pub struct BatchJoinOperator {
    pub left: crate::optimizer::PlanNode,
    pub right: crate::optimizer::PlanNode,
    pub join_type: String,
    pub condition: String,
}

impl BatchJoinOperator {
    pub fn new(
        left: crate::optimizer::PlanNode,
        right: crate::optimizer::PlanNode,
        join_type: String,
        condition: String,
    ) -> Self {
        Self {
            left,
            right,
            join_type,
            condition,
        }
    }

    pub async fn execute_batch(&self) -> Result<QueryResult> {
        // TODO: 实现批量连接逻辑
        let mut result = QueryResult::new();
        result.columns = vec!["id".to_string(), "name".to_string()];
        result.rows = vec![vec!["1".to_string(), "test".to_string()]];
        result.affected_rows = 1;
        Ok(result)
    }
}

#[derive(Debug)]
pub struct BatchAggregateOperator {
    pub input: crate::optimizer::PlanNode,
    pub group_by: Vec<String>,
    pub aggregates: Vec<String>,
}

impl BatchAggregateOperator {
    pub fn new(
        input: crate::optimizer::PlanNode,
        group_by: Vec<String>,
        aggregates: Vec<String>,
    ) -> Self {
        Self {
            input,
            group_by,
            aggregates,
        }
    }

    pub async fn execute_batch(&self) -> Result<QueryResult> {
        // TODO: 实现批量聚合逻辑
        let mut result = QueryResult::new();
        result.columns = self.aggregates.clone();
        result.rows = vec![vec!["10".to_string()]];
        result.affected_rows = 1;
        Ok(result)
    }
}

#[derive(Debug)]
pub struct BatchSortOperator {
    pub input: crate::optimizer::PlanNode,
    pub order_by: Vec<String>,
}

impl BatchSortOperator {
    pub fn new(input: crate::optimizer::PlanNode, order_by: Vec<String>) -> Self {
        Self { input, order_by }
    }

    pub async fn execute_batch(&self) -> Result<QueryResult> {
        // TODO: 实现批量排序逻辑
        let mut result = QueryResult::new();
        result.columns = vec!["id".to_string(), "name".to_string()];
        result.rows = vec![vec!["1".to_string(), "test".to_string()]];
        result.affected_rows = 1;
        Ok(result)
    }
}

// MPP 并行任务
#[derive(Debug)]
pub struct ParallelScanTask {
    pub task_id: String,
    pub table: String,
    pub columns: Vec<String>,
}

impl ParallelScanTask {
    pub fn new(task_id: String, table: String, columns: Vec<String>) -> Self {
        Self { task_id, table, columns }
    }

    pub async fn execute_parallel(&self) -> Result<QueryResult> {
        // TODO: 实现并行扫描逻辑
        let mut result = QueryResult::new();
        result.columns = self.columns.clone();
        result.rows = vec![vec!["1".to_string(), "test".to_string()]];
        result.affected_rows = 1;
        Ok(result)
    }
}

#[derive(Debug)]
pub struct ParallelIndexScanTask {
    pub task_id: String,
    pub table: String,
    pub index: String,
    pub columns: Vec<String>,
}

impl ParallelIndexScanTask {
    pub fn new(task_id: String, table: String, index: String, columns: Vec<String>) -> Self {
        Self { task_id, table, index, columns }
    }

    pub async fn execute_parallel(&self) -> Result<QueryResult> {
        // TODO: 实现并行索引扫描逻辑
        let mut result = QueryResult::new();
        result.columns = self.columns.clone();
        result.rows = vec![vec!["1".to_string(), "test".to_string()]];
        result.affected_rows = 1;
        Ok(result)
    }
}

#[derive(Debug)]
pub struct ParallelJoinTask {
    pub task_id: String,
    pub left: crate::optimizer::PlanNode,
    pub right: crate::optimizer::PlanNode,
    pub join_type: String,
    pub condition: String,
}

impl ParallelJoinTask {
    pub fn new(
        task_id: String,
        left: crate::optimizer::PlanNode,
        right: crate::optimizer::PlanNode,
        join_type: String,
        condition: String,
    ) -> Self {
        Self {
            task_id,
            left,
            right,
            join_type,
            condition,
        }
    }

    pub async fn execute_parallel(&self) -> Result<QueryResult> {
        // TODO: 实现并行连接逻辑
        let mut result = QueryResult::new();
        result.columns = vec!["id".to_string(), "name".to_string()];
        result.rows = vec![vec!["1".to_string(), "test".to_string()]];
        result.affected_rows = 1;
        Ok(result)
    }
}

#[derive(Debug)]
pub struct ParallelAggregateTask {
    pub task_id: String,
    pub input: crate::optimizer::PlanNode,
    pub group_by: Vec<String>,
    pub aggregates: Vec<String>,
}

impl ParallelAggregateTask {
    pub fn new(
        task_id: String,
        input: crate::optimizer::PlanNode,
        group_by: Vec<String>,
        aggregates: Vec<String>,
    ) -> Self {
        Self {
            task_id,
            input,
            group_by,
            aggregates,
        }
    }

    pub async fn execute_parallel(&self) -> Result<QueryResult> {
        // TODO: 实现并行聚合逻辑
        let mut result = QueryResult::new();
        result.columns = self.aggregates.clone();
        result.rows = vec![vec!["10".to_string()]];
        result.affected_rows = 1;
        Ok(result)
    }
}

#[derive(Debug)]
pub struct ParallelSortTask {
    pub task_id: String,
    pub input: crate::optimizer::PlanNode,
    pub order_by: Vec<String>,
}

impl ParallelSortTask {
    pub fn new(task_id: String, input: crate::optimizer::PlanNode, order_by: Vec<String>) -> Self {
        Self { task_id, input, order_by }
    }

    pub async fn execute_parallel(&self) -> Result<QueryResult> {
        // TODO: 实现并行排序逻辑
        let mut result = QueryResult::new();
        result.columns = vec!["id".to_string(), "name".to_string()];
        result.rows = vec![vec!["1".to_string(), "test".to_string()]];
        result.affected_rows = 1;
        Ok(result)
    }
}

/// 顺序扫描操作符
#[derive(Debug)]
pub struct SeqScanOperator {
    pub table: String,
    pub columns: Vec<String>,
    pub buffer_pool: Arc<BufferPool>,
    pub memory_manager: Arc<MemoryManager>,
    pub start_page: u32,
    pub end_page: u32,
}

impl SeqScanOperator {
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
            start_page: 0,
            end_page: u32::MAX,
        }
    }

    /// 设置扫描范围
    pub fn set_scan_range(&mut self, start_page: u32, end_page: u32) {
        self.start_page = start_page;
        self.end_page = end_page;
    }

    /// 执行顺序扫描
    async fn perform_seq_scan(&self) -> Result<Vec<Vec<String>>> {
        info!("Performing sequential scan on table: {} from page {} to {}",
              self.table, self.start_page, self.end_page);

        // 分配工作内存
        let work_memory = self.memory_manager.allocate_work_memory(1024 * 1024)?; // 1MB

        let mut rows = Vec::new();
        let mut current_page = self.start_page;

        // 顺序扫描指定范围内的页面
        while current_page <= self.end_page {
            match self.buffer_pool.get_buffer(PageId(current_page as usize)) {
                Ok(page) => {
                    let page_rows = self.parse_page_data(&page.data)?;
                    rows.extend(page_rows);
                }
                Err(_) => {
                    // 页面不存在，停止扫描
                    break;
                }
            }
            current_page += 1;
        }

        // 释放工作内存
        self.memory_manager.free_memory(work_memory);

        info!("Sequential scan completed, found {} rows", rows.len());
        Ok(rows)
    }

    /// 解析页面数据
    fn parse_page_data(&self, data: &[u8]) -> Result<Vec<Vec<String>>> {
        let mut rows = Vec::new();
        let row_size = 100;
        let num_rows = data.len() / row_size;

        for i in 0..num_rows {
            let start = i * row_size;
            let end = start + row_size;
            if end <= data.len() {
                let row_data = &data[start..end];
                let row = self.parse_row_data(row_data)?;
                rows.push(row);
            }
        }

        Ok(rows)
    }

    /// 解析行数据
    fn parse_row_data(&self, data: &[u8]) -> Result<Vec<String>> {
        let mut row = Vec::new();

        for column in &self.columns {
            let value = match column.as_str() {
                "id" => format!("{}", data.len()),
                "name" => format!("row_{}", data.len()),
                "value" => format!("val_{}", data.len()),
                _ => format!("col_{}", column),
            };
            row.push(value);
        }

        Ok(row)
    }
}

#[async_trait]
impl Operator for SeqScanOperator {
    async fn execute(&self) -> Result<QueryResult> {
        debug!("Executing sequential scan on table: {}", self.table);

        let rows = self.perform_seq_scan().await?;

        let mut result = QueryResult::new();
        result.columns = self.columns.clone();
        result.rows = rows;
        result.affected_rows = result.rows.len() as u64;

        info!("Sequential scan completed, returned {} rows", result.affected_rows);
        Ok(result)
    }
}

/// 增强版索引扫描操作符
#[derive(Debug)]
pub struct EnhancedIndexScanOperator {
    pub table: String,
    pub index_name: String,
    pub columns: Vec<String>,
    pub buffer_pool: Arc<BufferPool>,
    pub memory_manager: Arc<MemoryManager>,
    pub index_conditions: HashMap<String, String>,
    pub index_type: String, // B-tree, Hash, etc.
}

impl EnhancedIndexScanOperator {
    pub fn new(
        table: String,
        index_name: String,
        columns: Vec<String>,
        buffer_pool: Arc<BufferPool>,
        memory_manager: Arc<MemoryManager>,
    ) -> Self {
        Self {
            table,
            index_name,
            columns,
            buffer_pool,
            memory_manager,
            index_conditions: HashMap::new(),
            index_type: "B-tree".to_string(),
        }
    }

    /// 设置索引类型
    pub fn set_index_type(&mut self, index_type: String) {
        self.index_type = index_type;
    }

    /// 设置索引条件
    pub fn set_index_condition(&mut self, column: String, value: String) {
        self.index_conditions.insert(column, value);
    }

    /// 执行索引扫描
    async fn perform_index_scan(&self) -> Result<Vec<Vec<String>>> {
        info!("Performing index scan on index: {} of table: {} with type: {}",
              self.index_name, self.table, self.index_type);

        // 分配工作内存
        let work_memory = self.memory_manager.allocate_work_memory(512 * 1024)?; // 512KB

        // 模拟索引查找过程
        let mut rows = Vec::new();

        // 根据索引类型执行不同的查找策略
        match self.index_type.as_str() {
            "B-tree" => {
                rows = self.b_tree_scan().await?;
            }
            "Hash" => {
                rows = self.hash_scan().await?;
            }
            "Bitmap" => {
                rows = self.bitmap_scan().await?;
            }
            _ => {
                warn!("Unknown index type: {}, using B-tree", self.index_type);
                rows = self.b_tree_scan().await?;
            }
        }

        // 释放工作内存
        self.memory_manager.free_memory(work_memory);

        info!("Index scan completed, found {} rows", rows.len());
        Ok(rows)
    }

    /// B-tree索引扫描
    async fn b_tree_scan(&self) -> Result<Vec<Vec<String>>> {
        let mut rows = Vec::new();

        // 模拟B-tree索引查找
        for page_id in 0..5 {
            let page = self.buffer_pool.get_buffer(PageId(page_id))?;
            let page_rows = self.parse_page_data(&page.data)?;

            // 应用索引条件过滤
            let filtered_rows = self.apply_index_conditions(page_rows)?;
            rows.extend(filtered_rows);
        }

        Ok(rows)
    }

    /// Hash索引扫描
    async fn hash_scan(&self) -> Result<Vec<Vec<String>>> {
        let mut rows = Vec::new();

        // 模拟Hash索引查找
        for page_id in 0..3 {
            let page = self.buffer_pool.get_buffer(PageId(page_id))?;
            let page_rows = self.parse_page_data(&page.data)?;

            // 应用索引条件过滤
            let filtered_rows = self.apply_index_conditions(page_rows)?;
            rows.extend(filtered_rows);
        }

        Ok(rows)
    }

    /// Bitmap索引扫描
    async fn bitmap_scan(&self) -> Result<Vec<Vec<String>>> {
        let mut rows = Vec::new();

        // 模拟Bitmap索引查找
        for page_id in 0..4 {
            let page = self.buffer_pool.get_buffer(PageId(page_id))?;
            let page_rows = self.parse_page_data(&page.data)?;

            // 应用索引条件过滤
            let filtered_rows = self.apply_index_conditions(page_rows)?;
            rows.extend(filtered_rows);
        }

        Ok(rows)
    }

    /// 应用索引条件
    fn apply_index_conditions(&self, rows: Vec<Vec<String>>) -> Result<Vec<Vec<String>>> {
        if self.index_conditions.is_empty() {
            return Ok(rows);
        }

        let mut filtered_rows = Vec::new();

        for row in rows {
            let mut matches = true;

            for (column, expected_value) in &self.index_conditions {
                if let Some(column_index) = self.columns.iter().position(|c| c == column) {
                    if column_index < row.len() && row[column_index] != *expected_value {
                        matches = false;
                        break;
                    }
                }
            }

            if matches {
                filtered_rows.push(row);
            }
        }

        Ok(filtered_rows)
    }

    /// 解析页面数据
    fn parse_page_data(&self, data: &[u8]) -> Result<Vec<Vec<String>>> {
        let mut rows = Vec::new();
        let row_size = 100;
        let num_rows = data.len() / row_size;

        for i in 0..num_rows {
            let start = i * row_size;
            let end = start + row_size;
            if end <= data.len() {
                let row_data = &data[start..end];
                let row = self.parse_row_data(row_data)?;
                rows.push(row);
            }
        }

        Ok(rows)
    }

    /// 解析行数据
    fn parse_row_data(&self, data: &[u8]) -> Result<Vec<String>> {
        let mut row = Vec::new();

        for column in &self.columns {
            let value = match column.as_str() {
                "id" => format!("{}", data.len()),
                "name" => format!("row_{}", data.len()),
                "value" => format!("val_{}", data.len()),
                _ => format!("col_{}", column),
            };
            row.push(value);
        }

        Ok(row)
    }
}

#[async_trait]
impl Operator for EnhancedIndexScanOperator {
    async fn execute(&self) -> Result<QueryResult> {
        debug!("Executing index scan on index: {} of table: {}", self.index_name, self.table);

        let rows = self.perform_index_scan().await?;

        let mut result = QueryResult::new();
        result.columns = self.columns.clone();
        result.rows = rows;
        result.affected_rows = result.rows.len() as u64;

        info!("Index scan completed, returned {} rows", result.affected_rows);
        Ok(result)
    }
}

/// 位图扫描操作符
#[derive(Debug)]
pub struct BitmapScanOperator {
    pub table: String,
    pub columns: Vec<String>,
    pub buffer_pool: Arc<BufferPool>,
    pub memory_manager: Arc<MemoryManager>,
    pub bitmap_conditions: Vec<BitmapCondition>,
}

#[derive(Debug)]
pub struct BitmapCondition {
    pub column: String,
    pub values: Vec<String>,
    pub operation: String, // AND, OR, NOT
}

impl BitmapScanOperator {
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
            bitmap_conditions: Vec::new(),
        }
    }

    /// 添加位图条件
    pub fn add_bitmap_condition(&mut self, column: String, values: Vec<String>, operation: String) {
        self.bitmap_conditions.push(BitmapCondition {
            column,
            values,
            operation,
        });
    }

    /// 执行位图扫描
    async fn perform_bitmap_scan(&self) -> Result<Vec<Vec<String>>> {
        info!("Performing bitmap scan on table: {} with {} conditions",
              self.table, self.bitmap_conditions.len());

        // 分配工作内存
        let work_memory = self.memory_manager.allocate_work_memory(1024 * 1024)?; // 1MB

        // 构建位图
        let mut bitmap = self.build_bitmap().await?;

        // 应用位图条件
        bitmap = self.apply_bitmap_conditions(bitmap).await?;

        // 根据位图获取数据
        let rows = self.fetch_data_by_bitmap(bitmap).await?;

        // 释放工作内存
        self.memory_manager.free_memory(work_memory);

        info!("Bitmap scan completed, found {} rows", rows.len());
        Ok(rows)
    }

    /// 构建位图
    async fn build_bitmap(&self) -> Result<Vec<bool>> {
        // 模拟构建位图
        let mut bitmap = Vec::new();

        // 假设有1000行数据
        for i in 0..1000 {
            // 模拟位图构建逻辑
            bitmap.push(i % 2 == 0);
        }

        Ok(bitmap)
    }

    /// 应用位图条件
    async fn apply_bitmap_conditions(&self, mut bitmap: Vec<bool>) -> Result<Vec<bool>> {
        for condition in &self.bitmap_conditions {
            match condition.operation.as_str() {
                "AND" => {
                    bitmap = self.apply_and_condition(bitmap, condition).await?;
                }
                "OR" => {
                    bitmap = self.apply_or_condition(bitmap, condition).await?;
                }
                "NOT" => {
                    bitmap = self.apply_not_condition(bitmap, condition).await?;
                }
                _ => {
                    warn!("Unknown bitmap operation: {}", condition.operation);
                }
            }
        }

        Ok(bitmap)
    }

    /// 应用AND条件
    async fn apply_and_condition(&self, bitmap: Vec<bool>, condition: &BitmapCondition) -> Result<Vec<bool>> {
        let mut result = Vec::new();

        for (i, &bit) in bitmap.iter().enumerate() {
            // 模拟AND条件检查
            let matches = self.check_condition_match(i, condition)?;
            result.push(bit && matches);
        }

        Ok(result)
    }

    /// 应用OR条件
    async fn apply_or_condition(&self, bitmap: Vec<bool>, condition: &BitmapCondition) -> Result<Vec<bool>> {
        let mut result = Vec::new();

        for (i, &bit) in bitmap.iter().enumerate() {
            let matches = self.check_condition_match(i, condition)?;
            result.push(bit || matches);
        }

        Ok(result)
    }

    /// 应用NOT条件
    async fn apply_not_condition(&self, bitmap: Vec<bool>, condition: &BitmapCondition) -> Result<Vec<bool>> {
        let mut result = Vec::new();

        for (i, &bit) in bitmap.iter().enumerate() {
            let matches = self.check_condition_match(i, condition)?;
            result.push(bit && !matches);
        }

        Ok(result)
    }

    /// 检查条件匹配
    fn check_condition_match(&self, row_index: usize, condition: &BitmapCondition) -> Result<bool> {
        // 模拟条件匹配检查
        Ok(row_index % 3 == 0)
    }

    /// 根据位图获取数据
    async fn fetch_data_by_bitmap(&self, bitmap: Vec<bool>) -> Result<Vec<Vec<String>>> {
        let mut rows = Vec::new();

        for (i, &bit) in bitmap.iter().enumerate() {
            if bit {
                // 模拟从页面获取行数据
                let page_id = i / 10; // 每页10行
                if let Ok(page) = self.buffer_pool.get_buffer(PageId(page_id)) {
                    let page_rows = self.parse_page_data(&page.data)?;
                    if (i % 10) < page_rows.len() {
                        rows.push(page_rows[i % 10].clone());
                    }
                }
            }
        }

        Ok(rows)
    }

    /// 解析页面数据
    fn parse_page_data(&self, data: &[u8]) -> Result<Vec<Vec<String>>> {
        let mut rows = Vec::new();
        let row_size = 100;
        let num_rows = data.len() / row_size;

        for i in 0..num_rows {
            let start = i * row_size;
            let end = start + row_size;
            if end <= data.len() {
                let row_data = &data[start..end];
                let row = self.parse_row_data(row_data)?;
                rows.push(row);
            }
        }

        Ok(rows)
    }

    /// 解析行数据
    fn parse_row_data(&self, data: &[u8]) -> Result<Vec<String>> {
        let mut row = Vec::new();

        for column in &self.columns {
            let value = match column.as_str() {
                "id" => format!("{}", data.len()),
                "name" => format!("row_{}", data.len()),
                "value" => format!("val_{}", data.len()),
                _ => format!("col_{}", column),
            };
            row.push(value);
        }

        Ok(row)
    }
}

#[async_trait]
impl Operator for BitmapScanOperator {
    async fn execute(&self) -> Result<QueryResult> {
        debug!("Executing bitmap scan on table: {}", self.table);

        let rows = self.perform_bitmap_scan().await?;

        let mut result = QueryResult::new();
        result.columns = self.columns.clone();
        result.rows = rows;
        result.affected_rows = result.rows.len() as u64;

        info!("Bitmap scan completed, returned {} rows", result.affected_rows);
        Ok(result)
    }
}

/// 并行扫描操作符
#[derive(Debug)]
pub struct ParallelScanOperator {
    pub table: String,
    pub columns: Vec<String>,
    pub buffer_pool: Arc<BufferPool>,
    pub memory_manager: Arc<MemoryManager>,
    pub worker_pool: Arc<WorkerPool>,
    pub num_workers: usize,
    pub chunk_size: usize,
}

impl ParallelScanOperator {
    pub fn new(
        table: String,
        columns: Vec<String>,
        buffer_pool: Arc<BufferPool>,
        memory_manager: Arc<MemoryManager>,
        worker_pool: Arc<WorkerPool>,
    ) -> Self {
        Self {
            table,
            columns,
            buffer_pool,
            memory_manager,
            worker_pool,
            num_workers: 4,
            chunk_size: 1000,
        }
    }

    /// 设置并行参数
    pub fn set_parallel_params(&mut self, num_workers: usize, chunk_size: usize) {
        self.num_workers = num_workers;
        self.chunk_size = chunk_size;
    }

    /// 执行并行扫描
    async fn perform_parallel_scan(&self) -> Result<Vec<Vec<String>>> {
        info!("Performing parallel scan on table: {} with {} workers",
              self.table, self.num_workers);

        // 分配共享内存
        let shared_memory = self.memory_manager.allocate_shared_memory(2 * 1024 * 1024)?; // 2MB

        // 创建任务通道
        let (tx, rx) = channel();

        // 计算每个工作线程处理的页面范围
        let total_pages = 100; // 假设总共有100页
        let pages_per_worker = total_pages / self.num_workers;

        // 启动并行任务
        let mut handles = Vec::new();
        for worker_id in 0..self.num_workers {
            let start_page = worker_id * pages_per_worker;
            let end_page = if worker_id == self.num_workers - 1 {
                total_pages
            } else {
                (worker_id + 1) * pages_per_worker
            };

            let tx_clone = tx.clone();
            let buffer_pool = Arc::clone(&self.buffer_pool);
            let columns = self.columns.clone();

            let handle = thread::spawn(move || {
                let result = Self::scan_page_range(start_page, end_page, buffer_pool, columns);
                tx_clone.send((worker_id, result)).unwrap();
            });

            handles.push(handle);
        }

        // 收集结果
        let mut all_rows = Vec::new();
        for _ in 0..self.num_workers {
            if let Ok((worker_id, worker_result)) = rx.recv() {
                match worker_result {
                    Ok(rows) => {
                        info!("Worker {} completed, found {} rows", worker_id, rows.len());
                        all_rows.extend(rows);
                    }
                    Err(e) => {
                        warn!("Worker {} failed: {:?}", worker_id, e);
                    }
                }
            }
        }

        // 等待所有线程完成
        for handle in handles {
            handle.join().unwrap();
        }

        // 释放共享内存
        self.memory_manager.free_memory(shared_memory);

        info!("Parallel scan completed, found {} rows", all_rows.len());
        Ok(all_rows)
    }

    /// 扫描页面范围（静态方法，用于工作线程）
    fn scan_page_range(
        start_page: usize,
        end_page: usize,
        buffer_pool: Arc<BufferPool>,
        columns: Vec<String>,
    ) -> Result<Vec<Vec<String>>> {
        let mut rows = Vec::new();

        for page_id in start_page..end_page {
            if let Ok(page) = buffer_pool.get_buffer(PageId(page_id)) {
                let page_rows = Self::parse_page_data(&page.data, &columns)?;
                rows.extend(page_rows);
            }
        }

        Ok(rows)
    }

    /// 解析页面数据（静态方法）
    fn parse_page_data(data: &[u8], columns: &[String]) -> Result<Vec<Vec<String>>> {
        let mut rows = Vec::new();
        let row_size = 100;
        let num_rows = data.len() / row_size;

        for i in 0..num_rows {
            let start = i * row_size;
            let end = start + row_size;
            if end <= data.len() {
                let row_data = &data[start..end];
                let row = Self::parse_row_data(row_data, columns)?;
                rows.push(row);
            }
        }

        Ok(rows)
    }

    /// 解析行数据（静态方法）
    fn parse_row_data(data: &[u8], columns: &[String]) -> Result<Vec<String>> {
        let mut row = Vec::new();

        for column in columns {
            let value = match column.as_str() {
                "id" => format!("{}", data.len()),
                "name" => format!("row_{}", data.len()),
                "value" => format!("val_{}", data.len()),
                _ => format!("col_{}", column),
            };
            row.push(value);
        }

        Ok(row)
    }
}

#[async_trait]
impl Operator for ParallelScanOperator {
    async fn execute(&self) -> Result<QueryResult> {
        debug!("Executing parallel scan on table: {}", self.table);

        let rows = self.perform_parallel_scan().await?;

        let mut result = QueryResult::new();
        result.columns = self.columns.clone();
        result.rows = rows;
        result.affected_rows = result.rows.len() as u64;

        info!("Parallel scan completed, returned {} rows", result.affected_rows);
        Ok(result)
    }
}

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

    /// 设置分片信息
    pub fn set_shard_info(&mut self, shard_key: String, num_shards: usize, strategy: String) {
        self.shard_info = ShardInfo {
            shard_key,
            num_shards,
            shard_strategy: strategy,
        };
    }

    /// 添加分片节点
    pub fn add_shard_node(&mut self, node_id: String, host: String, port: u16, ranges: Vec<(String, String)>) {
        self.shard_nodes.push(ShardNode {
            node_id,
            host,
            port,
            shard_ranges: ranges,
        });
    }

    /// 执行分片扫描
    async fn perform_shard_scan(&self) -> Result<Vec<Vec<String>>> {
        info!("Performing shard scan on table: {} with {} shards",
              self.table, self.shard_info.num_shards);

        // 分配共享内存
        let shared_memory = self.memory_manager.allocate_shared_memory(2 * 1024 * 1024)?; // 2MB

        // 创建任务通道
        let (tx, rx) = channel();

        // 为每个分片节点启动扫描任务
        let mut handles = Vec::new();
        for (node_index, node) in self.shard_nodes.iter().enumerate() {
            let tx_clone = tx.clone();
            let node_id = node.node_id.clone();
            let host = node.host.clone();
            let port = node.port;
            let ranges = node.shard_ranges.clone();
            let columns = self.columns.clone();

            let handle = thread::spawn(move || {
                let result = Self::scan_shard_node(node_id, host, port, ranges, columns);
                tx_clone.send((node_index, result)).unwrap();
            });

            handles.push(handle);
        }

        // 收集所有分片的结果
        let mut all_rows = Vec::new();
        for _ in 0..self.shard_nodes.len() {
            if let Ok((node_index, node_result)) = rx.recv() {
                match node_result {
                    Ok(rows) => {
                        info!("Shard node {} completed, found {} rows", node_index, rows.len());
                        all_rows.extend(rows);
                    }
                    Err(e) => {
                        warn!("Shard node {} failed: {:?}", node_index, e);
                    }
                }
            }
        }

        // 等待所有线程完成
        for handle in handles {
            handle.join().unwrap();
        }

        // 释放共享内存
        self.memory_manager.free_memory(shared_memory);

        info!("Shard scan completed, found {} rows", all_rows.len());
        Ok(all_rows)
    }

    /// 扫描分片节点（静态方法，用于工作线程）
    fn scan_shard_node(
        node_id: String,
        host: String,
        port: u16,
        ranges: Vec<(String, String)>,
        columns: Vec<String>,
    ) -> Result<Vec<Vec<String>>> {
        let mut rows = Vec::new();

        // 模拟从远程分片节点获取数据
        for (start_key, end_key) in ranges {
            // 模拟网络请求获取分片数据
            let shard_rows = Self::fetch_shard_data(&node_id, &host, port, &start_key, &end_key, &columns)?;
            rows.extend(shard_rows);
        }

        Ok(rows)
    }

    /// 获取分片数据（静态方法）
    fn fetch_shard_data(
        node_id: &str,
        host: &str,
        port: u16,
        start_key: &str,
        end_key: &str,
        columns: &[String],
    ) -> Result<Vec<Vec<String>>> {
        // 模拟从远程分片获取数据
        let mut rows = Vec::new();

        // 模拟根据键范围生成数据
        if let (Ok(start), Ok(end)) = (start_key.parse::<i32>(), end_key.parse::<i32>()) {
            for i in start..=end {
                let mut row = Vec::new();
                for column in columns {
                    let value = match column.as_str() {
                        "id" => i.to_string(),
                        "name" => format!("shard_{}_{}", node_id, i),
                        "value" => format!("val_{}", i),
                        _ => format!("col_{}", column),
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
        debug!("Executing shard scan on table: {}", self.table);

        let rows = self.perform_shard_scan().await?;

        let mut result = QueryResult::new();
        result.columns = self.columns.clone();
        result.rows = rows;
        result.affected_rows = result.rows.len() as u64;

        info!("Shard scan completed, returned {} rows", result.affected_rows);
        Ok(result)
    }
}