# 进度看板（Progress）

> **唯一进度事实来源**。每次工作结束必须更新；没有证据 = 未完成。  
> 最后更新：2026-10-19

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
| 0 | 地基与规范 | `in-progress` | 2026-10-19 | — | 见 §4 | workspace/oracle/parity 已就绪；待 TiKV spike/CI |
| 1 | 核心类型与编码 | `not-started` | — | — | — | 依赖 Phase 0 |
| 2 | 解析器 + Mock 链路 | `not-started` | — | — | — | 依赖 Phase 1 |
| 3 | 元数据与 Bootstrap | `not-started` | — | — | — | 依赖 Phase 2 |
| 4 | Planner v1（RBO） | `not-started` | — | — | — | 依赖 Phase 3（可用 Mock InfoSchema 并行） |
| 5 | Executor v1 | `not-started` | — | — | — | 依赖 Phase 4 |
| 6 | 事务与 Session | `not-started` | — | — | — | 可与 Phase 5 并行 |
| 7 | CBO 与统计 | `not-started` | — | — | — | 依赖 Phase 5/6 |
| 8 | DDL 与高级 SQL | `not-started` | — | — | — | 依赖 Phase 6/7 |
| 9 | MySQL 协议与运维 | `not-started` | — | — | — | 可提前做只读协议 |
| 10 | 可选高级能力 | `deferred` | — | — | — | 不阻塞主线 |

---

## 3. 包级状态（Phase 0 基线）

> 当前所有 crate 只有空骨架（`Cargo.toml` + `lib.rs`），尚未登记任何源文件移植。  
> 精确文件级状态由 `project/parity/status.json` + `parity_check.py` 生成。

| 顶层包 | crate | 骨架 | 文件移植 | 目标阶段 |
|---|---|---|---|---|
| `pkg/parser` | `sealdb-parser` | ✅ | 0 | 2 |
| `pkg/types` | `sealdb-types` | ✅ | 0 | 1 |
| `pkg/kv` | `sealdb-kv` | ✅ | 0 | 1/6 |
| `pkg/util` | `sealdb-util` | ✅ | 0 | 1/5 |
| `pkg/config` | `sealdb-config` | ✅ | 0 | 0/9 |
| `pkg/errno` | `sealdb-errno` | ✅ | 0 | 1 |
| `pkg/meta` | `sealdb-meta` | ✅ | 0 | 3 |
| `pkg/infoschema` | `sealdb-infoschema` | ✅ | 0 | 3 |
| `pkg/domain` | `sealdb-domain` | ✅ | 0 | 3/6 |
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
| `pkg/ddl` | `sealdb-ddl` | ✅ | 0 | 8 |
| `pkg/server` | `sealdb-server` | ✅ | 0 | 9 |
| `cmd/tidb-server` | `sealdb-server-bin` | ✅ | 0 | 2/9 |

---

## 4. 当前工作与证据

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
| Gate 0 | `project/reports/phase-00/`（待建） | 进行中 |
| Gate 1 | `project/reports/phase-01/`（待建） | 未开始 |
| Gate 2 | `project/reports/phase-02/`（待建） | 未开始 |
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
