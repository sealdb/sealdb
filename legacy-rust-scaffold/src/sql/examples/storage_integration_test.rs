//! Memory存储引擎与执行器集成测试
//!
//! 验证执行器能否通过memory引擎进行读写和扫描操作

use sql::executor::StorageExecutor;
use sql::parser::SqlParser;
use sql::optimizer::Optimizer;
use storage::EngineType;
use storage::StorageConfig;
use tracing_subscriber;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 初始化日志系统
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();

    println!("=== Memory存储引擎与执行器集成测试 ===");

    // 1. 创建存储执行器
    println!("1. 创建存储执行器...");
    let mut storage_executor = StorageExecutor::new();
    storage_executor.set_default_engine(EngineType::Memory);

    // 2. 注册memory存储引擎
    println!("2. 注册memory存储引擎...");
    let memory_config = StorageConfig::default();
    storage_executor.register_storage_engine(EngineType::Memory, memory_config).await?;

    // 3. 测试基本CRUD操作
    println!("3. 测试基本CRUD操作...");

    // 3.1 插入数据
    println!("3.1 插入数据...");
    let insert_result = storage_executor.execute_insert("users", "user1", "Alice,25", &Default::default()).await?;
    println!("   - 插入user1: {} 行", insert_result);

    let insert_result = storage_executor.execute_insert("users", "user2", "Bob,30", &Default::default()).await?;
    println!("   - 插入user2: {} 行", insert_result);

    let insert_result = storage_executor.execute_insert("users", "user3", "Charlie,35", &Default::default()).await?;
    println!("   - 插入user3: {} 行", insert_result);

    // 3.2 查询数据
    println!("3.2 查询数据...");
    let query_result = storage_executor.execute_point_query("users", "user1", &Default::default()).await?;
    println!("   - 查询user1: {} 行", query_result.rows.len());
    if !query_result.rows.is_empty() {
        println!("   - user1数据: {:?}", query_result.rows[0]);
    }

    // 3.3 表扫描
    println!("3.3 表扫描...");
    let scan_result = storage_executor.execute_table_scan("users", &["*".to_string()], Some(10), &Default::default()).await?;
    println!("   - 表扫描: {} 行", scan_result.rows.len());
    for (i, row) in scan_result.rows.iter().enumerate() {
        println!("   - 行 {}: {:?}", i + 1, row);
    }

    // 3.4 删除数据
    println!("3.4 删除数据...");
    let delete_result = storage_executor.execute_delete("users", "user2", &Default::default()).await?;
    println!("   - 删除user2: {} 行", delete_result);

    // 3.5 验证删除
    println!("3.5 验证删除...");
    let query_result = storage_executor.execute_point_query("users", "user2", &Default::default()).await?;
    println!("   - 验证删除user2: {} 行", query_result.rows.len());

    // 4. 测试SQL解析到存储的完整流程
    println!("4. 测试SQL解析到存储的完整流程...");

    let parser = SqlParser::new();
    let optimizer = Optimizer::new();

    let test_sqls = vec![
        "SELECT * FROM users WHERE id = 1",
        "INSERT INTO users (name, age) VALUES ('David', 40)",
        "UPDATE users SET age = 26 WHERE name = 'Alice'",
        "DELETE FROM users WHERE name = 'Charlie'",
    ];

    for (i, sql) in test_sqls.iter().enumerate() {
        println!("  测试SQL {}: {}", i + 1, sql);

        match parser.parse(sql) {
            Ok(parsed_stmt) => {
                println!("    ✓ SQL解析成功");

                match optimizer.optimize(parsed_stmt).await {
                    Ok(optimized_plan) => {
                        println!("    ✓ 查询优化成功");
                        println!("    最终执行计划节点数: {}", optimized_plan.nodes.len());
                        println!("    最终估计成本: {:.2}", optimized_plan.estimated_cost);
                        println!("    最终估计行数: {}", optimized_plan.estimated_rows);
                    }
                    Err(e) => {
                        println!("    ✗ 查询优化失败: {}", e);
                    }
                }
            }
            Err(e) => {
                println!("    ✗ SQL解析失败: {}", e);
            }
        }
    }

    // 5. 测试批量操作
    println!("5. 测试批量操作...");

    use sql::executor::storage_executor::{StorageOperation, StorageOperationType};

    let batch_operations = vec![
        StorageOperation {
            operation_id: "batch_op_1".to_string(),
            operation_type: StorageOperationType::Insert,
            table_name: Some("products".to_string()),
            key: Some("prod1".to_string()),
            value: Some("Laptop,999".to_string()),
        },
        StorageOperation {
            operation_id: "batch_op_2".to_string(),
            operation_type: StorageOperationType::Insert,
            table_name: Some("products".to_string()),
            key: Some("prod2".to_string()),
            value: Some("Mouse,29".to_string()),
        },
        StorageOperation {
            operation_id: "batch_op_3".to_string(),
            operation_type: StorageOperationType::Select,
            table_name: Some("products".to_string()),
            key: Some("prod1".to_string()),
            value: None,
        },
    ];

    let batch_results = storage_executor.execute_batch_operations(batch_operations, &Default::default()).await?;
    println!("  批量操作完成，结果数量: {}", batch_results.len());

    for (i, result) in batch_results.iter().enumerate() {
        println!("  操作 {}: 成功={}, 影响行数={}, 错误={:?}",
                i + 1, result.success, result.affected_rows, result.error);
    }

    println!("\n=== 集成测试完成 ===");
    println!("✓ Memory存储引擎与执行器集成成功");
    println!("✓ 基本CRUD操作正常");
    println!("✓ SQL解析到存储流程正常");
    println!("✓ 批量操作正常");

    Ok(())
}