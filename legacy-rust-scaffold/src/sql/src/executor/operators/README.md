# 操作符重构说明

## 概述

原来的 `operators.rs` 文件（5108行）已经被重构为多个功能模块，以提高代码的可维护性和可读性。

## 重构结构

### 1. `operator_trait.rs`
- **功能**: 定义基础操作符 trait
- **内容**: `Operator` trait 及其实现

### 2. `scan_operators.rs`
- **功能**: 扫描相关的操作符
- **包含的操作符**:
  - `ScanOperator`: 基础表扫描
  - `IndexScanOperator`: 索引扫描
  - `SeqScanOperator`: 顺序扫描
  - `EnhancedIndexScanOperator`: 增强索引扫描（支持多种索引类型）
  - `BitmapScanOperator`: 位图扫描

### 3. `join_operators.rs`
- **功能**: 连接相关的操作符
- **包含的操作符**:
  - `JoinOperator`: 基础连接操作符
  - `NestedLoopJoinOperator`: 嵌套循环连接
  - `HashJoinOperator`: 哈希连接
  - `MergeJoinOperator`: 归并连接

### 4. `aggregate_operators.rs`
- **功能**: 聚合相关的操作符
- **包含的操作符**:
  - `AggregateOperator`: 基础聚合操作符
  - `HashAggOperator`: 哈希聚合
  - `GroupAggOperator`: 分组聚合

### 5. `sort_operators.rs`
- **功能**: 排序相关的操作符
- **包含的操作符**:
  - `SortOperator`: 基础排序操作符
  - `ExternalSortOperator`: 外部排序
  - `TopNOperator`: TopN 排序

### 6. `set_operators.rs`
- **功能**: 集合操作相关的操作符
- **包含的操作符**:
  - `UnionOperator`: 并集操作
  - `IntersectOperator`: 交集操作
  - `ExceptOperator`: 差集操作

### 7. `batch_operators.rs`
- **功能**: 批处理相关的操作符
- **包含的操作符**:
  - `BatchScanOperator`: 批处理扫描
  - `BatchIndexScanOperator`: 批处理索引扫描
  - `BatchJoinOperator`: 批处理连接
  - `BatchAggregateOperator`: 批处理聚合
  - `BatchSortOperator`: 批处理排序

### 8. `parallel_operators.rs`
- **功能**: 并行处理相关的操作符
- **包含的操作符**:
  - `ParallelScanTask`: 并行扫描任务
  - `ParallelIndexScanTask`: 并行索引扫描任务
  - `ParallelJoinTask`: 并行连接任务
  - `ParallelAggregateTask`: 并行聚合任务
  - `ParallelSortTask`: 并行排序任务
  - `ParallelScanOperator`: 并行扫描操作符
  - `ParallelSortOperator`: 并行排序操作符

### 9. `distributed_operators.rs`
- **功能**: 分布式处理相关的操作符
- **包含的操作符**:
  - `ShardScanOperator`: 分片扫描操作符
  - `DistributedAggOperator`: 分布式聚合操作符

## 使用方式

### 导入操作符

```rust
use crate::executor::{
    Operator,
    ScanOperator,
    JoinOperator,
    AggregateOperator,
    SortOperator,
    UnionOperator,
    // ... 其他操作符
};
```

### 创建操作符实例

```rust
// 创建扫描操作符
let scan_op = ScanOperator::new(
    "table_name".to_string(),
    vec!["id".to_string(), "name".to_string()],
    buffer_pool.clone(),
    memory_manager.clone(),
);

// 执行操作符
let result = scan_op.execute().await?;
```

## 重构优势

1. **模块化**: 每个文件专注于特定类型的操作符
2. **可维护性**: 更容易找到和修改特定功能
3. **可扩展性**: 新增操作符类型时不会影响其他模块
4. **可读性**: 代码结构更清晰，便于理解
5. **编译效率**: 修改某个模块时只需要重新编译该模块

## 文件大小对比

- **重构前**: `operators.rs` - 5108行
- **重构后**: 9个文件，每个文件约200-800行

## 注意事项

1. 所有操作符都实现了 `Operator` trait
2. 每个模块都有相应的测试用例
3. 模块间的依赖关系通过 `mod.rs` 文件管理
4. 公共接口通过 `mod.rs` 重新导出 