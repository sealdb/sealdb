# 路线图与阶段 Gate（Roadmap）

> 原则：**每个阶段交付一条可运行的 SQL 竖切链路**；Gate 未通过不得进入下一阶段。  
> 工期为“1 名熟悉 TiDB 的资深工程师”的相对估算；多人可并行压缩。  
> 进度看板见 [progress.md](./progress.md)。

---

## 0. 阶段总览

```mermaid
flowchart LR
  P0[Phase 0<br/>地基] --> P1[Phase 1<br/>类型与编码]
  P1 --> P2[Phase 2<br/>解析器 + Mock 链路]
  P2 --> P3[Phase 3<br/>元数据/Bootstrap]
  P3 --> P4[Phase 4<br/>Planner RBO]
  P4 --> P5[Phase 5<br/>Executor v1]
  P5 --> P6[Phase 6<br/>事务/Session]
  P6 --> P7[Phase 7<br/>CBO/统计]
  P7 --> P8[Phase 8<br/>DDL/高级 SQL]
  P8 --> P9[Phase 9<br/>MySQL 协议/运维]
  P9 --> P10[Phase 10<br/>可选：TiFlash/MPP 等]
```

| Phase | 名称 | 预估 | 核心交付 | Gate |
|---|---|---|---|---|
| 0 | 地基与规范 | 1–2 周 | 工程手册、Oracle、parity 脚本、工具链、TiKV spike | `cargo check` + oracle smoke + parity 报告 |
| 1 | 核心类型与编码 | 3–4 周 | `types` / `codec` / `tablecodec` / `kv` 基础 | 编码字节级一致 |
| 2 | 解析器 + Mock 链路 | 4–6 周 | `parser` + AST + sqlparser bridge + Mock Planner/Executor | `SELECT 1` 端到端；Parser 通过率 ≥60% |
| 3 | 元数据与 Bootstrap | 3–4 周 | `meta` / `infoschema` / `domain` / 读取 TiDB Schema | `SHOW/DESC` 与 TiDB 一致 |
| 4 | Planner v1（RBO） | 5–7 周 | Preprocess、PlanBuilder、逻辑算子、RBO 子集、固定统计 CBO | `EXPLAIN` 计划树对比 |
| 5 | Executor v1 | 6–8 周 | Chunk、表达式核心、Reader/Join/Agg/Sort/Limit/DML | 结果集与 TiDB 一致 |
| 6 | 事务与 Session | 4–6 周 | sessionctx/sessiontxn、TSO、2PC、悲观/乐观、SchemaChecker | 并发/隔离测试 |
| 7 | CBO 与统计信息 | 5–7 周 | statistics、cardinality、cost model、FindBestTask、Skyline | 计划选择与 TiDB 对齐 |
| 8 | DDL 与高级 SQL | 8–12 周 | 在线 DDL、分区、CTE/Window、SetOp、IndexMerge、权限 | DDL/复杂查询回归 |
| 9 | MySQL 协议与运维 | 3–4 周 | 握手/认证/COM_QUERY/结果集/TLS/状态页 | `mysql` 客户端可连接 |
| 10 | 可选高级能力 | 持续 | TiFlash/MPP、Plan Cache、Binding、资源组、插件 | 按需 |

---

## Phase 0 — 地基与规范

### 目标

让“移植”成为可重复、可验证的工程流程，而不是一次性手工劳动。

### 交付物

- [x] `project/` 工程手册（本文档集）
- [x] 根 `Cargo.toml`（workspace，顶层包粒度）、`rust-toolchain.toml`（1.99.1）
- [x] `cargo metadata` / `cargo check --workspace` 可在空骨架通过
- [x] `tools/oracle/` 的 `parse` 模式可运行（`plan`/`execute` 待补）
- [x] `project/scripts/parity_check.py`
- [x] `project/scripts/install-skills.sh`
- [x] 旧 `src/`/`tests/`/`docs/` 归档到 `legacy-rust-scaffold/`
- [ ] CI（GitHub Actions 或本地脚本）：fmt + clippy + test + oracle smoke
- [ ] TiKV/PD 连接 spike：PD TSO、RegionCache、乐观/悲观事务、RawClient Coprocessor
- [ ] `project/parity/status.json` 初始登记与 `--strict` 通过

### Gate 0

```bash
cargo metadata --no-deps >/dev/null
cargo check --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
python3 project/scripts/parity_check.py --tidb-root <tidb> --rust-root . --strict
cd tools/oracle && go run ./cmd/tidb-oracle -sql "select 1" -pretty
```

**通过标准**：全部命令成功；Oracle 能对 `select 1` 输出 AST JSON；parity 报告可生成；TiKV spike 有书面结论。

### 风险

- 受限网络下安装 Rust 1.99.1 → 使用 `rsproxy.cn` 镜像。
- `tikv-client` 能力不足 → 见 ADR-0004 的兜底方案。
- PD/TiKV 版本不统一 → 统一使用 8.5.8 测试镜像/tag。

---

## Phase 1 — 核心类型与编码

### 范围

```text
pkg/parser/model        Schema 模型（DBInfo/TableInfo/ColumnInfo/IndexInfo）
pkg/parser/mysql        MySQL 常量、类型、Flag、错误码
pkg/parser/types        FieldType 等 parser 类型
pkg/types               Datum、FieldType、MyDecimal、Time、JSON、Enum/Set
pkg/util/codec          memcomparable 编码
pkg/tablecodec          行/索引 Key 编码
pkg/kv                   Key/KeyRange/Handle/Version/IsolationLevel（接口先不实现 Storage）
pkg/errno               MySQL errno / SQLSTATE
pkg/util/chunk          Column/Chunk/sel（先核心，不接执行器）
```

### 竖切

`cargo test -p tidb types::...`：类型比较、编码/解码 round-trip。

### Gate 1

- 整数/无符号/浮点/字节/Decimal/Time 的 `codec.Encode*` 字节级一致。
- `tablecodec.EncodeRowKey/EncodeIndexKey` 与 TiDB 输出一致。
- `Datum` 比较、NULL、溢出、隐式转换的核心用例通过。
- Oracle 用例 ≥ 200 条（从 TiDB `codec_test.go`/`tablecodec_test.go`/`types` 提取）。
- 无 `unwrap` 进入库路径。

---

## Phase 2 — 解析器 + AST + Mock 链路

### 目标

先让“SQL 文本 → AST → 固定计划 → 固定结果”跑通，验证目录、接口、错误模型。

### 范围

```text
pkg/parser/ast          Node/ExprNode/StmtNode、Visitor、Flag、Restore
pkg/parser/format       RestoreCtx
pkg/parser/parser       ParseSQL/ParseOneStmt（sqlparser bridge）
pkg/parser/lexer        兼容层（可薄封装 sqlparser Tokenizer）
pkg/parser/digester     SQL 归一化/指纹
pkg/parser/hintparser   Hint 解析（先保留文本）
pkg/parser/mysql        关键字/常量（按需）
pkg/parser/sqlparser_bridge  TiDBDialect + lowering + 文本预处理
pkg/session             最小 Session：Parse → ExecuteStmt
pkg/planner             Mock：AST → 固定 LogicalPlan
pkg/executor            Mock：固定结果（SELECT 1 / 常量表）
cmd/tidb-server         最小 CLI：--execute / REPL
```

### 竖切

```bash
cargo run --bin tidb-server -- --execute "SELECT 1"
# 输出：1
#       1
cargo run --bin tidb-server -- --execute "SELECT * FROM t LIMIT 1"
# 输出固定 mock 行（来自 Mock Planner/Executor）
```

### Parser 首批语法（按优先级）

1. `SELECT`：投影、别名、`FROM`、`WHERE`、`GROUP BY`、`HAVING`、`ORDER BY`、`LIMIT`、`DISTINCT`。
2. `JOIN`：INNER/LEFT/RIGHT/CROSS、`ON`/`USING`。
3. 表达式：算术、比较、逻辑、`IN`、`BETWEEN`、`LIKE`、`IS NULL`、`CASE`、函数调用、`CAST`。
4. 子查询、`EXISTS`、`UNION`。
5. `INSERT` / `UPDATE` / `DELETE`。
6. `CREATE/DROP/ALTER TABLE`（基础）。
7. `USE` / `SET` / `BEGIN` / `COMMIT` / `ROLLBACK` / `SHOW`（基础）。

### Gate 2

- Parser Oracle 通过率 ≥ **60%**（语料：TiDB `pkg/parser/parser_test.go` + `ast/*_test.go` 提取的选择性子集，Phase 2 冻结分母）。
- 所有已支持语句 `Restore` 后再次解析，结构一致（round-trip）。
- 错误输入返回错误而非 panic；AST depth 限制生效。
- `SELECT 1` / `SELECT * FROM t LIMIT n` 端到端可运行。
- 未支持语句有明确错误，并在 `project/parity/status.json` 标 `stub/omitted`。

---

## Phase 3 — 元数据与 Bootstrap

### 范围

```text
pkg/meta/model           DBInfo/TableInfo/ColumnInfo/IndexInfo/SchemaDiff
pkg/meta                 Reader/Mutator、m 前缀 key 布局
pkg/infoschema           InfoSchema/InfoCache/Builder（先全量）
pkg/domain               Domain、Schema 加载循环（先全量 Reload）
pkg/session/bootstrap    空集群初始化 mysql 系统库（按需）
pkg/session             SHOW DATABASES/TABLES、DESC、USE
```

### 竖切

连接一个由 TiDB 初始化过的 TiKV 集群，Rust 节点读取 schema：

```sql
SHOW DATABASES;
SHOW TABLES;
DESC t;
SELECT * FROM information_schema.tables LIMIT 5;
```

### Gate 3

- 对同一快照，Rust 的 `SHOW DATABASES/TABLES`、`DESC` 输出与 TiDB 一致（列顺序/类型/默认值）。
- 能读取 TiDB 创建的表，并正确解析 `TableInfo`（列、索引、主键、分区基础字段）。
- Domain 能感知 schema 版本变化并 Reload。
- 空集群 Bootstrap 的行为与 TiDB 对齐（至少 `mysql` 库与 `tidb` 基础表）。
- 已知不支持（视图/分区/临时表）有明确错误。

---

## Phase 4 — Planner v1（RBO + 固定统计 CBO）

### 范围

```text
pkg/planner/core/base               Plan/LogicalPlan/PhysicalPlan/LogicalOptRule/Task
pkg/planner/core/operator/logicalop DataSource/Join/Agg/Selection/Projection/Sort/TopN/Limit/UnionAll/Lock/UnionScan
pkg/planner/core/operator/physicalop 先实现计划节点与属性，不急于全部执行
pkg/planner/core/preprocess          名字解析、InfoSchema 绑定、visitInfo（权限先占位）
pkg/planner/core/planbuilder         PlanBuilder.Build、buildSelect
pkg/planner/core/expression_rewriter AST Expr → expression.Expression
pkg/planner/core/optimizer          DoOptimize / logicalOptimize / optRuleList
pkg/planner/core/rule               首批 RBO：ColumnPrune、PPD、ConstantPropagation、Outer2Inner、BuildKeyInfo、TopN pushdown、JoinReorder(基础 DP)
pkg/planner/core/find_best_task     固定 stats + 简单代价
pkg/planner/core/explain            EXPLAIN 输出
pkg/planner/property                PhysicalProperty/StatsInfo/TaskType
pkg/planner/optimize                入口（无 Plan Cache/Binding）
pkg/expression                       Column/Constant/ScalarFunction 基础（供计划与 EXPLAIN）
```

### 统计信息策略（Phase 4）

- `StatsInfo` 行数/NDV 用**固定值或简单规则**（如 `10000` 行、选择率 0.1）。
- 代价模型先用 Ver1 的简化版；Phase 7 替换。
- 所有 mock 必须在 `PARITY_NOTES` 标注，且接口与真实实现一致。

### 竖切

```sql
EXPLAIN SELECT t.id, t.b, s.c
FROM t JOIN s ON t.a = s.a
WHERE t.b > 10
ORDER BY t.id
LIMIT 20;
```

输出计划树（Limit → Sort → Projection → HashJoin → TableReader...）。

### Gate 4

- 单表 `SELECT` 的逻辑计划与 TiDB 一致（固定 stats）。
- 两表等值 Join 的 RBO 后计划形状一致率 ≥ **70%**（选定语料）。
- RBO 规则顺序、flag 位、interaction 规则行为与上游一致。
- `EXPLAIN` 可稳定 diff；`EXPLAIN FORMAT=verbose` 至少输出核心字段。
- 无物理计划时返回 TiDB 风格错误（`Can't find a proper physical plan...`）。
- 未实现规则/算子有明确 `stub` 标记。

---

## Phase 5 — Executor v1

### 范围

```text
pkg/util/chunk                     Chunk/Column/sel/Pool/Codec
pkg/expression                     核心标量函数、Cast、比较、逻辑、字符串基础
pkg/executor/internal/exec         Executor trait/BaseExecutor
pkg/executor/builder               Plan → Executor 的 match（按算子分批）
pkg/executor/table_reader          TableReader（KV/Cop）
pkg/executor/point_get             PointGet/BatchPointGet
pkg/executor/select                Selection/Limit/MaxOneRow/TableDual
pkg/executor/projection            Projection
pkg/executor/sortexec              Sort/TopN
pkg/executor/aggregate             HashAgg/StreamAgg
pkg/executor/aggfuncs              COUNT/SUM/AVG/MIN/MAX（先核心）
pkg/executor/join                  HashJoin/MergeJoin（先 HashJoin）
pkg/executor/unionexec             UnionAll
pkg/executor/insert/update/delete  DML 基础
pkg/distsql                        RequestBuilder/SelectResult
pkg/store/copr                     Region 切分/重试/发送
pkg/executor/explain               EXPLAIN/EXPLAIN ANALYZE（基础）
```

### 竖切

```sql
SELECT id, name FROM users WHERE id = 1;          -- PointGet
SELECT * FROM users WHERE age > 18 ORDER BY id LIMIT 10; -- TableReader + Selection + Sort + Limit
SELECT COUNT(*) FROM users;                        -- Agg
SELECT t.id, s.c FROM t JOIN s ON t.a = s.a;       -- HashJoin
INSERT INTO users(id,name) VALUES (1,'a');         -- DML
```

### Gate 5

- 单表 SELECT（含 WHERE/ORDER/LIMIT）结果与 TiDB 一致（类型化比较，不看字符串格式）。
- 两表 HashJoin 结果一致（含 NULL/重复键）。
- COUNT/SUM/AVG/MIN/MAX 结果一致（含 NULL、空表）。
- PointGet/BatchPointGet 与 TableReader 均可用；Cop 下推 Selection/Projection/Limit。
- DML 可写 TiKV，TiDB 能读到（同一集群/同一快照策略）。
- 算子 `Open/Next/Close` 可重复/可取消；无 panic。
- 结果集类型、列名、NULL 表示与 Oracle 对齐。

---

## Phase 6 — 事务与 Session

### 范围

```text
pkg/kv                          Transaction/Snapshot/Storage/Retriever 完整接口
pkg/store/driver/txn            tikv-client 事务封装、MemBuffer、UnionIter、锁
pkg/sessiontxn                  TxnManager/TxnContextProvider、isolation/*
pkg/session/txn                 LazyTxn、StmtCommit/Rollback
pkg/session                     PrepareTxnCtx、ExecuteStmt、错误重试
pkg/domain/schema_checker       SchemaChecker
pkg/executor/union_scan         UnionScan
pkg/executor/insert/update/delete 悲观锁/事务写路径
```

### 竖切

```sql
BEGIN PESSIMISTIC;
UPDATE t SET b = b + 1 WHERE id = 1;
COMMIT;

BEGIN;
SELECT * FROM t WHERE id = 1; -- 读己之写
ROLLBACK;
```

### Gate 6

- 乐观/悲观 RR 的基本读写、提交、回滚一致。
- `SELECT ... FOR UPDATE` 加锁；并发冲突行为与 TiDB 对齐（锁等待/冲突错误）。
- RR 下快照读一致；RC 下语句级读（若实现）。
- 读己之写（MemBuffer ∪ Snapshot）。
- 提交前 SchemaChecker 能拒绝不兼容 DDL。
- 冲突/重试路径有测试；`ErrResultUndetermined` 不被误判为可重试。
- 与 TiDB 交叉验证：Rust 写 → TiDB 读；TiDB 写 → Rust 读。

---

## Phase 7 — CBO 与统计信息

### 范围

```text
pkg/statistics                  Histogram/CMSketch/FMSketch/TopN/Table/Column/Index
pkg/statistics/handle           统计信息加载/缓存（先内存/系统表）
pkg/planner/cardinality         基数/选择率估计
pkg/planner/core/plan_cost_ver1 plan_cost_ver2 cost/
pkg/planner/core/find_best_task FindBestTask/ExhaustPhysicalPlans/Attach2Task
pkg/planner/core/index_join_path/indexmerge_path
pkg/planner/core/rule_join_reorder_dp/greedy
pkg/executor/analyze            ANALYZE（基础）
```

### Gate 7

- 用与 TiDB 相同的统计信息快照，`EXPLAIN` 选择相同访问路径/Join 类型的比例 ≥ **85%**（选定语料）。
- 直方图/CMSketch 的估计值与 TiDB 在容差内。
- Skyline pruning 行为一致（支配关系）。
- Cost Ver1/Ver2 的因子可配置；默认与 TiDB 一致。
- `ANALYZE` 能写回统计信息并被 CBO 使用。
- 计划回归套件（plan suite）可重复通过。

---

## Phase 8 — DDL 与高级 SQL

### 范围（按优先级分批）

1. DDL 基础：`CREATE/ALTER/DROP TABLE`、`CREATE/DROP INDEX`、`TRUNCATE`、`RENAME`。
2. 在线 DDL：Job 队列、Owner、状态机、SchemaDiff、SchemaSyncer、MDL（后置）。
3. 高级 SQL：CTE、Window、SetOp（UNION/EXCEPT/INTERSECT）、IndexMerge、分区裁剪、视图、Sequence、Placement、Resource Group。
4. 权限：`privilege`、`GRANT/REVOKE`、动态权限、SEM（后置）。
5. Binding / Plan Cache。
6. `LOAD DATA` / `IMPORT INTO`（后置）。

### Gate 8

- 基础 DDL 在 TiDB/Rust 间互操作；Schema 版本正确推进。
- 在线加索引/加列的中间状态对 DML/查询可见性符合 TiDB 文档。
- CTE/Window/SetOp 结果与 TiDB 一致。
- 分区裁剪计划与结果一致。
- 权限检查在优化前失败（fail-fast），错误码一致。
- DDL 故障注入/重试测试通过。

---

## Phase 9 — MySQL 协议与运维

### 范围

```text
pkg/server                   Server/Accept/连接生命周期
pkg/server/internal/packetio MySQL packet 读写、压缩
pkg/server/internal/handshake 握手/认证/TLS
pkg/server/internal/column    列定义/行编码
pkg/server/internal/resultset 结果集
pkg/server/conn               COM_QUERY/PING/INIT_DB/QUIT
pkg/server/http_*             status/metrics（基础）
pkg/config                   配置加载
cmd/tidb-server              flags/config/启动/优雅退出
```

### Gate 9

- `mysql -h 127.0.0.1 -P 4000` 可连接并执行 `SELECT 1`。
- 文本结果集、OK/ERR/EOF、多语句、`USE`、`SET` 基础可用。
- 连接数限制、超时、优雅退出可用。
- Prometheus `/metrics` 可抓取核心指标。
- 与 TiDB 的协议抓包对比（关键包字段一致）。

---

## Phase 10 — 可选高级能力

- TiFlash / MPP / BatchCop。
- 向量化表达式全覆盖 / SIMD 优化。
- 资源组 / Runaway / 全局内存控制。
- Extension/Plugin / 审计。
- BR/Dumpling/Lightning 兼容（只读/写入协议）。
- Cascades 优化器。

> 这些能力不阻塞主链路；每项单独 ADR + Gate。

---

## 阶段依赖与并行建议

| 可并行 | 说明 |
|---|---|
| Phase 1 类型/编码 ∥ Phase 2 AST 骨架 | 先定义类型接口，编码与 AST 可并行 |
| Phase 3 元数据 ∥ Phase 4 Planner 接口 | Planner 可用 Mock InfoSchema 先行 |
| Phase 5 Executor ∥ Phase 6 事务 | Executor 先用只读快照，事务层并行 |
| Phase 7 CBO ∥ Phase 8 DDL 基础 | 统计信息与 DDL 元数据相对独立 |
| Phase 9 协议 ∥ Phase 8 高级 SQL | 协议层可先用 CLI 验证 |

## 每个阶段的固定动作

1. 开工前：读本阶段文档 + 上游包 + 测试；更新 `progress.md` 为 `in_progress`。
2. 开发中：每个文件遵守 `conventions.md`；每周跑一次 Oracle 差分。
3. 收工前：跑 Gate 命令；把报告放 `project/reports/phase-XX/`；更新 `progress.md` / `parity-matrix.md`。
4. 评审：按 `conventions.md` §12 检查；未通过不开下一阶段。
