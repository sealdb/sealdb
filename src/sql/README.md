# SealDB SQL 模块

SealDB SQL 模块提供了完整的 SQL 处理功能，包括解析、优化、执行等。

## 架构概览

```
SQL 查询处理流程:
┌─────────────┐    ┌─────────────┐    ┌─────────────┐    ┌─────────────┐
│   Parser    │───▶│   Planner   │───▶│  Optimizer  │───▶│  Executor   │
│  (解析器)    │    │  (规划器)    │    │  (优化器)    │    │  (执行器)    │
│             │    │   RBO       │    │   CBO       │    │             │
└─────────────┘    └─────────────┘    └─────────────┘    └─────────────┘
                          │                     │
                          ▼                     ▼
                   ┌─────────────┐    ┌─────────────┐
                   │   Storage   │    │ Distributed │
                   │  (存储层)    │    │ (分布式层)   │
                   └─────────────┘    └─────────────┘
```

### 模块职责分工

1. **Parser (解析器)**: 解析 SQL 语句，生成语法树
2. **Planner (规划器)**: 基于规则的优化 (RBO)，包括常量折叠、谓词下推等
3. **Optimizer (优化器)**: 基于成本的优化 (CBO)，包括连接重排序、索引选择等
4. **Executor (执行器)**: 执行优化后的查询计划
5. **Storage (存储层)**: 数据存储抽象，支持多种存储引擎
6. **Distributed (分布式层)**: 分布式执行和节点管理

## 模块结构

```
src/sql/src/
├── lib.rs              # 主入口文件
├── parser/             # SQL 解析器模块
│   ├── mod.rs         # 解析器模块入口
│   └── parser.rs      # SQL 解析器实现
├── planner/            # 查询规划器模块 (RBO)
│   ├── mod.rs         # 规划器模块入口
│   └── rbo.rs         # 基于规则的优化器实现
├── optimizer/          # 查询优化器模块 (CBO)
│   ├── mod.rs         # 优化器模块入口
│   ├── optimizer.rs   # 主优化器（协调 RBO 和 CBO）
│   ├── cbo.rs         # 基于成本的优化器 (CBO)
│   ├── cost_model.rs  # 成本模型
│   └── statistics.rs  # 统计信息管理
├── executor/           # 查询执行器模块
│   ├── mod.rs         # 执行器模块入口
│   ├── executor.rs    # 主执行器
│   ├── execution_models.rs  # 执行模型
│   └── operators.rs   # 操作符实现
├── storage/            # 存储模块
│   ├── mod.rs         # 存储模块入口
│   ├── memory.rs      # 内存管理
│   ├── buffer_pool.rs # 缓冲池
│   ├── cache_manager.rs # 缓存管理
│   └── worker_pool.rs # 工作线程池
└── distributed/        # 分布式模块
    ├── mod.rs         # 分布式模块入口
    └── distributed.rs # 分布式执行
```

## 模块说明

### 解析器模块 (parser)
- **功能**: SQL 语句解析和语法分析
- **主要组件**:
  - `SqlParser`: SQL 解析器
  - `ParsedStatement`: 解析后的语句结构
  - `ParsedExpression`: 解析后的表达式

### 规划器模块 (planner)
- **功能**: 基于规则的查询优化 (RBO - Rule-Based Optimization)
- **主要组件**:
  - `RuleBasedPlanner`: 基于规则的规划器
  - `RuleBasedOptimizer`: 基于规则的优化器
  - `OptimizationRule`: 优化规则 trait
  - `QueryPlan`: 查询计划结构
  - `PlanNode`: 计划节点类型

### 优化器模块 (optimizer)
- **功能**: 基于成本的查询优化 (CBO - Cost-Based Optimization)
- **主要组件**:
  - `Optimizer`: 主优化器，协调 RBO 和 CBO
  - `CostBasedOptimizer`: 基于成本的优化器
  - `CostModel`: 成本模型
  - `StatisticsManager`: 统计信息管理

#### 基于规则的优化 (RBO) - 在 planner 模块中实现
- **常量折叠**: 在编译时计算常量表达式
- **表达式简化**: 简化冗余的布尔表达式
- **子查询扁平化**: 将子查询转换为连接
- **谓词下推**: 将过滤条件下推到数据源
- **列裁剪**: 只选择需要的列
- **连接重排序**: 优化连接顺序
- **索引选择**: 选择合适的索引
- **排序优化**: 利用索引进行排序
- **分组优化**: 优化 GROUP BY 操作
- **去重优化**: 优化 DISTINCT 操作
- **限制优化**: 优化 LIMIT 操作
- **联合优化**: 优化 UNION 操作

#### 基于成本的优化 (CBO)
- **连接重排序**: 使用动态规划优化连接顺序
- **索引选择**: 基于成本选择最佳索引
- **聚合优化**: 优化聚合操作
- **成本估算**: 估算各种操作的成本
- **统计信息**: 管理和使用数据库统计信息

### 执行器模块 (executor)
- **功能**: 查询执行，支持多种执行模型
- **主要组件**:
  - `Executor`: 主执行器
  - `ExecutionEngine`: 执行引擎
  - `VolcanoExecutor`: 火山模型执行器
  - `PipelineExecutor`: 流水线执行器
  - `VectorizedExecutor`: 向量化执行器
  - `MppExecutor`: MPP 执行器

### 存储模块 (storage)
- **功能**: 数据存储和内存管理
- **主要组件**:
  - `MemoryManager`: 内存管理器
  - `BufferPool`: 缓冲池
  - `CacheManager`: 缓存管理器
  - `WorkerPool`: 工作线程池

### 分布式模块 (distributed)
- **功能**: 分布式执行和节点管理
- **主要组件**:
  - `DistributedExecutor`: 分布式执行器
  - `NodeManager`: 节点管理器
  - `ShardManager`: 分片管理器
  - `DistributedTransactionManager`: 分布式事务管理器

## 使用示例

```rust
use sql::{SqlEngine, SqlParser, Optimizer, Executor};

// 创建 SQL 引擎
let engine = SqlEngine::new();

// 执行 SQL 查询
let result = engine.execute("SELECT * FROM users WHERE id = 1").await?;
```

## 设计原则

1. **模块化**: 每个模块职责单一，接口清晰
2. **可扩展**: 支持插件式的优化器和执行器
3. **高性能**: 支持多种执行模型和优化策略
4. **分布式**: 原生支持分布式执行
5. **可观测**: 完整的监控和日志系统

## 开发指南

### 添加新的优化规则
1. 在 `optimizer/rbo.rs` 中实现新的规则
2. 实现 `OptimizationRule` trait
3. 在 `RuleBasedOptimizer::new()` 中注册新规则

### 添加新的成本模型
1. 在 `optimizer/cost_model.rs` 中扩展 `CostModel`
2. 实现新的成本估算方法
3. 在 `CostBasedOptimizer` 中使用新的成本模型

### 添加新的执行模型
1. 在 `executor/` 目录下创建新的执行器
2. 实现 `ExecutionEngine` trait
3. 在 `ExecutionEngine` 中添加新的执行模型

### 添加新的存储后端
1. 在 `storage/` 目录下创建新的存储实现
2. 实现相应的 trait
3. 在存储模块中注册新的后端