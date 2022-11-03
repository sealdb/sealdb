//! SQL 引擎与存储层集成示例
//!
//! 展示如何使用存储感知的执行器

use sql::executor::{StorageExecutor, StorageOperation, StorageOperationType};
use sql::storage::StorageHandler;
use storage::common::{EngineType, StorageConfig};

#[tokio::main]
async fn main() -> Result<()> {
    // 初始化日志
    tracing_subscriber::fmt::init();

    println!("=== SQL 引擎与存储层集成示例 ===");

    // 1. 创建存储处理器
    println!("1. 创建存储处理器...");
    let mut storage_handler = StorageHandler::new();
    storage_handler.set_default_engine(EngineType::Memory);

    // 2. 注册存储引擎
    println!("2. 注册存储引擎...");
    let memory_config = StorageConfig::default();
    storage_handler.register_engine(EngineType::Memory, memory_config).await?;

    // 3. 创建存储感知执行器
    println!("3. 创建存储感知执行器...");
    let mut storage_executor = StorageExecutor::new();
    storage_executor.set_default_engine(EngineType::Memory);

    // 4. 注册存储引擎到执行器
    println!("4. 注册存储引擎到执行器...");
    let executor_config = StorageConfig::default();
    storage_executor.register_storage_engine(EngineType::Memory, executor_config).await?;

    // 5. 创建执行上下文
    println!("5. 创建执行上下文...");
    let context = sql::executor::execution_models::ExecutionContext::new();

    // 6. 执行基本存储操作
    println!("6. 执行基本存储操作...");

    // 插入数据
    let insert_result = storage_executor.execute_insert("users", "user1", "Alice", &context).await?;
    println!("   - 插入用户1: {} 行", insert_result);

    let insert_result = storage_executor.execute_insert("users", "user2", "Bob", &context).await?;
    println!("   - 插入用户2: {} 行", insert_result);

    let insert_result = storage_executor.execute_insert("users", "user3", "Charlie", &context).await?;
    println!("   - 插入用户3: {} 行", insert_result);

    // 查询数据
    let query_result = storage_executor.execute_point_query("users", "user1", &context).await?;
    println!("   - 查询用户1: {} 行", query_result.row_count);
    if !query_result.rows.is_empty() {
        println!("   - 用户1数据: {:?}", query_result.rows[0]);
    }

    // 表扫描
    let scan_result = storage_executor.execute_table_scan("users", &["value".to_string()], Some(10), &context).await?;
    println!("   - 表扫描: {} 行", scan_result.row_count);
    for (i, row) in scan_result.rows.iter().enumerate() {
        println!("   - 行 {}: {:?}", i + 1, row);
    }

    // 删除数据
    let delete_result = storage_executor.execute_delete("users", "user2", &context).await?;
    println!("   - 删除用户2: {} 行", delete_result);

    // 验证删除
    let query_result = storage_executor.execute_point_query("users", "user2", &context).await?;
    println!("   - 验证删除用户2: {} 行", query_result.row_count);

    // 7. 执行批量操作
    println!("7. 执行批量操作...");
    let batch_operations = vec![
        StorageOperation {
            operation_id: "batch_op_1".to_string(),
            operation_type: StorageOperationType::Insert,
            table_name: Some("products".to_string()),
            key: Some("prod1".to_string()),
            value: Some("Laptop".to_string()),
        },
        StorageOperation {
            operation_id: "batch_op_2".to_string(),
            operation_type: StorageOperationType::Insert,
            table_name: Some("products".to_string()),
            key: Some("prod2".to_string()),
            value: Some("Mouse".to_string()),
        },
        StorageOperation {
            operation_id: "batch_op_3".to_string(),
            operation_type: StorageOperationType::Select,
            table_name: Some("products".to_string()),
            key: Some("prod1".to_string()),
            value: None,
        },
    ];

    let batch_results = storage_executor.execute_batch_operations(batch_operations, &context).await?;
    println!("   - 批量操作结果:");
    for result in &batch_results {
        println!("     - 操作 {}: 成功={}, 影响行数={}",
                result.operation_id, result.success, result.affected_rows);
        if let Some(error) = &result.error {
            println!("       错误: {}", error);
        }
    }

    // 8. 测试不同存储引擎
    println!("8. 测试不同存储引擎...");

    // 注册 TiKV 引擎（如果可用）
    let mut tikv_config = StorageConfig::default();
    tikv_config.engine_type = EngineType::TiKV;
    tikv_config.connection_string = "127.0.0.1:2379".to_string();

    // 注意：这里只是演示，实际需要 TiKV 服务运行
    // storage_handler.register_engine(EngineType::TiKV, tikv_config).await?;
    println!("   - TiKV 引擎注册（需要 TiKV 服务）");

    // 9. 性能测试
    println!("9. 性能测试...");
    let start_time = std::time::Instant::now();

    // 批量插入测试
    for i in 0..100 {
        let key = format!("test_key_{}", i);
        let value = format!("test_value_{}", i);
        storage_executor.execute_insert("test_table", &key, &value, &context).await?;
    }

    let elapsed = start_time.elapsed();
    println!("   - 插入 100 条记录耗时: {:?}", elapsed);
    println!("   - 平均每条记录: {:?}", elapsed / 100);

    // 10. 清理测试数据
    println!("10. 清理测试数据...");
    for i in 0..100 {
        let key = format!("test_key_{}", i);
        storage_executor.execute_delete("test_table", &key, &context).await?;
    }
    println!("   - 清理完成");

    println!("=== 示例完成 ===");
    Ok(())
}