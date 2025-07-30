use common::Result;
use tracing::{debug, info};

use crate::optimizer::{OptimizedPlan, PlanNode};

/// SQL 执行器
pub struct Executor;

impl Executor {
    pub fn new() -> Self {
        Self {}
    }

    /// 执行优化后的查询计划
    pub async fn execute(&self, plan: OptimizedPlan) -> Result<QueryResult> {
        info!("Executing optimized query plan");
        
        let mut result = QueryResult {
            columns: Vec::new(),
            rows: Vec::new(),
            affected_rows: 0,
            last_insert_id: None,
        };

        for node in plan.nodes {
            match node {
                PlanNode::TableScan { table, columns } => {
                    let table_result = self.execute_table_scan(&table, &columns).await?;
                    result.merge(table_result);
                }
                _ => {
                    debug!("Unsupported plan node type");
                }
            }
        }

        debug!("Query execution completed");
        Ok(result)
    }

    /// 执行表扫描
    async fn execute_table_scan(&self, table: &str, columns: &[String]) -> Result<QueryResult> {
        debug!("Executing table scan on table: {}", table);
        
        // 简化实现：返回模拟数据
        let result = QueryResult {
            columns: columns.to_vec(),
            rows: vec![
                vec!["1".to_string(), "Alice".to_string(), "25".to_string()],
                vec!["2".to_string(), "Bob".to_string(), "30".to_string()],
                vec!["3".to_string(), "Charlie".to_string(), "35".to_string()],
            ],
            affected_rows: 0,
            last_insert_id: None,
        };
        
        Ok(result)
    }
}

/// 查询结果
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub affected_rows: u64,
    pub last_insert_id: Option<u64>,
}

impl QueryResult {
    /// 合并查询结果
    pub fn merge(&mut self, other: QueryResult) {
        if self.columns.is_empty() {
            self.columns = other.columns;
        }
        self.rows.extend(other.rows);
        self.affected_rows += other.affected_rows;
        if self.last_insert_id.is_none() {
            self.last_insert_id = other.last_insert_id;
        }
    }
} 