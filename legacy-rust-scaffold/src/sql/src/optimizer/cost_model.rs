//! SealDB 成本模型
//! 
//! 借鉴 PostgreSQL 和 TiDB 的成本模型实现

use common::Result;
use crate::parser::{ParsedExpression, ParsedOperator};

/// 成本模型（借鉴 PostgreSQL 的 cost model）
#[derive(Debug, Clone)]
pub struct CostModel {
    // CPU 成本参数
    pub cpu_tuple_cost: f64,      // 处理一个元组的 CPU 成本
    pub cpu_index_tuple_cost: f64, // 处理一个索引元组的 CPU 成本
    pub cpu_operator_cost: f64,   // 执行一个操作符的 CPU 成本

    // I/O 成本参数
    pub seq_page_cost: f64,       // 顺序扫描一个页面的成本
    pub random_page_cost: f64,    // 随机访问一个页面的成本
    pub cpu_page_cost: f64,       // CPU 处理一个页面的成本

    // 网络成本参数（分布式环境）
    pub network_cost_per_byte: f64, // 每字节的网络传输成本
    pub network_latency: f64,     // 网络延迟成本

    // 内存成本参数
    pub memory_cost_per_mb: f64,  // 每 MB 内存的成本

    // 并行度参数
    pub parallel_worker_cost: f64, // 每个并行工作线程的成本
    pub parallel_setup_cost: f64,  // 并行设置成本
}

impl Default for CostModel {
    fn default() -> Self {
        Self {
            // PostgreSQL 默认值
            cpu_tuple_cost: 0.01,
            cpu_index_tuple_cost: 0.005,
            cpu_operator_cost: 0.0025,
            seq_page_cost: 1.0,
            random_page_cost: 4.0,
            cpu_page_cost: 0.1,

            // 分布式环境参数
            network_cost_per_byte: 0.000001,
            network_latency: 0.1,
            memory_cost_per_mb: 0.01,
            parallel_worker_cost: 0.1,
            parallel_setup_cost: 1000.0,
        }
    }
}

impl CostModel {
    pub fn new() -> Self {
        Self::default()
    }

    /// 估算谓词选择性
    pub async fn estimate_selectivity(&self, predicate: &ParsedExpression) -> Result<f64> {
        match predicate {
            ParsedExpression::BinaryOp { operator, .. } => {
                match operator {
                    ParsedOperator::Equal => Ok(0.1), // 等值谓词
                    ParsedOperator::LessThan | ParsedOperator::LessThanOrEqual => Ok(0.3),
                    ParsedOperator::GreaterThan | ParsedOperator::GreaterThanOrEqual => Ok(0.3),
                    ParsedOperator::NotEqual => Ok(0.9),
                    _ => Ok(0.5), // 默认选择性
                }
            }
            ParsedExpression::Function { name, .. } => {
                // 处理函数调用，如 AND, OR, NOT
                match name.as_str() {
                    "AND" => Ok(0.3), // AND 函数
                    "OR" => Ok(0.7),  // OR 函数
                    "NOT" => Ok(0.5), // NOT 函数
                    _ => Ok(0.5),     // 默认选择性
                }
            }
            _ => Ok(0.5), // 默认选择性
        }
    }

    /// 估算连接选择性
    pub async fn estimate_join_selectivity(&self, condition: &ParsedExpression) -> Result<f64> {
        match condition {
            ParsedExpression::BinaryOp { operator, .. } => {
                match operator {
                    ParsedOperator::Equal => Ok(0.1), // 等值连接
                    _ => Ok(0.3), // 其他连接
                }
            }
            _ => Ok(0.5), // 默认连接选择性
        }
    }
}

/// 成本估算结果
#[derive(Debug, Clone)]
pub struct CostEstimate {
    pub startup_cost: f64,  // 启动成本
    pub total_cost: f64,    // 总成本
    pub io_cost: f64,       // I/O 成本
    pub cpu_cost: f64,      // CPU 成本
    pub network_cost: f64,  // 网络成本
    pub memory_cost: f64,   // 内存成本
    pub output_rows: u64,   // 输出行数
    pub output_width: f64,  // 输出宽度
}

impl CostEstimate {
    pub fn new() -> Self {
        Self {
            startup_cost: 0.0,
            total_cost: 0.0,
            io_cost: 0.0,
            cpu_cost: 0.0,
            network_cost: 0.0,
            memory_cost: 0.0,
            output_rows: 0,
            output_width: 0.0,
        }
    }

    /// 计算总成本
    pub fn calculate_total(&mut self) {
        self.total_cost = self.startup_cost + self.io_cost + self.cpu_cost + 
                         self.network_cost + self.memory_cost;
    }

    /// 添加成本
    pub fn add_cost(&mut self, other: &CostEstimate) {
        self.startup_cost += other.startup_cost;
        self.io_cost += other.io_cost;
        self.cpu_cost += other.cpu_cost;
        self.network_cost += other.network_cost;
        self.memory_cost += other.memory_cost;
        self.calculate_total();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_cost_model_default() {
        let cost_model = CostModel::default();
        assert_eq!(cost_model.cpu_tuple_cost, 0.01);
        assert_eq!(cost_model.seq_page_cost, 1.0);
        assert_eq!(cost_model.random_page_cost, 4.0);
    }

    #[tokio::test]
    async fn test_selectivity_estimation() {
        let cost_model = CostModel::new();
        
        // 测试等值谓词
        let equal_predicate = ParsedExpression::BinaryOp {
            left: Box::new(ParsedExpression::Column("id".to_string())),
            operator: ParsedOperator::Equal,
            right: Box::new(ParsedExpression::Literal(crate::parser::ParsedValue::Number("1".to_string()))),
        };
        
        let selectivity = cost_model.estimate_selectivity(&equal_predicate).await.unwrap();
        assert_eq!(selectivity, 0.1);
    }
} 