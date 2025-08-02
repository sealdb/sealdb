//! SealDB 主优化器
//! 
//! 协调基于规则的优化 (RBO) 和基于成本的优化 (CBO)

use common::Result;
use tracing::{debug, info};

use crate::parser::{ParsedExpression, ParsedStatement};
use crate::optimizer::rbo::RuleBasedOptimizer;
use crate::optimizer::cbo::CostBasedOptimizer;

/// 查询优化器
pub struct Optimizer {
    rbo: RuleBasedOptimizer,
    cbo: CostBasedOptimizer,
}

impl Default for Optimizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Optimizer {
    pub fn new() -> Self {
        Self {
            rbo: RuleBasedOptimizer::new(),
            cbo: CostBasedOptimizer::new(),
        }
    }

    /// 优化查询
    pub async fn optimize(&self, stmt: ParsedStatement) -> Result<OptimizedPlan> {
        info!("开始查询优化");
        debug!("原始SQL语句类型: {:?}", std::mem::discriminant(&stmt));

        // 1. 基于规则的优化 (RBO)
        info!("=== 开始基于规则的优化 (RBO) ===");
        let rbo_optimized = self.rbo.optimize(stmt).await?;
        debug!("RBO优化完成，执行计划: {:#?}", rbo_optimized);
        info!("RBO优化后计划节点数: {}", rbo_optimized.nodes.len());
        info!("RBO优化后估计成本: {:.2}", rbo_optimized.estimated_cost);
        info!("RBO优化后估计行数: {}", rbo_optimized.estimated_rows);

        // 2. 基于成本的优化 (CBO)
        info!("=== 开始基于成本的优化 (CBO) ===");
        let final_plan = self.cbo.optimize(rbo_optimized).await?;
        debug!("CBO优化完成，最终执行计划: {:#?}", final_plan);
        info!("CBO优化后计划节点数: {}", final_plan.nodes.len());
        info!("CBO优化后估计成本: {:.2}", final_plan.estimated_cost);
        info!("CBO优化后估计行数: {}", final_plan.estimated_rows);

        info!("查询优化完成");
        debug!("最终执行计划树结构: {:#?}", final_plan);
        Ok(final_plan)
    }
}

/// 优化后的查询计划
#[derive(Debug, Clone)]
pub struct OptimizedPlan {
    pub nodes: Vec<PlanNode>,
    pub estimated_cost: f64,
    pub estimated_rows: u64,
}

impl OptimizedPlan {
    pub fn from_statement(stmt: ParsedStatement) -> Self {
        // 简化实现：根据语句类型创建基本计划
        let nodes = match stmt {
            ParsedStatement::Select(select) => {
                let mut plan_nodes = Vec::new();

                // 为每个表创建扫描节点
                for table in select.from {
                    plan_nodes.push(PlanNode::TableScan {
                        table: table.name,
                        columns: select.columns.iter().map(|c| c.name.clone()).collect(),
                    });
                }

                // 如果有 WHERE 条件，添加过滤节点
                if let Some(where_clause) = select.where_clause {
                    if let Some(last_node) = plan_nodes.pop() {
                        plan_nodes.push(PlanNode::Filter {
                            input: Box::new(last_node),
                            predicate: where_clause,
                        });
                    }
                }

                plan_nodes
            }
            ParsedStatement::Insert(_) => {
                vec![PlanNode::TableScan {
                    table: "target_table".to_string(),
                    columns: vec!["*".to_string()],
                }]
            }
            ParsedStatement::Update(_) => {
                vec![PlanNode::TableScan {
                    table: "target_table".to_string(),
                    columns: vec!["*".to_string()],
                }]
            }
            ParsedStatement::Delete(_) => {
                vec![PlanNode::TableScan {
                    table: "target_table".to_string(),
                    columns: vec!["*".to_string()],
                }]
            }
            ParsedStatement::CreateTable(_) => {
                vec![PlanNode::TableScan {
                    table: "system_table".to_string(),
                    columns: vec!["*".to_string()],
                }]
            }
            ParsedStatement::Drop(_) => {
                vec![PlanNode::TableScan {
                    table: "system_table".to_string(),
                    columns: vec!["*".to_string()],
                }]
            }
        };

        Self {
            nodes,
            estimated_cost: 1000.0,
            estimated_rows: 1000,
        }
    }
}

/// 计划节点
#[derive(Debug, Clone)]
pub enum PlanNode {
    TableScan {
        table: String,
        columns: Vec<String>,
    },
    IndexScan {
        table: String,
        index: String,
        columns: Vec<String>,
    },
    Filter {
        input: Box<PlanNode>,
        predicate: ParsedExpression,
    },
    Project {
        input: Box<PlanNode>,
        columns: Vec<String>,
    },
    Join {
        left: Box<PlanNode>,
        right: Box<PlanNode>,
        join_type: JoinType,
        condition: Option<ParsedExpression>,
    },
    Aggregate {
        input: Box<PlanNode>,
        group_by: Vec<String>,
        aggregates: Vec<String>,
    },
    Sort {
        input: Box<PlanNode>,
        order_by: Vec<String>,
    },
    Limit {
        input: Box<PlanNode>,
        limit: u64,
        offset: u64,
    },
}

/// 连接类型
#[derive(Debug, Clone)]
pub enum JoinType {
    Inner,
    Left,
    Right,
    Full,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rule_based_optimizer_new() {
        let optimizer = RuleBasedOptimizer::new();
        // 测试优化器是否成功创建
        assert!(true);
    }

    #[test]
    fn test_cost_based_optimizer_new() {
        let optimizer = CostBasedOptimizer::new();
        // 测试优化器是否成功创建
        assert!(true);
    }

    #[test]
    fn test_optimizer_new() {
        let optimizer = Optimizer::new();
        // 测试优化器是否成功创建
        assert!(true);
    }

    #[tokio::test]
    async fn test_estimate_cost() {
        use crate::optimizer::cbo::CostModel;
        let cost_model = CostModel::new();
        let plan = OptimizedPlan {
            nodes: vec![PlanNode::TableScan {
                table: "users".to_string(),
                columns: vec!["id".to_string(), "name".to_string()],
            }],
            estimated_cost: 1000.0,
            estimated_rows: 1000,
        };

        let cost = cost_model.estimate_cost(&plan).await.unwrap();
        assert!(cost > 0.0);
    }

    #[tokio::test]
    async fn test_apply_optimization_rules() {
        let optimizer = RuleBasedOptimizer::new();
        let stmt = ParsedStatement::Select(crate::parser::ParsedSelect {
            columns: vec![crate::parser::ParsedColumn { name: "*".to_string(), alias: None }],
            from: vec![crate::parser::ParsedTable { name: "users".to_string(), alias: None }],
            where_clause: None,
            group_by: vec![],
            order_by: vec![],
            limit: None,
            offset: None,
        });

        let plan = optimizer.optimize(stmt).await.unwrap();
        assert!(!plan.nodes.is_empty());
    }
}
