# 进度看板（Progress）

> **唯一进度事实来源**。每次工作结束必须更新；没有证据 = 未完成。  
> 最后更新：2026-11-01  
> 功能级状态：[features/status.json](./features/status.json)

---

## 1. 状态图例

| 状态 | 含义 |
|---|---|
| `not-started` | 尚未开始 |
| `in-progress` | 进行中（必须写负责人/下一步） |
| `blocked` | 被阻塞（必须写阻塞原因/解除条件） |
| `review` | 开发完成，等待 Gate 评审 |
| `done` | Gate 通过，有证据 |
| `deferred` | 本阶段不做（记录到后续阶段） |

---

## 2. 阶段状态

| Phase | 名称 | 状态 | 开始 | 完成 | Gate 证据 | 备注 |
|---|---|---|---|---|---|---|
| 0 | 地基与规范 | `done` | 2026-10-19 | 2026-11-01 | U0 | workspace/CI/parity/spike 书面结论 |
| 1 | 核心类型与编码 | `done` | 2026-11-01 | 2026-11-01 | U1 | int/uint/float/bytes + row/index key；Decimal/Time 核心路径 |
| 2 | 解析器 + Mock 链路 | `in-progress` | 2026-11-01 | — | U2/U3/F01 | `SELECT 1` 与 F01 表达式已通；parser 全量 60% 留完整性波 |
| 3 | 元数据与 Bootstrap | `done` | 2026-11-01 | 2026-11-01 | F02/F11 | SHOW/DESC 走内存 InfoSchema |
| 4 | Planner v1（RBO） | `in-progress` | 2026-11-01 | — | F03–F10 | F03 TableReader 已通；WHERE 起才有规则 |
| 5 | Executor v1 | `in-progress` | 2026-11-01 | — | F03–F10 | INSERT + SELECT * 走内存 KV |
| 6 | 事务与 Session | `in-progress` | 2026-11-01 | — | F12/F13 | F12 乐观 RR 已通 |
| 7 | CBO 与统计 | `not-started` | — | — | F21 | 依赖 Phase 5/6 |
| 8 | DDL 与高级 SQL | `not-started` | — | — | F14–F20 | 依赖 Phase 6/7 |
| 9 | MySQL 协议与运维 | `not-started` | — | — | F22 | 可提前做只读协议 |
| 10 | 可选高级能力 | `deferred` | — | — | C6 | 完整性波不阻塞主线 |

### 2.1 功能波进度

| ID | 状态 | 证据 |
|---|---|---|
| U0–U3 | `core-ported` | workspace / codec tests / `--parse` / `SELECT 1` |
| F01 | `core-ported` | `project/reports/features/F01/` |
| F02 | `core-ported` | `project/reports/features/F02/` |
| F11 | `core-ported` | `project/reports/features/F11/` |
| F03 | `core-ported` | `project/reports/features/F03/` |
| F04 | `core-ported` | `project/reports/features/F04/` |
| F05 | `core-ported` | `project/reports/features/F05/` |
| F06 | `core-ported` | `project/reports/features/F06/` |
| F07 | `core-ported` | `project/reports/features/F07/` |
| F08 | `core-ported` | `project/reports/features/F08/` |
| F09 | `core-ported` | `project/reports/features/F09/` |
| F10 | `core-ported` | `project/reports/features/F10/` |
| F12 | `core-ported` | `project/reports/features/F12/` |
| F13–F22 | `not-started` | — |
| C1 | `in-progress` | `project/scripts/leftover_inventory.py` |
| C2–C5 | `not-started` | — |
| C6 | `deferred` | Cascades/TiFlash/MPP/BR |

---

## 3. 包级状态（Phase 0 基线）

> U1/U2/U3/F01 相关 crate 已有核心路径实现；其余 crate 仍是可编译骨架。  
> 精确文件级状态由 `project/parity/status.json` + `parity_check.py` 生成。

| 顶层包 | crate | 骨架 | 文件移植 | 目标阶段 |
|---|---|---|---|---|
| `pkg/parser` | `sealdb-parser` | ✅ | 核心 | 2/3 |
| `pkg/types` | `sealdb-types` | ✅ | 核心 | 1 |
| `pkg/kv` | `sealdb-kv` | ✅ | 0 | 1/6 |
| `pkg/util` | `sealdb-util` | ✅ | 核心 | 1/5 |
| `pkg/config` | `sealdb-config` | ✅ | 0 | 0/9 |
| `pkg/errno` | `sealdb-errno` | ✅ | 核心 | 1 |
| `pkg/meta` | `sealdb-meta` | ✅ | 核心 | 3 |
| `pkg/infoschema` | `sealdb-infoschema` | ✅ | 核心 | 3 |
| `pkg/domain` | `sealdb-domain` | ✅ | 核心 | 3/6 |
| `pkg/expression` | `sealdb-expression` | ✅ | 0 | 2/5 |
| `pkg/planner` | `sealdb-planner` | ✅ | 0 | 4/7 |
| `pkg/executor` | `sealdb-executor` | ✅ | 0 | 5/8 |
| `pkg/session` | `sealdb-session` | ✅ | 0 | 2/6 |
| `pkg/sessionctx` | `sealdb-sessionctx` | ✅ | 0 | 2/6 |
| `pkg/sessiontxn` | `sealdb-sessiontxn` | ✅ | 0 | 6 |
| `pkg/store` | `sealdb-store` | ✅ | 0 | 5/6 |
| `pkg/distsql` | `sealdb-distsql` | ✅ | 0 | 5 |
| `pkg/table` | `sealdb-table` | ✅ | 0 | 5 |
| `pkg/tablecodec` | `sealdb-tablecodec` | ✅ | 0 | 1 |
| `pkg/statistics` | `sealdb-statistics` | ✅ | 0 | 7 |
| `pkg/privilege` | `sealdb-privilege` | ✅ | 0 | 8 |
| `pkg/bindinfo` | `sealdb-bindinfo` | ✅ | 0 | 8 |
| `pkg/ddl` | `sealdb-ddl` | ✅ | 核心 | 3/8 |
| `pkg/server` | `sealdb-server` | ✅ | 0 | 9 |
| `cmd/tidb-server` | `sealdb-server-bin` | ✅ | 0 | 2/9 |

---

## 4. 当前工作与证据

### 2026-11-01

- **Phase**: F12 BEGIN/COMMIT/ROLLBACK + 乐观 RR
- **完成**:
  - 显式事务与 autocommit 共用 session-owned txn
  - 内存 KV 快照读 + 提交写写冲突
- **验证**:
  - `./project/scripts/feature_gate.sh F12`
- **下一步**:
  - F13 `SELECT ... FOR UPDATE` / 悲观 RR

### 2026-11-01

- **Phase**: F10 二级索引 / IndexLookUp
- **完成**:
  - `CREATE TABLE ... INDEX/KEY` 写入 `IndexInfo`
  - INSERT/UPDATE/DELETE 维护二级索引 key
  - 索引列等值走 `IndexLookUpExecutor`
- **验证**:
  - `./project/scripts/feature_gate.sh F10`
- **下一步**:
  - F12 事务 / BEGIN COMMIT ROLLBACK

### 2026-11-01

- **Phase**: F09 HashJoin
- **完成**:
  - 两表 Inner / Left / Right HashJoin
  - 等值 ON 条件
- **验证**:
  - `./project/scripts/feature_gate.sh F09`
- **下一步**:
  - F10 二级索引 / IndexLookUp

### 2026-11-01

- **Phase**: F08 聚合 / GROUP BY
- **完成**:
  - `COUNT/SUM/AVG/MIN/MAX` 走内存 HashAgg
  - 单表 `GROUP BY` 列分组
- **验证**:
  - `./project/scripts/feature_gate.sh F08`
- **下一步**:
  - F09 HashJoin

### 2026-11-01

- **Phase**: F07 UPDATE / DELETE
- **完成**:
  - 单表 `UPDATE SET` / `DELETE` 写内存 KV
  - WHERE 过滤复用 Selection 表达式
- **验证**:
  - `./project/scripts/feature_gate.sh F07`
- **下一步**:
  - F08 聚合 / GROUP BY

### 2026-11-01

- **Phase**: F06 PointGet
- **完成**:
  - `WHERE id = const` 走 handle PointGet
  - INSERT 用第一列整数当 handle
- **验证**:
  - `./project/scripts/feature_gate.sh F06`
- **下一步**:
  - F07 UPDATE / DELETE

### 2026-11-01

- **Phase**: F05 ORDER BY / LIMIT
- **完成**:
  - `ORDER BY` 内存排序
  - `LIMIT` / `OFFSET`
- **验证**:
  - `./project/scripts/feature_gate.sh F05`
- **下一步**:
  - F06 PointGet

### 2026-11-01

- **Phase**: F04 WHERE / Selection
- **完成**:
  - `WHERE` 比较/逻辑过滤
  - `SelectionExec` 包 TableReader / Dual
- **验证**:
  - `./project/scripts/feature_gate.sh F04`
  - 见 `project/reports/features/F04/`
- **阻塞/风险**:
  - 无 PPD / `IN` / `LIKE`
- **下一步**:
  - F05 `ORDER BY` / `LIMIT`

### 2026-11-01

- **Phase**: F03 INSERT + SELECT *
- **完成**:
  - `INSERT ... VALUES` 写入 TiDB 形 row key
  - `SELECT *` 经 TableReader 扫描表前缀
  - Domain 持有 `MemStorage`，单测/feature gate 不依赖实集群
- **验证**:
  - `./project/scripts/feature_gate.sh F03`
  - 见 `project/reports/features/F03/`
- **阻塞/风险**:
  - 行值暂用 tagged 布局，不是 TiDB rowcodec
  - 实集群 PD/TiKV 镜像仍在拉取；host-side tikv-client 还要改 advertise
- **下一步**:
  - F04 `WHERE` / Selection

### 2026-11-01

- **Phase**: F02 元数据
- **完成**:
  - `CREATE/DROP DATABASE|TABLE` 核心路径
  - TiDB 兼容 meta key（`m` + hash/string encoding，`DB:{id}` / `Table:{id}`）
  - InfoSchema 内存快照 + Domain reload
- **验证**:
  - `./project/scripts/feature_gate.sh F02`
  - 见 `project/reports/features/F02/`
- **阻塞/风险**:
  - 仍是内存 meta，未写入真实 TiKV
- **下一步**:
  - F11 `SHOW` / `DESC`，或 F03 `INSERT` + `SELECT *`

### 2026-11-01

- **Phase**: 功能波 Unlock + F01
- **完成**:
  - workspace 空 crate 可编译；`[workspace.dependencies]` 落地
  - U1 codec/tablecodec 核心向量与 row/index key
  - U2 sqlparser_bridge + `--parse --ast-json`
  - U3 `sealdb-server --execute "SELECT 1"`
  - F01 算术/比较/逻辑/`CAST`/`IF`/`IFNULL`
  - 功能清单 `project/features.md` + leftover 脚本 + CI
- **验证**:
  - `cargo test --workspace` / `cargo clippy --workspace --all-targets -- -D warnings`
  - 见 `project/reports/features/F01/`
- **阻塞/风险**:
  - 本机 Homebrew cargo 是 1.97；需 `rustup` 1.99.1
  - TiKV spike 仍缺实集群；结论见 ADR-0004
- **下一步**:
  - F02 `CREATE/DROP TABLE` 写兼容 meta key

### 2026-10-19

**决策落地**

- 旧代码移动到 `legacy-rust-scaffold/`（`src/`、`tests/`、`docs/`、旧 `Cargo.toml`、旧 `Makefile`）。
- 新 workspace：24 个 `pkg/*` crate + `cmd/tidb-server`，顶层包粒度，共 25 个 package。
- Rust 升级到 **1.99.1**（rsproxy 镜像），edition 2024，resolver 3。
- 版本统一：TiDB/PD/TiKV 基线 **v8.5.8**；本地 9.0 beta 仅冒烟。
- 源码不再使用来源/移植标记；状态改由 `project/parity/status.json` 维护。

**已验证**

```text
cargo metadata --no-deps   -> 25 packages
cargo check --workspace    -> success (1.92s)
tools/oracle parse         -> SELECT `id`,`name` FROM `users` WHERE `id`=1
```

**待完成（Phase 0 剩余）**

1. `tools/oracle` 的 `plan` / `execute` 模式。
2. `project/parity/status.json` 初始登记 + `--strict` 通过。
3. TiKV/PD 连接 spike（TSO/Region/事务/Coprocessor）。
4. CI 基线（fmt/clippy/test/oracle smoke）。
5. 根 Makefile 重写（新命令）。
6. `docker/` 镜像固定到 v8.5.8。

**TiKV spike 状态**

- 已添加 `tools/spikes/tikv-client`（乐观事务最小示例 + 能力清单）。
- 当前环境未运行 PD/TiKV，且 rsproxy 索引缺少 `web-sys 0.3.106`，编译验证暂未完成。
- 解除条件：启动 v8.5.8 集群，或切换可用的 crates 稀疏索引/降级 web-sys。

---

## 5. Gate 证据索引

| Gate | 报告/命令 | 状态 |
|---|---|---|
| Gate 0 | U0 + CI | 通过（功能波口径） |
| Gate 1 | U1 codec 单测 | 通过（核心类型） |
| Gate 2 | U2/U3/F01 | 进行中（核心路径已通，parser 60% 留完整性波） |
| Gate 3 | `project/reports/phase-03/`（待建） | 未开始 |
| Gate 4 | `project/reports/phase-04/`（待建） | 未开始 |
| Gate 5 | `project/reports/phase-05/`（待建） | 未开始 |
| Gate 6 | `project/reports/phase-06/`（待建） | 未开始 |
| Gate 7 | `project/reports/phase-07/`（待建） | 未开始 |
| Gate 8 | `project/reports/phase-08/`（待建） | 未开始 |
| Gate 9 | `project/reports/phase-09/`（待建） | 未开始 |

---

## 6. 已知偏差 / 技术债

| ID | 内容 | 影响 | 计划 |
|---|---|---|---|
| D1 | 旧 Rust 代码已归档 | 不可复用 | 保持归档 |
| D2 | ~~edition 2024 vs Cargo 1.80~~ | 已解决 | Rust 1.99.1 |
| D3 | 本地 PD/TiKV 仍可能是 9.0 beta | 行为/协议风险 | 测试固定 8.5.8 |
| D4 | sqlparser-rs 与目标语法差异 | Parser 覆盖不全 | bridge + 分批补齐（ADR-0003） |
| D5 | tikv-client 能力待验证 | 事务/Cop 可能缺 API | Phase 0 spike（ADR-0004） |
| D6 | workspace crate 边界 | 需显式依赖 | 按顶层包维护；循环依赖单独讨论 |
| D7 | 源码不含来源标记 | 追溯依赖 manifest | 强制更新 `project/parity/status.json` |

---

## 7. 更新模板

每次更新追加一条：

```markdown
### YYYY-MM-DD

- **Phase**: 0X-名称
- **完成**:
  - ...
- **验证**:
  - 命令/结果/报告路径
- **阻塞/风险**:
  - ...
- **下一步**:
  - ...
```
