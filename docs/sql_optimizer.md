# SQL优化架构

## 概述

SealDB的SQL优化采用两阶段架构：基于规则的优化（RBO）和基于成本的优化（CBO）。这种分离设计使得优化过程更加清晰和高效。

## 架构图

```
SQL文本
    ↓
Parser (解析器)
    ↓
AST (抽象语法树)
    ↓
Planner (RBO - 基于规则的优化)
    ↓
初始执行计划
    ↓
Optimizer (CBO - 基于成本的优化)
    ↓
优化执行计划
    ↓
Executor (执行器)
    ↓
结果
```

## 模块职责

### 1. Parser (解析器)

**位置**: `src/sql/parser/`
**职责**: 将SQL文本转换为AST

**子模块**:
- `antlr4/`: ANTLR4解析器实现
- `pg/`: PostgreSQL风格解析器实现

**功能**:
- SQL语法解析
- 词法分析
- 语法分析
- AST生成

### 2. Planner (RBO - 基于规则的优化)

**位置**: `src/sql/planner/`
**职责**: 将AST转换为初始执行计划，应用基于规则的优化

**核心文件**:
- `planner.cpp`: 执行计划生成器
- `planner.h`: 执行计划生成器头文件

**RBO优化规则**:
- **谓词下推** (Predicate Pushdown): 将WHERE条件尽可能下推到数据源
- **列裁剪** (Column Pruning): 只选择需要的列
- **子查询优化** (Subquery Optimization): 将子查询转换为连接
- **常量折叠** (Constant Folding): 在编译时计算常量表达式
- **表达式简化** (Expression Simplification): 简化复杂表达式

**示例**:
```sql
-- 原始SQL
SELECT * FROM users WHERE age > 18 AND name LIKE 'John%';

-- RBO优化后
SELECT id, name, age FROM users WHERE age > 18 AND name LIKE 'John%';
-- (列裁剪 + 谓词下推)
```

### 3. Optimizer (CBO - 基于成本的优化)

**位置**: `src/sql/optimizer/`
**职责**: 基于统计信息和成本模型进行查询优化

**核心文件**:
- `optimizer.cpp`: 查询优化器
- `cost_estimator.cpp`: 成本估算器
- `index_selector.cpp`: 索引选择器
- `statistics_manager.cpp`: 统计信息管理器

**CBO优化策略**:
- **连接重排序** (Join Reordering): 选择最优的连接顺序
- **索引选择** (Index Selection): 选择最合适的索引
- **访问路径选择** (Access Path Selection): 选择最优的数据访问方式
- **并行执行计划生成** (Parallel Execution Planning): 生成并行执行计划

**示例**:
```sql
-- 原始执行计划
SELECT * FROM orders o JOIN customers c ON o.customer_id = c.id
WHERE c.country = 'USA' AND o.amount > 1000;

-- CBO优化后
SELECT o.id, o.amount, c.name
FROM customers c JOIN orders o ON c.id = o.customer_id
WHERE c.country = 'USA' AND o.amount > 1000;
-- (使用索引 + 连接重排序)
```

### 4. Executor (执行器)

**位置**: `src/sql/executor/`
**职责**: 执行优化后的执行计划

**核心文件**:
- `executor.cpp`: SQL执行器
- `physical_operator.h`: 物理操作符定义

**物理操作符**:
- `TableScanOperator`: 表扫描
- `IndexScanOperator`: 索引扫描
- `FilterOperator`: 过滤
- `ProjectOperator`: 投影
- `JoinOperator`: 连接
- `AggregateOperator`: 聚合
- `SortOperator`: 排序
- `LimitOperator`: 限制

## 优化流程

### 阶段1: RBO (Planner)

1. **AST分析**: 分析SQL AST的结构
2. **规则应用**: 应用基于规则的优化
3. **初始计划生成**: 生成初始执行计划

```cpp
// RBO示例
std::unique_ptr<ExecutionPlan> Planner::plan_select(SelectStatement* stmt) {
    // 1. 分析AST
    auto tables = extract_tables(stmt);
    auto conditions = extract_conditions(stmt);

    // 2. 应用RBO规则
    auto optimized_stmt = apply_predicate_pushdown(stmt);
    auto optimized_stmt = apply_column_pruning(optimized_stmt);

    // 3. 生成初始计划
    return generate_initial_plan(optimized_stmt);
}
```

### 阶段2: CBO (Optimizer)

1. **统计信息收集**: 收集表和索引的统计信息
2. **成本估算**: 估算不同执行计划的成本
3. **最优计划选择**: 选择成本最低的执行计划

```cpp
// CBO示例
std::unique_ptr<ExecutionPlan> Optimizer::optimize_select(SelectStatement* stmt) {
    // 1. 生成初始计划
    Planner planner;
    auto plan = planner.plan_select(stmt);

    // 2. 应用CBO优化
    plan = apply_join_reordering(std::move(plan));
    plan = apply_index_selection(std::move(plan));
    plan = apply_access_path_selection(std::move(plan));

    return plan;
}
```

## 统计信息管理

### 统计信息类型

1. **表统计信息**:
   - 行数 (row_count)
   - 页数 (page_count)
   - 平均行大小 (avg_row_size)

2. **列统计信息**:
   - 不同值数量 (distinct_values)
   - 最小值/最大值 (min_value/max_value)
   - NULL值比例 (null_fraction)
   - 直方图 (histogram_bounds)

3. **索引统计信息**:
   - 索引高度 (height)
   - 叶子页数 (leaf_pages)
   - 选择性 (selectivity)

### 统计信息更新

```cpp
// 更新表统计信息
statistics_manager.update_table_stats("users", {
    .row_count = 10000,
    .page_count = 500,
    .avg_row_size = 128.0
});

// 更新列统计信息
statistics_manager.update_column_stats("users", "age", {
    .distinct_values = 50,
    .min_value = 18,
    .max_value = 80,
    .null_fraction = 0.0
});
```

## 成本模型

### 扫描成本

```cpp
double CostEstimator::estimate_scan_cost(const std::string& table_name) {
    auto stats = statistics_manager.get_table_stats(table_name);
    if (!stats) return 0.0;

    // 扫描成本 = 页数 * 每页I/O成本
    return stats->page_count * IO_COST_PER_PAGE;
}
```

### 连接成本

```cpp
double CostEstimator::estimate_join_cost(const std::string& left_table,
                                       const std::string& right_table,
                                       const std::unique_ptr<Expression>& condition) {
    auto left_stats = statistics_manager.get_table_stats(left_table);
    auto right_stats = statistics_manager.get_table_stats(right_table);

    // 连接成本 = 左表行数 * 右表行数 * 连接选择性
    double selectivity = estimate_join_selectivity(condition);
    return left_stats->row_count * right_stats->row_count * selectivity;
}
```

## 配置选项

### CMake配置

```cmake
# 启用RBO
option(USE_RBO "Enable Rule-Based Optimization" ON)

# 启用CBO
option(USE_CBO "Enable Cost-Based Optimization" ON)

# 统计信息更新频率
option(STATS_UPDATE_FREQUENCY "Statistics update frequency (hours)" 24)
```

### 运行时配置

```cpp
// 配置优化器
OptimizerConfig config;
config.enable_rbo = true;
config.enable_cbo = true;
config.stats_update_interval = 3600; // 1小时

Optimizer optimizer(config);
```

## 性能监控

### 优化指标

1. **优化时间**: RBO和CBO的执行时间
2. **计划质量**: 执行计划的成本估算
3. **统计信息准确性**: 统计信息与实际数据的偏差

### 监控接口

```cpp
class OptimizationMetrics {
public:
    double rbo_time_ms;
    double cbo_time_ms;
    double total_cost;
    size_t plan_complexity;

    void record_rbo_time(double time_ms);
    void record_cbo_time(double time_ms);
    void record_plan_cost(double cost);
};
```

## 扩展指南

### 添加新的RBO规则

1. 在`planner.cpp`中添加新规则
2. 在`planner.h`中声明规则接口
3. 更新测试用例

### 添加新的CBO策略

1. 在`optimizer.cpp`中添加新策略
2. 在`cost_estimator.cpp`中添加成本估算
3. 更新统计信息管理器

### 添加新的物理操作符

1. 在`physical_operator.h`中定义新操作符
2. 在`executor.cpp`中实现操作符
3. 更新执行器工厂

## 最佳实践

1. **统计信息维护**: 定期更新统计信息以确保CBO的准确性
2. **规则优先级**: 合理设置RBO规则的优先级
3. **成本模型调优**: 根据实际硬件调整成本模型参数
4. **监控和调优**: 持续监控优化效果并进行调优