use sql::{SqlEngine, demonstrate_sql_processing};
use tracing::{info, Level};
use tracing_subscriber;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 初始化日志
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .init();

    info!("=== SealDB SQL 处理流程演示程序 ===");

    // 方法1: 使用演示函数
    demonstrate_sql_processing().await?;

    // 方法2: 直接使用 SqlEngine
    let engine = SqlEngine::new();

    let test_queries = vec![
        "SELECT id, name FROM users WHERE age > 18",
        "SELECT id, name, age FROM users WHERE age > 18 ORDER BY name",
        "SELECT u.id, u.name, p.title FROM users u JOIN posts p ON u.id = p.user_id WHERE u.age > 18",
    ];

    for (i, sql) in test_queries.iter().enumerate() {
        info!("=== 测试查询 {} ===", i + 1);
        info!("SQL: {}", sql);

        // 1. 规划阶段
        info!("1. 规划阶段 (RBO)");
        match engine.plan_query(sql).await {
            Ok(plan) => info!("✓ 规划成功: {:?}", plan),
            Err(e) => info!("✗ 规划失败: {}", e),
        }

        // 2. 优化阶段
        info!("2. 优化阶段 (CBO)");
        match engine.optimize_query(sql).await {
            Ok(optimized_plan) => info!("✓ 优化成功: {:?}", optimized_plan),
            Err(e) => info!("✗ 优化失败: {}", e),
        }

        // 3. 完整执行
        info!("3. 完整执行");
        match engine.execute_query(sql).await {
            Ok(result) => {
                info!("✓ 执行成功!");
                info!("  返回列数: {}", result.columns.len());
                info!("  返回行数: {}", result.row_count);
                info!("  执行时间: {}ms", result.execution_time_ms);
            },
            Err(e) => info!("✗ 执行失败: {}", e),
        }

        info!("");
    }

    info!("=== 演示程序结束 ===");
    Ok(())
}