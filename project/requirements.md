# 需求与约束（Requirements）

> 状态：**v0.1 初稿**（Phase 0 基线）  
> 上游基线：TiDB **v8.5.8**（commit `f396bb4`）  
> 本文中的“必须/禁止”是硬约束；偏离必须新增 ADR。

---

## 1. 项目目标

### 1.1 总目标

用 Rust 实现一个与 **TiDB 计算层**行为等价的 SQL 计算节点：

```text
MySQL Client
   │  MySQL Protocol
   ▼
[Rust Compute Node]  ←→  PD（TSO / Region / 调度）
   │  KV / Coprocessor / 2PC
   ▼
TiKV（保持原样，不修改）
```

### 1.2 分目标

| ID | 目标 | 验收方式 |
|---|---|---|
| G1 | SQL 文本可被解析为与 TiDB 语义一致的 AST | Parser Oracle 差分 |
| G2 | AST 可被预处理、构建逻辑计划、RBO、CBO | Plan Oracle 差分（固定统计信息） |
| G3 | 物理计划可被执行为正确结果 | Result Oracle 差分 |
| G4 | 能读写 TiDB 创建的表与元数据 | 同一 TiKV 集群交叉验证 |
| G5 | 事务语义与 TiDB 对齐（乐观/悲观、RR/RC） | 并发/隔离级别测试 |
| G6 | 目录名、文件名与 TiDB 对齐 | `parity_check.py` |
| G7 | 每个阶段都有可运行的竖切链路 | Phase Gate |

### 1.3 非目标（初版明确不做）

- 不修改 TiKV、PD 的任何代码；不替换 Raft/Region/2PC 存储实现。
- 不实现 TiKV LSM、Raft、Region 调度内部逻辑。
- 首版不追求 TiFlash/MPP、向量化 SIMD 内核、BR/Dumpling/Lightning、Plugin/Telemetry 的完整能力。
- 不追求 MySQL/TiDB 100% 边缘语法与错误消息逐字一致；以「兼容矩阵 + Oracle 差异清单」逐步收敛。
- 不在首版追求极致性能；先正确、可观测、可回归，再优化。

---

## 2. 范围（Scope）

### 2.1 纳入范围（Compute Layer）

以 TiDB `pkg/` 下与 SQL 计算相关的包为主：

```text
cmd/tidb-server
pkg/server            MySQL 协议、连接、命令分发
pkg/session           Session、ExecuteStmt、Txn 封装
pkg/sessionctx        会话上下文、变量、StmtCtx
pkg/sessiontxn        隔离级别、TS、Schema 绑定
pkg/parser            Lexer/语法入口、AST、Preprocess 之前的语法层
pkg/planner           优化入口、Plan Cache、Binding
pkg/planner/core      PlanBuilder、RBO、CBO、物理算子
pkg/planner/property  逻辑/物理属性、统计信息
pkg/planner/cascades  可选 Cascades（后置）
pkg/executor          Compiler、Builder、各执行器
pkg/executor/internal Executor 契约、Builder、MPP 等内部包
pkg/expression        标量/向量化表达式
pkg/distsql           Cop 请求构建与结果解码
pkg/store/copr        Region 切分、Cop 调度
pkg/store/driver      TiKV 客户端驱动封装
pkg/kv                Transaction/Snapshot/Storage 抽象
pkg/meta              元数据读写
pkg/meta/model        Schema 模型
pkg/infoschema        内存 Schema 缓存
pkg/domain            Domain、Schema 同步、DDL/Stats 生命周期
pkg/ddl               分布式 DDL
pkg/statistics        直方图/CMSketch/基数估计
pkg/tablecodec        行/索引 Key 编码
pkg/table             表抽象
pkg/types             类型系统、Datum、Decimal、Time
pkg/util/chunk        列存 Chunk
pkg/util/codec        memcomparable 编码
pkg/privilege         权限管理
pkg/bindinfo          SQL Binding
pkg/config            配置
pkg/errno             MySQL 错误码
pkg/parser/model      与 parser 共享的 model
pkg/parser/mysql      MySQL 常量/类型/Flag
pkg/parser/types      parser 用的类型
```

### 2.2 暂不纳入 / 后置

| 包/目录 | 处理方式 |
|---|---|
| `br/`, `dumpling/`, `lightning/`, `tiflash/`, `tispark/` | 目录占位 + README 说明，不移植 |
| `pkg/disttask/`, `pkg/resourcemanager/`, `pkg/owner/` | 后置（DDL 阶段按需） |
| `pkg/plugin/`, `pkg/extension/`, `pkg/telemetry/` | 后置 |
| `pkg/util/` 中与计算无关的工具 | 按需移植；未用文件 `omitted` |
| TiFlash / MPP / 向量索引 | Phase 10 可选 |

> 目录占位仍需遵守“目录名与 TiDB 一致”；占位目录内放 `README.md` 说明原因。

---

## 3. 硬性约束（Hard Constraints）

| ID | 约束 | 说明 |
|---|---|---|
| C1 | **目录名、文件名与 TiDB 一致** | `.go` → `.rs`；见 `architecture.md` §目录对齐。构建文件例外。 |
| C2 | **核心逻辑/算法一致** | 禁止“重新发明”优化规则、代价模型、编码格式、事务状态机。 |
| C3 | **解析器使用 sqlparser-rs** | 不移植 goyacc；通过 `TiDBDialect + AST lowering` 对齐 TiDB AST。 |
| C4 | **PD/TiKV 能力保持不变** | 只作为客户端使用；不 fork、不修改其源码。 |
| C5 | **阶段化交付** | 每阶段可运行；优化器早期允许 mock/固定返回。 |
| C6 | **可追溯** | 每个文件标注上游路径与版本；每个阶段有 Gate 证据。 |
| C7 | **禁止静默 stub** | 未实现必须显式标注并进入 `progress.md`。 |
| C8 | **安全 Rust** | 默认 `#![forbid(unsafe_code)]`；确需 `unsafe` 必须 ADR + 最小封装 + 测试。 |

---

## 4. 兼容性契约

### 4.1 Parser 契约

- 输入：SQL 文本 + SQL Mode + Charset/Collation。
- 输出：`Vec<ast::StmtNode>` / `ast::StmtNode` + warnings + error。
- 必须支持：
  - 多语句拆分（受 `CLIENT_MULTI_STATEMENTS` 约束）。
  - MySQL 注释：`--`、`#`、`/* */`、`/*! ... */`。
  - 优化器 Hint：`/*+ ... */`（至少解析并保留）。
  - 标识符：反引号、大小写规则、Unicode。
  - 字符串：转义、字符集 introducer（如 `_utf8mb4'...'`）。
  - 变量：`@user_var`、`@@system_var`。
- AST 必须提供：
  - `Accept(Visitor)`、`Restore`、`Text/OriginalText`、`SetFlag/GetFlag`。
  - `StmtNode` / `ExprNode` / `ResultSetNode` / `DDLNode` / `DMLNode` 分层。
- 语义分析（名字解析、权限）**不在 parser**；交给 Preprocess/PlanBuilder。

### 4.2 Planner 契约

- 与 TiDB 相同的阶段划分：
  `Preprocess → PlanBuilder(Build) → logicalOptimize(RBO) → physicalOptimize(CBO) → postOptimize`。
- RBO 规则顺序必须与 `optRuleList` 一致；规则位图语义一致。
- 物理计划必须保留 Task 概念：`Root` / `Cop*` / `Mpp*`。
- Phase 4 允许 **固定统计信息/固定行数**；Phase 7 接入真实 statistics。
- `EXPLAIN` 输出必须可读、可 diff；优先对齐 TiDB 的计划树结构。

### 4.3 Executor 契约

- 控制流：`Open → Next → Close`（Volcano）。
- 数据单元：`Chunk`（列存批次），`Next` 填充调用方传入的 Chunk。
- `Next` 填充前必须 `Reset`；`Close` 可重复调用。
- 表达式：行式 `Eval` + 列式 `VecEval`（先做核心函数，逐步补齐）。
- 下推边界：可下推的 Selection/Projection/Limit/Agg/TopN 进入 DAG；其余本地执行。

### 4.4 事务契约

- 支持乐观事务与悲观事务（先 RR，后 RC；Serializable 后置）。
- 时间戳：`startTS`、`forUpdateTS`、`commitTS`，由 PD TSO 提供。
- 2PC：Prewrite + Commit（可复用 tikv-client）；支持冲突重试与 `ErrResultUndetermined` 语义。
- `MemBuffer ∪ Snapshot` 的 UnionScan 可见性必须正确。
- 提交前 SchemaChecker 必须校验 DDL 变更。

### 4.5 元数据契约

- 读取 TiDB 已写入 TiKV 的 `m` 前缀元数据（SchemaVersion/DB/Table/Diff 等）。
- `InfoSchema` 对外接口与 TiDB 对齐；支持按版本/按快照 TS 读取。
- Domain 的 Schema Lease/Reload 行为对齐；早期可只做全量 Reload。

### 4.6 协议契约（Phase 9）

- MySQL Protocol 4.1：握手、认证、`COM_QUERY`、`COM_PING`、`COM_INIT_DB`、`COM_QUIT`。
- 文本结果集；预处理语句（Prepare/Execute/Fetch/Close）后置。
- OK/ERR/EOF 包格式与 TiDB 对齐；错误码优先复用 `pkg/errno`。
- 支持 TLS、连接属性、`max_allowed_packet` 等基础能力。

---

## 5. 质量门槛（Definition of Done）

### 5.1 单文件 DoD

一个 `.rs` 文件算“完成”，必须同时满足：

- [ ] 有 `PORT_SOURCE` / `PORT_STATUS` 头。
- [ ] 核心逻辑与上游一致；偏差有注释/ADR。
- [ ] 有单元测试或 Oracle 用例覆盖。
- [ ] `cargo fmt` / `cargo clippy -D warnings` 通过。
- [ ] `parity-matrix.md` / `progress.md` 已更新。
- [ ] 无 `unwrap()/expect()` 进入生产路径（测试除外）。
- [ ] 无静默 `todo!()`；未实现必须 `PORT_STATUS: stub` 并列入进度。

### 5.2 阶段 DoD

- [ ] 该阶段 Gate 用例全部通过（见 `roadmap.md`）。
- [ ] Oracle 差分报告归档到 `project/reports/phase-XX/`。
- [ ] 已知差异登记到 `parity-matrix.md`。
- [ ] `progress.md` 状态、日期、证据链接齐全。
- [ ] 下一阶段的依赖项已明确。

### 5.3 质量指标（渐进）

| 指标 | Phase 2 | Phase 5 | Phase 7 | 最终目标 |
|---|---|---|---|---|
| Parser 用例通过率 | ≥ 60% | ≥ 80% | ≥ 90% | ≥ 95% |
| 单表 SELECT 结果一致 | 基础类型 | 核心算子 | 复杂查询 | 全量回归 |
| Plan 形状一致率（固定 stats） | — | ≥ 70% | ≥ 85% | ≥ 95% |
| 未标注 stub 数 | 0 | 0 | 0 | 0 |
| Clippy warnings | 0 | 0 | 0 | 0 |

> 百分比以 Oracle 语料为分母；具体分母在 `testing.md` 中按阶段冻结。

---

## 6. 交付方式

- **竖切优先**：每个阶段都交付“SQL 进 → 结果出”的可运行路径。
- **Mock 允许但有边界**：优化器/统计信息可先固定返回；但接口、数据结构、调用顺序必须与 TiDB 一致，避免后期返工。
- **先读后写**：移植一个文件前，先读上游实现、调用方、测试；禁止只看函数名猜实现。
- **小步提交**：一个提交只做一件事；提交说明引用上游路径/阶段。
- **不自动提交**：除非用户明确要求，否则不执行 `git commit`。

---

## 7. 变更管理

| 变更类型 | 流程 |
|---|---|
| 目录/文件对齐规则变更 | 新增 ADR + 更新 `architecture.md` |
| 解析器策略变更 | ADR + Parser Oracle 回归 |
| TiKV 客户端/协议变更 | ADR + Phase 0 spike 结论 |
| 阶段范围/顺序变更 | 更新 `roadmap.md` + `progress.md` |
| 与 TiDB 的已知偏差 | 登记 `parity-matrix.md`「已知偏差」 |
| 引入新依赖 | 说明用途、版本、License、替代方案 |

---

## 8. 开放问题（需确认）

| ID | 问题 | 建议默认值 | 影响 |
|---|---|---|---|
| Q1 | 现有 `src/` 直接删除还是先归档？ | 打 tag/分支后删除，保留 `docker/` 等基础设施 | 仓库结构 |
| Q2 | 单 crate 还是多 crate workspace？ | **单 crate 优先**（保目录对齐、降维护成本），后期按需拆分 | 编译/依赖 |
| Q3 | Rust edition/MSRV？ | **edition 2021 + MSRV 1.80.1**（当前环境可编译）；后续可升 2024 | 工具链 |
| Q4 | PD/TiKV 版本？ | 一致性测试固定 **v8.5.x**；本地 9.0 beta 仅做冒烟 | 协议/测试 |
| Q5 | Phase 2 是否包含完整 MySQL 协议？ | 否；先 CLI/`--execute`，完整协议放 Phase 9 | 交付节奏 |
| Q6 | 文档语言？ | 中文为主，代码/标识符英文 | 协作 |
