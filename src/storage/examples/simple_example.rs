//! 简化的存储层示例

use storage::*;

#[tokio::main]
async fn main() -> Result<()> {
    println!("=== SealDB 存储层示例 ===");

    // 初始化存储层
    init_storage_layer().await?;

    // 创建内存引擎
    let mut engine = MemoryEngine::new();
    let config = StorageConfig::default();
    engine.initialize(&config).await?;

    // 基本操作测试
    let context = StorageContext::default();
    let options = StorageOptions::default();

    // PUT 操作
    let key = b"test_key".to_vec();
    let value = b"test_value".to_vec();
    engine.put(&key, &value, &context, &options).await?;
    println!("PUT 操作完成");

    // GET 操作
    let result = engine.get(&key, &context, &options).await?;
    println!("GET 操作结果: {:?}", result.value);

    // SCAN 操作
    let scan_result = engine.scan(&b"test".to_vec(), &b"test\xff".to_vec(), 10, &context, &options).await?;
    println!("SCAN 操作结果: {} 条记录", scan_result.value.len());

    // 批量操作
    let batch_keys = vec![b"key1".to_vec(), b"key2".to_vec()];
    let batch_values = vec![b"value1".to_vec(), b"value2".to_vec()];
    let batch_data: Vec<KeyValue> = batch_keys.iter()
        .zip(batch_values.iter())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    engine.batch_put(&batch_data, &context, &options).await?;
    println!("批量 PUT 操作完成");

    // 事务测试
    let mut transaction = engine.begin_transaction(&context, &options).await?;
    println!("事务开始: {}", transaction.transaction_id());

    let tx_key = b"tx_key".to_vec();
    let tx_value = b"tx_value".to_vec();
    transaction.put(&tx_key, &tx_value, &options).await?;

    transaction.commit().await?;
    println!("事务提交完成");

    // 获取统计信息
    let stats = engine.get_stats().await?;
    println!("统计信息: 总操作数={}, 成功操作数={}",
             stats.total_operations, stats.successful_operations);

    println!("=== 示例完成 ===");
    Ok(())
}