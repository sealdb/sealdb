pub mod parser;
pub mod optimizer;
pub mod executor;

pub use parser::SqlParser;
pub use optimizer::{Optimizer, RuleBasedOptimizer, CostBasedOptimizer};
pub use executor::Executor;

use common::Result;

/// SQL 引擎
pub struct SqlEngine {
    parser: SqlParser,
    optimizer: Optimizer,
    executor: Executor,
}

impl SqlEngine {
    pub fn new() -> Self {
        Self {
            parser: SqlParser::new(),
            optimizer: Optimizer::new(),
            executor: Executor::new(),
        }
    }

    /// 执行 SQL 语句
    pub async fn execute(&self, sql: &str) -> Result<QueryResult> {
        // 1. 解析 SQL
        let parsed_stmt = self.parser.parse(sql)?;
        
        // 2. 优化查询计划
        let optimized_plan = self.optimizer.optimize(parsed_stmt).await?;
        
        // 3. 执行查询
        let executor_result = self.executor.execute(optimized_plan).await?;
        
        // 转换为公共的 QueryResult 类型
        let result = QueryResult {
            columns: executor_result.columns,
            rows: executor_result.rows,
            affected_rows: executor_result.affected_rows,
            last_insert_id: executor_result.last_insert_id,
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
