# 调试与排障（Debugging）

> 原则：**先定位层，再定位代码；先与 Oracle 对比，再改实现。**  
> 禁止“猜一个改一个”的 shotgun debugging。

---

## 1. 分层定位法

一条 SQL 失败时，按顺序确认它在哪一层偏离 TiDB：

```text
0. 协议/连接
1. SQL 文本 → Token/AST（Parser）
2. AST → 绑定后的 AST（Preprocess / 名字解析）
3. AST → 逻辑计划（PlanBuilder）
4. 逻辑计划 → RBO 后计划（logicalOptimize）
5. RBO → 物理计划（physicalOptimize/FindBestTask）
6. 物理计划 → Executor 树（builder）
7. Open / Next / Close（执行）
8. DistSQL/Cop → TiKV
9. KV/Txn → TiKV / PD
10. 提交 / SchemaChecker
```

每一层都要能回答：

- 输入是什么？（打印/序列化）
- 期望输出是什么？（Oracle）
- 实际输出是什么？
- 第一个不一致的字段/分支在哪里？

---

## 2. 快速命令

```bash
# 最小执行
cargo run --bin tidb-server -- --execute "SELECT 1"

# 指定日志级别
RUST_LOG=debug cargo run --bin tidb-server -- --execute "..."

# 只跑某个测试
cargo test parser::tests::test_select -- --nocapture

# Oracle 对比
./project/scripts/oracle_diff.sh --sql "SELECT ..." --mode ast
./project/scripts/oracle_diff.sh --sql "SELECT ..." --mode plan
./project/scripts/oracle_diff.sh --sql "SELECT ..." --mode exec

# 生成 optimizer trace（Phase 4+）
cargo run --bin tidb-server -- --execute "EXPLAIN ..." --trace-out /tmp/trace.json
```

---

## 3. 日志与 Tracing

### 3.1 约定

| Span | 字段 | 说明 |
|---|---|---|
| `session` | `conn_id`, `session_id` | 连接生命周期 |
| `stmt` | `stmt_id`, `sql_digest` | 单语句 |
| `parse` | `sql_len`, `stmt_count` | 解析 |
| `preprocess` | `schema_version` | 名字解析/绑定 |
| `build` | `plan_id` | 逻辑计划构建 |
| `rbo` | `rule`, `changed` | 每条规则 |
| `cbo` | `task_type`, `candidate_cnt` | 物理枚举 |
| `exec` | `operator`, `rows`, `elapsed` | 执行 |
| `cop` | `region_id`, `store_addr`, `ranges` | Cop 请求 |
| `txn` | `start_ts`, `commit_ts`, `keys` | 事务 |

### 3.2 环境变量

```bash
RUST_LOG=info
RUST_LOG=tidb::planner=debug,tidb::executor=trace
RUST_LOG=error,tidb::store::copr=debug
```

### 3.3 日志反模式

- 在热路径 `format!` 大 SQL；用 `tracing::enabled!` 或延迟字段。
- 打印密码/密钥；使用 `SecureText`。
- 只打印“失败”，不打印输入/上下文。

---

## 4. Parser 调试

### 4.1 检查清单

- [ ] 原文是否被 `preprocess_text` 改写？打印改写后的文本。
- [ ] sqlparser 是否支持该语法？单独跑 `sqlparser` 解析。
- [ ] `TiDBDialect` 是否覆盖？检查 `dialect.rs`。
- [ ] Lowering 是否丢失字段？对比 AST JSON。
- [ ] `Restore` 是否可 round-trip？
- [ ] 错误位置/错误码是否与 TiDB 接近？

### 4.2 AST 对比

```bash
# TiDB
go run ./tools/oracle/cmd/tidb-oracle parse --sql "..." --pretty > /tmp/tidb.ast.json

# Rust（提供 debug 子命令）
cargo run --bin tidb-server -- --parse "..." --ast-json > /tmp/rust.ast.json

diff -u /tmp/tidb.ast.json /tmp/rust.ast.json
```

**定位技巧**：先比较顶层 `type`，再递归比较字段；不要一开始就 diff 整个 JSON。

### 4.3 常见问题

| 现象 | 可能原因 | 处理 |
|---|---|---|
| 语法错误但 TiDB 可解析 | sqlparser 不支持/TiDBDialect 缺 | 加 Dialect/预处理/子解析 |
| AST 字段为空 | lowering 漏字段 | 对照上游 AST 定义 |
| `Restore` 与原文差异 | 格式化策略/优先级 | 对照 `format` 包 |
| 多语句只解析第一条 | 分号处理 | 检查 `parse_sql` 循环 |
| Hint 丢失 | 未保留注释 token | 先保留原文，后结构化 |

---

## 5. Preprocess / PlanBuilder 调试

### 5.1 检查清单

- [ ] InfoSchema 版本是否正确？（打印 `schema_version`）
- [ ] 表/列名是否解析到正确 ID？
- [ ] 数据库/表别名作用域是否正确？
- [ ] `visitInfo` 是否收集？（权限失败时）
- [ ] 逻辑算子类型/子节点顺序是否符合预期？
- [ ] `optFlag` 是否打开了正确的规则位？

### 5.2 打印逻辑计划

```text
Limit
└─ Sort(t.id)
   └─ Projection(t.id,t.b,s.c)
      └─ Selection(t.b>10)
         └─ Join(t.a=s.a)
            ├─ DataSource(t)
            └─ DataSource(s)
```

与 TiDB 的对比方法：

1. 用 Oracle `plan --stage logical` 输出逻辑计划。
2. 用 `EXPLAIN` 只看物理计划；逻辑计划需要内部 debug 输出。
3. 比较：算子类型、子节点顺序、谓词位置、列引用。

### 5.3 常见问题

| 现象 | 可能原因 | 处理 |
|---|---|---|
| `unknown table` | Preprocess 未绑定/大小写 | 检查 InfoSchema/大小写规则 |
| 谓词没下推 | PPD 规则未启用/条件不满足 | 检查 `optFlag`/规则实现 |
| 列裁剪过度 | 未识别 correlated column | 对照 ColumnPruner 上游 |
| Join 顺序异常 | JoinReorder 统计/约束 | 固定 stats 复现 |
| 计划中有多余 Projection | ProjectionEliminator 未跑 | 检查规则顺序 |

---

## 6. CBO / FindBestTask 调试

### 6.1 观察点

- `RecursiveDeriveStats`：每层 `rowCount`/`NDV` 是否合理。
- `ExhaustPhysicalPlans`：候选物理算子是否齐全。
- `Skyline pruning`：是否错误剪掉更优路径。
- `Attach2Task`：Task 类型（Root/Cop/MPP）是否符合预期。
- `CompareTaskCost`：代价因子是否与 TiDB 一致。
- `taskMap`：命中/复用是否正确。

### 6.2 Optimizer Trace

Phase 4 起输出 JSON，字段尽量对齐 TiDB：

```json
{
  "steps": [
    {"name": "BuildLogicalPlan", "plan": "..."},
    {"name": "RBO", "rule": "PPDSolver", "changed": true, "plan": "..."},
    {"name": "CBO", "candidates": [{"task": "Root", "cost": 123.4}]}
  ]
}
```

### 6.3 常见问题

| 现象 | 可能原因 | 处理 |
|---|---|---|
| 选了全表扫而非索引 | stats 缺失/代价因子 | 对比 stats/成本 |
| 计划成本为 0/NaN | mock stats/溢出 | 检查 stats 初始化 |
| 无物理计划 | 属性不满足/算子未实现 | 看 `InvalidTask` 原因 |
| 候选数异常少 | ExhaustPhysicalPlans 缺分支 | 对照上游 switch |
| 计划不稳定 | map 遍历顺序/浮点 | 固定顺序/容差 |

---

## 7. Executor 调试

### 7.1 检查清单

- [ ] `Open` 是否按自顶向下顺序？
- [ ] 子算子是否都 `Close`？
- [ ] `Next` 前是否 `Reset`？
- [ ] 空 Chunk 是否表示 EOF？
- [ ] `sel` 是否正确传递？
- [ ] 表达式类型/Nullable 是否匹配？
- [ ] 并发 worker 是否泄漏/死锁？

### 7.2 单算子隔离

```rust
// 用 TableDual / Values 构造最小输入
let child = TableDualExec::new(...);
let mut exec = SelectionExec::new(child, filter);
let mut chunk = exec.new_chunk();
exec.open(&mut ctx)?;
while exec.next(&mut ctx, &mut chunk)? > 0 { ... }
```

### 7.3 常见问题

| 现象 | 可能原因 | 处理 |
|---|---|---|
| 结果多行/少行 | `sel`/NULL/循环终止 | 打印 `NumRows`/`sel` |
| 结果值错位 | 列顺序/schema 不一致 | 对比 Schema/FieldType |
| 类型错误 | Cast/隐式转换 | 对比 TiDB 类型推导 |
| 死锁/卡住 | 通道/锁/取消 | `tokio-console`/日志 |
| 内存暴涨 | Chunk 未复用/泄漏 | 检查 Pool/Close |
| panic | `unwrap`/越界 | 定位调用栈；补错误处理 |

---

## 8. DistSQL / Coprocessor 调试

### 8.1 观察点

- KeyRange 是否正确（table prefix、handle 编码）。
- Region 切分是否正确（start/end key、epoch）。
- DAG 请求是否包含预期算子。
- Cop 响应编码（row vs chunk）是否匹配。
- 重试/backoff 是否合理（Region epoch、leader 迁移、ServerIsBusy）。

### 8.2 工具

```bash
# 查看 Region
pd-ctl -u http://127.0.0.1:2379 region key <hex-key>

# 查看 store
pd-ctl -u http://127.0.0.1:2379 store

# TiKV 日志
docker logs -f <tikv-container>

# gRPC 调试（若启用 reflection）
grpcurl -plaintext 127.0.0.1:20160 list
```

### 8.3 常见问题

| 现象 | 可能原因 | 处理 |
|---|---|---|
| `region epoch not match` | Region 分裂/迁移 | 刷新 RegionCache 重试 |
| `key not in region` | Key 编码错误 | 对比 `tablecodec` 字节 |
| Cop 返回空 | DAG/range 错误 | 打印请求与 TiKV 日志 |
| 解码失败 | tipb 编码/字段不匹配 | 对照 tipb 定义 |
| 超时 | store 忙/网络 | backoff/超时配置 |

---

## 9. 事务调试

### 9.1 观察点

- `startTS` / `forUpdateTS` / `commitTS` 是否符合预期。
- 读路径：Snapshot vs MemBuffer 的优先级。
- 写路径：MemBuffer → StmtCommit → Commit。
- 悲观锁：`LockKeys` 的 key 集合、锁等待。
- 冲突：write conflict / lock timeout / `ErrResultUndetermined`。
- SchemaChecker：提交前是否检测到 DDL。

### 9.2 常见问题

| 现象 | 可能原因 | 处理 |
|---|---|---|
| 读不到自己的写 | UnionScan/MemBuffer 未合并 | 检查 `GetMemBuffer`/`UnionIter` |
| 提交后 TiDB 读不到 | commitTS/2PC 未完成 | 检查 Prewrite/Commit 响应 |
| 锁等待超时 | 锁未释放/顺序死锁 | 查看 TiKV 锁信息 |
| 乐观冲突重试失败 | 重试逻辑/时间戳 | 对照 TiDB `OnStmtErrorForNextAction` |
| 提交结果不明被重放 | `ErrResultUndetermined` 处理错误 | 禁止自动重试，交给上层 |

---

## 10. MySQL 协议调试（Phase 9）

| 工具 | 用途 |
|---|---|
| `mysql --debug` | 客户端侧日志 |
| `tcpdump -i lo -w /tmp/mysql.pcap 'tcp port 4000'` | 抓包 |
| Wireshark（MySQL dissector） | 看包字段 |
| `go-mysql` / `mysql-connector` | 客户端兼容性 |

检查顺序：握手能力位 → 认证插件 → 序列号 → 结果集列定义 → 行编码 → OK/ERR/EOF。

---

## 11. 故障手册（按错误类型）

| 错误/现象 | 首先检查 | 常见根因 |
|---|---|---|
| `You have an error in your SQL syntax` | Parser/AST | 语法不支持/预处理错误 |
| `Table doesn't exist` | InfoSchema/Preprocess | schema 版本/大小写/库名 |
| `Unknown column` | 名字解析 | 别名作用域/列裁剪 |
| `Can't find a proper physical plan` | CBO | 属性/算子/统计 |
| `region epoch not match` | RegionCache | 分裂/迁移 |
| `write conflict` | 事务 | 并发写/重试 |
| `lock wait timeout` | 悲观锁 | 锁顺序/长事务 |
| `context canceled` | 取消 | Close/超时未传播 |
| `index out of range` | Executor/Chunk | `sel`/列数不一致 |
| OOM | Chunk/排序/聚合 | 无 spill/内存池泄漏 |

---

## 12. 调试记录模板

每次非平凡排障，在 `project/notes/debug-YYYYMMDD-<slug>.md` 记录：

```markdown
# 问题
- SQL/场景：
- 期望：
- 实际：

# 定位
- 层：
- 证据（日志/计划/diff）：

# 根因
- ...

# 修复
- 文件/提交：
- 测试：

# 遗留
- ...
```

> 调试记录是后续 ADR/回归测试的输入；不要只留在聊天记录里。
