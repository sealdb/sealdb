---
name: plan-debug
description: 分层调试 TiDB Rust 移植中的优化器/执行器问题：从 AST、逻辑计划、RBO、CBO 到 Executor/Chunk/Cop，定位第一处偏差。适用于“EXPLAIN 不对/计划选错/结果不对”。
---

# Skill: 计划与执行调试

## 适用

- `EXPLAIN` 与 TiDB 不一致；
- 选错索引/Join 算法；
- 结果行数/值/类型错误；
- 执行卡住/panic/OOM。

## 总原则

**先定位层，再改代码；先与 Oracle 对比，再猜原因。**  
禁止 shotgun debugging（连续随机改）。

## 分层检查

### 1. Parser / AST

```bash
cargo run --bin tidb-server -- --parse "SQL" --ast-json > /tmp/rust.ast.json
go run ./tools/oracle/cmd/tidb-oracle parse --sql "SQL" --pretty > /tmp/tidb.ast.json
diff -u /tmp/tidb.ast.json /tmp/rust.ast.json
```

关注：节点类型、字段、别名、`Flag`、`Restore`。

### 2. Preprocess / PlanBuilder

- InfoSchema 版本是否正确？
- 表/列是否解析到正确 ID？
- 逻辑算子顺序：`Limit → Sort → Projection → Selection → Join → DataSource`？
- `optFlag` 是否打开正确规则位？

### 3. RBO

逐条规则检查（按 `optRuleList` 顺序）：

- 规则是否执行？`changed`？
- PPD 是否把谓词推到正确侧？
- ColumnPrune 是否剪掉需要列？
- JoinReorder 是否受 `straight_join`/hint 约束？

### 4. CBO / FindBestTask

- `DeriveStats` 行数/NDV 是否合理？
- `ExhaustPhysicalPlans` 候选是否齐全？
- Skyline 是否错误剪枝？
- Task 类型（Root/Cop）是否符合预期？
- 代价因子是否与 TiDB 一致？

### 5. Executor

- `Open` 顺序、子节点 `Close`；
- `Next` 前 `Reset`；
- 空 Chunk = EOF；
- `sel` 是否正确传递；
- 表达式类型/Nullable 是否匹配；
- 并发 worker 是否泄漏/死锁。

### 6. DistSQL / KV

- KeyRange/tablecodec 编码；
- Region 切分/epoch；
- DAG 算子；
- Cop 响应编码；
- 锁/冲突/重试。

## 工具

```bash
# 日志
RUST_LOG=tidb::planner=debug,tidb::executor=trace cargo run --bin tidb-server -- --execute "..."

# 计划
cargo run --bin tidb-server -- --explain "SQL"
cargo run --bin tidb-server -- --explain-analyze "SQL"

# Trace（Phase 4+）
cargo run --bin tidb-server -- --execute "SQL" --trace-out /tmp/trace.json

# PD/TiKV
pd-ctl -u http://127.0.0.1:2379 region key <hex>
pd-ctl -u http://127.0.0.1:2379 store
docker logs -f <tikv-container>
```

## 输出

修复后必须留下：

- 根因（层 + 文件/函数）；
- 最小复现 SQL；
- 新增/更新的回归测试；
- 如涉及设计偏差，新增 ADR。

## 禁止

- ❌ 不看计划就改优化器。
- ❌ 用 `unwrap`/忽略错误“让它跑”。
- ❌ 删除/放宽测试。
- ❌ 只修一个用例，不跑相邻回归。

## 完成标准

- [ ] 定位到具体层/文件。
- [ ] 最小复现 + 回归测试。
- [ ] 与 Oracle 对齐（AST/Plan/Result 对应层）。
- [ ] 调试记录写入 `project/notes/`。
