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

    async fn perform_aggregation(&self, input_data: QueryResult) -> Result<Vec<Vec<String>>> {
        info!("Performing aggregation with group by: {:?}, aggregates: {:?}", 
              self.group_by, self.aggregates);

        // 分配工作内存
        let work_memory = self.memory_manager.allocate_work_memory(1024 * 1024)?;

        let mut groups: HashMap<String, Vec<Vec<String>>> = HashMap::new();

        // 按分组键分组数据
        for row in &input_data.rows {
            let group_key = self.create_group_key(row, &input_data.columns)?;
            groups.entry(group_key).or_insert_with(Vec::new).push(row.clone());
        }

        // 对每个分组进行聚合
        let mut result_rows = Vec::new();
        for (group_key, rows) in groups {
            let aggregated_row = self.aggregate_group(&group_key, &rows, &input_data.columns)?;
            result_rows.push(aggregated_row);
        }

        // 释放工作内存
        self.memory_manager.free_memory(work_memory);

        Ok(result_rows)
    }

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
impl Operator for AggregateOperator {
    async fn execute(&self) -> Result<QueryResult> {
        debug!("Executing aggregate operation");

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

        let aggregated_rows = self.perform_aggregation(input_data).await?;

        let mut result = QueryResult::new();
        result.columns = vec!["name".to_string(), "count".to_string(), "sum".to_string(), "avg".to_string()];
        result.rows = aggregated_rows;
        result.affected_rows = result.rows.len() as u64;

        info!("Aggregation completed, returned {} rows", result.affected_rows);
        Ok(result)
    }
}

/// Hash聚合操作符
#[derive(Debug)]
pub struct HashAggOperator {
    pub input: crate::optimizer::PlanNode,
    pub group_by: Vec<String>,
    pub aggregates: Vec<String>,
    pub memory_manager: Arc<MemoryManager>,
    pub buffer_pool: Arc<BufferPool>,
    pub hash_table_size: usize,
    pub group_keys: Vec<String>,
}

impl HashAggOperator {
    pub fn new(
        input: crate::optimizer::PlanNode,
        group_by: Vec<String>,
        aggregates: Vec<String>,
        memory_manager: Arc<MemoryManager>,
        buffer_pool: Arc<BufferPool>,
    ) -> Self {
        Self {
            input,
            group_by,
            aggregates,
            memory_manager,
            buffer_pool,
            hash_table_size: 10000,
            group_keys: vec!["name".to_string()],
        }
    }

    pub fn set_hash_table_size(&mut self, size: usize) {
        self.hash_table_size = size;
    }

    pub fn set_group_keys(&mut self, keys: Vec<String>) {
        self.group_keys = keys;
    }

    async fn perform_hash_aggregation(&self, input_data: QueryResult) -> Result<Vec<Vec<String>>> {
        info!("Performing hash aggregation with hash table size: {}", self.hash_table_size);

        // 分配工作内存
        let work_memory = self.memory_manager.allocate_work_memory(1024 * 1024)?;

        // 使用哈希表进行分组
        let mut hash_table: HashMap<String, Vec<Vec<String>>> = HashMap::new();

        for row in &input_data.rows {
            let group_key = self.extract_group_key(row, &input_data.columns)?;
            hash_table.entry(group_key).or_insert_with(Vec::new).push(row.clone());
        }

        // 对每个分组进行聚合
        let mut result_rows = Vec::new();
        for (group_key, rows) in hash_table {
            let aggregated_row = self.aggregate_group(&group_key, &rows, &input_data.columns)?;
            result_rows.push(aggregated_row);
        }

        // 释放工作内存
        self.memory_manager.free_memory(work_memory);

        Ok(result_rows)
    }

    fn extract_group_key(&self, row: &[String], columns: &[String]) -> Result<String> {
        let mut key_parts = Vec::new();

        for group_key in &self.group_keys {
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
impl Operator for HashAggOperator {
    async fn execute(&self) -> Result<QueryResult> {
        debug!("Executing hash aggregate operation");

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

        let aggregated_rows = self.perform_hash_aggregation(input_data).await?;

        let mut result = QueryResult::new();
        result.columns = vec!["name".to_string(), "count".to_string(), "sum".to_string(), "avg".to_string()];
        result.rows = aggregated_rows;
        result.affected_rows = result.rows.len() as u64;

        info!("Hash aggregation completed, returned {} rows", result.affected_rows);
        Ok(result)
    }
}

/// 分组聚合操作符
#[derive(Debug)]
pub struct GroupAggOperator {
    pub input: crate::optimizer::PlanNode,
    pub group_by: Vec<String>,
    pub aggregates: Vec<String>,
    pub memory_manager: Arc<MemoryManager>,
    pub buffer_pool: Arc<BufferPool>,
    pub sort_keys: Vec<String>,
    pub batch_size: usize,
}

impl GroupAggOperator {
    pub fn new(
        input: crate::optimizer::PlanNode,
        group_by: Vec<String>,
        aggregates: Vec<String>,
        memory_manager: Arc<MemoryManager>,
        buffer_pool: Arc<BufferPool>,
    ) -> Self {
        Self {
            input,
            group_by,
            aggregates,
            memory_manager,
            buffer_pool,
            sort_keys: vec!["name".to_string()],
            batch_size: 1000,
        }
    }

    pub fn set_sort_keys(&mut self, keys: Vec<String>) {
        self.sort_keys = keys;
    }

    pub fn set_batch_size(&mut self, size: usize) {
        self.batch_size = size;
    }

    async fn perform_group_aggregation(&self, input_data: QueryResult) -> Result<Vec<Vec<String>>> {
        info!("Performing group aggregation with batch size: {}", self.batch_size);

        // 分配工作内存
        let work_memory = self.memory_manager.allocate_work_memory(1024 * 1024)?;

        // 对数据进行排序
        let sorted_data = self.sort_data(input_data.rows, &input_data.columns)?;

        // 分批处理排序后的数据
        let mut result_rows = Vec::new();
        let mut current_group = Vec::new();
        let mut current_group_key = String::new();

        for row in sorted_data {
            let group_key = self.extract_group_key(&row, &input_data.columns)?;

            if group_key != current_group_key && !current_group.is_empty() {
                // 处理当前分组
                let aggregated_row = self.aggregate_group(&current_group_key, &current_group, &input_data.columns)?;
                result_rows.push(aggregated_row);
                current_group.clear();
            }

            current_group_key = group_key.clone();
            current_group.push(row);
        }

        // 处理最后一个分组
        if !current_group.is_empty() {
            let aggregated_row = self.aggregate_group(&current_group_key, &current_group, &input_data.columns)?;
            result_rows.push(aggregated_row);
        }

        // 释放工作内存
        self.memory_manager.free_memory(work_memory);

        Ok(result_rows)
    }

    fn sort_data(&self, mut rows: Vec<Vec<String>>, columns: &[String]) -> Result<Vec<Vec<String>>> {
        rows.sort_by(|a, b| {
            let a_key = self.extract_group_key(a, columns).unwrap_or_default();
            let b_key = self.extract_group_key(b, columns).unwrap_or_default();
            a_key.cmp(&b_key)
        });
        Ok(rows)
    }

    fn extract_group_key(&self, row: &[String], columns: &[String]) -> Result<String> {
        let mut key_parts = Vec::new();

        for sort_key in &self.sort_keys {
            if let Some(col_index) = columns.iter().position(|c| c == sort_key) {
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
impl Operator for GroupAggOperator {
    async fn execute(&self) -> Result<QueryResult> {
        debug!("Executing group aggregate operation");

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

        let aggregated_rows = self.perform_group_aggregation(input_data).await?;

        let mut result = QueryResult::new();
        result.columns = vec!["name".to_string(), "count".to_string(), "sum".to_string(), "avg".to_string()];
        result.rows = aggregated_rows;
        result.affected_rows = result.rows.len() as u64;

        info!("Group aggregation completed, returned {} rows", result.affected_rows);
        Ok(result)
    }
} 