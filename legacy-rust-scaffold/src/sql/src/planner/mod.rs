//! SQL 查询规划器模块
//!
//! 负责基于规则的优化 (RBO - Rule-Based Optimization)
//! 包括查询重写、常量折叠、谓词下推等优化规则

use serde::{Deserialize, Serialize};
use common::Result;

// 重新导出 RBO 相关类型
pub use rbo::*;

/// SQL 查询规划器
pub trait Planner {
    /// 对查询计划进行基于规则的优化
    fn optimize(&self, plan: QueryPlan) -> Result<QueryPlan>;
}

/// 基于规则的规划器
pub struct RuleBasedPlanner {
    rules: Vec<Box<dyn OptimizationRule>>,
}

impl RuleBasedPlanner {
    pub fn new() -> Self {
        let mut planner = Self { rules: Vec::new() };

        // 注册优化规则（按应用顺序）
        planner.register_rule(Box::new(ConstantFoldingRule));
        planner.register_rule(Box::new(ExpressionSimplificationRule));
        planner.register_rule(Box::new(SubqueryFlatteningRule));
        planner.register_rule(Box::new(PredicatePushdownRule));
        planner.register_rule(Box::new(ColumnPruningRule));
        planner.register_rule(Box::new(JoinReorderRule));
        planner.register_rule(Box::new(IndexSelectionRule));
        planner.register_rule(Box::new(OrderByOptimizationRule));
        planner.register_rule(Box::new(GroupByOptimizationRule));
        planner.register_rule(Box::new(DistinctOptimizationRule));
        planner.register_rule(Box::new(LimitOptimizationRule));
        planner.register_rule(Box::new(UnionOptimizationRule));

        planner
    }

    /// 注册优化规则
    pub fn register_rule(&mut self, rule: Box<dyn OptimizationRule>) {
        self.rules.push(rule);
    }

    /// 获取规则数量（用于测试）
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }
}

impl Planner for RuleBasedPlanner {
    fn optimize(&self, plan: QueryPlan) -> Result<QueryPlan> {
        // 将 QueryPlan 转换为 OptimizedPlan，应用 RBO 规则，再转换回 QueryPlan
        // 这里简化实现，直接返回原计划
        Ok(plan)
    }
}

/// 查询计划
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryPlan {
    pub root: PlanNode,
}

/// 计划节点
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PlanNode {
    Scan {
        table_name: String,
        columns: Vec<String>,
        filters: Vec<Filter>,
    },
    Filter {
        condition: Expression,
        child: Box<PlanNode>,
    },
    Project {
        columns: Vec<Expression>,
        child: Box<PlanNode>,
    },
    Join {
        left: Box<PlanNode>,
        right: Box<PlanNode>,
        condition: Expression,
        join_type: JoinType,
    },
    Aggregate {
        group_by: Vec<Expression>,
        aggregates: Vec<AggregateFunction>,
        child: Box<PlanNode>,
    },
}

/// 表达式
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Expression {
    Column(String),
    Literal(Literal),
    BinaryOp {
        left: Box<Expression>,
        op: BinaryOperator,
        right: Box<Expression>,
    },
    Function {
        name: String,
        args: Vec<Expression>,
    },
}

/// 字面量
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Literal {
    Integer(i64),
    Float(f64),
    String(String),
    Boolean(bool),
    Null,
}

/// 二元操作符
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BinaryOperator {
    Add,
    Subtract,
    Multiply,
    Divide,
    Equal,
    NotEqual,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
    And,
    Or,
}

/// 过滤条件
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Filter {
    pub column: String,
    pub operator: FilterOperator,
    pub value: Literal,
}

/// 过滤操作符
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FilterOperator {
    Equal,
    NotEqual,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
    Like,
    In,
}

/// 连接类型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum JoinType {
    Inner,
    Left,
    Right,
    Full,
}

/// 聚合函数
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregateFunction {
    pub name: String,
    pub argument: Expression,
}

// 包含 RBO 实现
pub mod rbo;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rule_based_planner_creation() {
        let planner = RuleBasedPlanner::new();
        assert_eq!(planner.rule_count(), 12);
    }

    #[test]
    fn test_query_plan_creation() {
        let plan = QueryPlan {
            root: PlanNode::Scan {
                table_name: "test_table".to_string(),
                columns: vec!["id".to_string(), "name".to_string()],
                filters: vec![],
            },
        };

        assert!(matches!(plan.root, PlanNode::Scan { .. }));
    }
}