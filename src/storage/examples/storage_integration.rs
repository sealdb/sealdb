//! 存储层集成示例
//!
//! 展示如何将计算层的执行计划转换为存储层的执行计划并执行

use storage::*;
use std::collections::HashMap;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 初始化日志
    tracing_subscriber::fmt::init();

    println!("=== SealDB 存储层集成示例 ===");

    // 1. 初始化存储层
    println!("1. 初始化存储层...");
    init_storage_layer().await?;

    // 2. 创建存储引擎工厂
    println!("2. 创建存储引擎工厂...");
    let factory = StorageEngineFactory::new();

    // 3. 注册 TiKV 引擎配置
    println!("3. 注册 TiKV 引擎配置...");
    let mut tikv_config = StorageConfig::default();
    tikv_config.engine_type = EngineType::TiKV;
    tikv_config.connection_string = "127.0.0.1:2379".to_string();
    factory.register_engine(EngineType::TiKV, tikv_config).await?;

    // 4. 注册内存引擎配置
    println!("4. 注册内存引擎配置...");
    let mut memory_config = StorageConfig::default();
    memory_config.engine_type = EngineType::Memory;
    factory.register_engine(EngineType::Memory, memory_config).await?;

    // 5. 创建存储客户端
    println!("5. 创建存储客户端...");
    let client_config = StorageConfig::default();
    let client = StorageClient::new(client_config).await?;

    // 6. 创建计划构建器
    println!("6. 创建计划构建器...");
    let plan_builder = PlanBuilder::new();

    // 7. 模拟从计算层接收的 SQL 执行计划
    println!("7. 模拟 SQL 执行计划...");
    let sql_plan = create_sample_sql_plan();

    // 8. 转换为存储执行计划
    println!("8. 转换为存储执行计划...");
    let context = StorageContext::default();
    let options = StorageOptions::default();

    let storage_plan = plan_builder.create_plan(sql_plan, &context, &options).await?;
    println!("   - 计划 ID: {}", storage_plan.plan_id);
    println!("   - 操作数量: {}", storage_plan.operations.len());
    println!("   - 估计成本: {:.2}", storage_plan.estimated_cost);
    println!("   - 估计行数: {}", storage_plan.estimated_rows);

    // 9. 分析存储计划
    println!("9. 分析存储计划...");
    let analyzer = PlanAnalyzer::new();
    let analysis = analyzer.analyze_plan(&storage_plan, &context, &options).await?;
    println!("   - 推荐引擎: {:?}", analysis.recommended_engine);
    println!("   - 估计延迟: {}ms", analysis.estimated_latency_ms);
    println!("   - 优化建议: {:?}", analysis.optimization_suggestions);
    println!("   - 风险因素: {:?}", analysis.risk_factors);

    // 10. 优化存储计划
    println!("10. 优化存储计划...");
    let optimizer = PlanOptimizer::new();
    let optimized_plan = optimizer.optimize_plan(storage_plan, &context, &options).await?;
    println!("   - 优化后成本: {:.2}", optimized_plan.estimated_cost);
    println!("   - 优化后行数: {}", optimized_plan.estimated_rows);

    // 11. 使用内存引擎执行计划
    println!("11. 使用内存引擎执行计划...");
    let memory_engine = factory.get_engine(EngineType::Memory).await?;

    // 先插入一些测试数据
    let test_data = create_test_data();
    for (key, value) in &test_data {
        memory_engine.put(key, value, &context, &options).await?;
    }
    println!("   - 插入测试数据完成");

    // 执行存储计划
    let result = memory_engine.execute_plan(optimized_plan, &context, &options).await?;
    println!("   - 执行完成");
    println!("   - 总延迟: {}ms", result.value.total_latency_ms);
    println!("   - 总行数: {}", result.value.total_rows);
    println!("   - 操作结果数量: {}", result.value.results.len());

    // 12. 显示执行结果
    println!("12. 显示执行结果...");
    for (i, operation_result) in result.value.results.iter().enumerate() {
        println!("   操作 {}: {:?} - 成功: {}, 影响行数: {}, 延迟: {}ms",
                i + 1,
                operation_result.operation_type,
                operation_result.success,
                operation_result.rows_affected,
                operation_result.latency_ms);
    }

    // 13. 测试事务
    println!("13. 测试事务...");
    let transaction = memory_engine.begin_transaction(&context, &options).await?;
    println!("   - 事务 ID: {}", transaction.transaction_id());

    // 在事务中执行操作
    let tx_key = b"transaction_key".to_vec();
    let tx_value = b"transaction_value".to_vec();
    transaction.put(&tx_key, &tx_value, &options).await?;
    println!("   - 在事务中插入数据");

    // 提交事务
    let mut tx = transaction;
    tx.commit().await?;
    println!("   - 事务提交成功");

    // 验证事务中的数据
    let get_result = memory_engine.get(&tx_key, &context, &options).await?;
    if let Some(value) = get_result.value {
        println!("   - 验证事务数据: {:?}", String::from_utf8_lossy(&value));
    }

    // 14. 测试批量操作
    println!("14. 测试批量操作...");
    let batch_keys = vec![
        b"batch_key1".to_vec(),
        b"batch_key2".to_vec(),
        b"batch_key3".to_vec(),
    ];
    let batch_values = vec![
        b"batch_value1".to_vec(),
        b"batch_value2".to_vec(),
        b"batch_value3".to_vec(),
    ];

    let batch_key_values: Vec<KeyValue> = batch_keys.iter()
        .zip(batch_values.iter())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    memory_engine.batch_put(&batch_key_values, &context, &options).await?;
    println!("   - 批量插入完成");

    let batch_get_result = memory_engine.batch_get(&batch_keys, &context, &options).await?;
    println!("   - 批量获取完成，结果数量: {}", batch_get_result.value.len());

    // 15. 获取统计信息
    println!("15. 获取统计信息...");
    let stats = memory_engine.get_stats().await?;
    println!("   - 总操作数: {}", stats.total_operations);
    println!("   - 成功操作数: {}", stats.successful_operations);
    println!("   - 失败操作数: {}", stats.failed_operations);
    println!("   - 平均延迟: {:.2}ms", stats.avg_latency_ms);

    // 16. 健康检查
    println!("16. 健康检查...");
    let health = memory_engine.health_check().await?;
    println!("   - 引擎健康状态: {}", health);

    // 17. 关闭存储层
    println!("17. 关闭存储层...");
    factory.shutdown_all().await?;
    client.shutdown().await?;
    println!("   - 存储层关闭完成");

    println!("=== 示例执行完成 ===");
    Ok(())
}

/// 创建示例 SQL 执行计划
fn create_sample_sql_plan() -> planner::SqlExecutionPlan {
    planner::SqlExecutionPlan {
        plan_id: "sample_sql_plan".to_string(),
        operations: vec![
            // SELECT 操作
            planner::SqlOperation {
                operation_id: "select_op".to_string(),
                operation_type: planner::SqlOperationType::Select,
                table_name: Some("users".to_string()),
                columns: vec!["id".to_string(), "name".to_string(), "email".to_string()],
                conditions: vec![
                    planner::SqlCondition {
                        column: "status".to_string(),
                        operator: "=".to_string(),
                        value: "active".to_string(),
                        is_indexed: true,
                    }
                ],
                joins: vec![],
                aggregations: vec![],
                order_by: vec![
                    planner::SqlOrderBy {
                        column: "id".to_string(),
                        direction: "ASC".to_string(),
                    }
                ],
                limit: Some(100),
                offset: None,
            },
            // INSERT 操作
            planner::SqlOperation {
                operation_id: "insert_op".to_string(),
                operation_type: planner::SqlOperationType::Insert,
                table_name: Some("users".to_string()),
                columns: vec!["name".to_string(), "email".to_string()],
                conditions: vec![],
                joins: vec![],
                aggregations: vec![],
                order_by: vec![],
                limit: None,
                offset: None,
            },
        ],
        estimated_cost: 25.0,
        estimated_rows: 150,
    }
}

/// 创建测试数据
fn create_test_data() -> HashMap<Key, Value> {
    let mut data = HashMap::new();

    // 用户数据
    data.insert(b"user:1".to_vec(), b"{\"id\":1,\"name\":\"Alice\",\"email\":\"alice@example.com\"}".to_vec());
    data.insert(b"user:2".to_vec(), b"{\"id\":2,\"name\":\"Bob\",\"email\":\"bob@example.com\"}".to_vec());
    data.insert(b"user:3".to_vec(), b"{\"id\":3,\"name\":\"Charlie\",\"email\":\"charlie@example.com\"}".to_vec());

    // 订单数据
    data.insert(b"order:1".to_vec(), b"{\"id\":1,\"user_id\":1,\"amount\":100.50}".to_vec());
    data.insert(b"order:2".to_vec(), b"{\"id\":2,\"user_id\":2,\"amount\":200.75}".to_vec());
    data.insert(b"order:3".to_vec(), b"{\"id\":3,\"user_id\":1,\"amount\":150.25}".to_vec());

    // 产品数据
    data.insert(b"product:1".to_vec(), b"{\"id\":1,\"name\":\"Laptop\",\"price\":999.99}".to_vec());
    data.insert(b"product:2".to_vec(), b"{\"id\":2,\"name\":\"Mouse\",\"price\":29.99}".to_vec());
    data.insert(b"product:3".to_vec(), b"{\"id\":3,\"name\":\"Keyboard\",\"price\":89.99}".to_vec());

    data
}