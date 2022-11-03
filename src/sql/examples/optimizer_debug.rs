//! SQL 优化器 Debug 日志示例
//!
//! 展示如何查看RBO和CBO优化后的执行计划

use sql::parser::SqlParser;
use sql::optimizer::Optimizer;
use tracing_subscriber;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 初始化日志系统
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();

    println!("=== SQL 优化器 Debug 日志示例 ===");

    let parser = SqlParser::new();
    let optimizer = Optimizer::new();

    // 测试不同类型的SQL语句优化
    let test_sqls = vec![
        "SELECT * FROM users WHERE id = 1",
        "SELECT name, age FROM users WHERE age > 25 ORDER BY age",
        "INSERT INTO users (name, age) VALUES ('Alice', 25)",
        "UPDATE users SET age = 26 WHERE id = 1",
        "DELETE FROM users WHERE id = 1",
    ];

    for (i, sql) in test_sqls.iter().enumerate() {
        println!("\n--- 测试 SQL {} ---", i + 1);
        println!("输入SQL: {}", sql);
        
        // 1. 解析SQL
        match parser.parse(sql) {
            Ok(parsed_stmt) => {
                println!("✓ SQL解析成功");
                
                // 2. 优化查询
                match optimizer.optimize(parsed_stmt).await {
                    Ok(optimized_plan) => {
                        println!("✓ 查询优化成功");
                        println!("最终执行计划节点数: {}", optimized_plan.nodes.len());
                        println!("最终估计成本: {:.2}", optimized_plan.estimated_cost);
                        println!("最终估计行数: {}", optimized_plan.estimated_rows);
                    }
                    Err(e) => {
                        println!("✗ 查询优化失败: {}", e);
                    }
                }
            }
            Err(e) => {
                println!("✗ SQL解析失败: {}", e);
            }
        }
    }

    println!("\n=== 示例完成 ===");
    Ok(())
} 