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
| 0 | 地基与规范 | `in-progress` | 2026-10-19 | — | 本文档集；待补 `Cargo.toml`/Oracle/parity 脚本 | 文档已建；代码地基未动 |
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

> LOC 为 TiDB v8.5.8 **非测试 Go 代码**的近似值；状态针对 **Rust 移植**，不是旧 `src/`。

| TiDB 包 | Go LOC | Rust 状态 | 目标阶段 | 备注 |
|---|---:|---|---|---|
| `cmd/tidb-server` | — | `not-started` | 2/9 | 先 CLI，后协议 |
| `pkg/parser` | 62,473 | `not-started` | 2 | sqlparser bridge |
| `pkg/planner` | 107,549 | `not-started` | 4/7 | RBO/CBO 分批 |
| `pkg/executor` | 98,789 | `not-started` | 5/8 | 算子分批 |
| `pkg/expression` | 79,638 | `not-started` | 2/5 | 核心函数优先 |
| `pkg/session` | 12,726 | `not-started` | 2/6 | 最小 Session 先行 |
| `pkg/sessionctx` | 15,922 | `not-started` | 2/6 | 变量/StmtCtx |
| `pkg/sessiontxn` | — | `not-started` | 6 | 隔离级别/Provider |
| `pkg/kv` | 3,324 | `not-started` | 1/6 | 接口先行 |
| `pkg/store` | 30,912 | `not-started` | 5/6 | driver/copr |
| `pkg/types` | 17,937 | `not-started` | 1 | Datum/Decimal/Time |
| `pkg/meta` | 10,829 | `not-started` | 3 | 元数据 |
| `pkg/infoschema` | 13,262 | `not-started` | 3 | Schema 缓存 |
| `pkg/statistics` | 25,512 | `not-started` | 7 | 直方图/CBO |
| `pkg/domain` | 11,016 | `not-started` | 3/6 | Domain/Reload |
| `pkg/table` | 9,578 | `not-started` | 5 | 表抽象 |
| `pkg/tablecodec` | 2,103 | `not-started` | 1 | Key 编码 |
| `pkg/ddl` | 57,307 | `not-started` | 8 | 在线 DDL |
| `pkg/distsql` | — | `not-started` | 5 | Cop 请求 |
| `pkg/privilege` | — | `not-started` | 8 | 权限 |
| `pkg/bindinfo` | — | `not-started` | 8 | Binding |
| `pkg/config` | — | `not-started` | 0/9 | 配置 |
| `pkg/errno` | — | `not-started` | 1 | 错误码 |
| `pkg/util/chunk` | — | `not-started` | 1/5 | Chunk |
| `pkg/util/codec` | — | `not-started` | 1 | memcomparable |
| `pkg/server` | — | `not-started` | 9 | MySQL 协议 |

> `—` 表示本次统计未单独取数；后续由 `parity_check.py` 自动生成精确值。

---

## 4. 当前工作（Working On）

### 2026-10-19

- **Phase 0 文档地基**：创建 `project/` 全套规范/路线图/进度/测试/调试/Skill/脚本。
- **结论记录**：
  - 现有 `src/` 为玩具脚手架，与 TiDB 目录/算法不对应；建议归档后重建（ADR-0007）。
  - 当前 `Cargo.toml` edition 2024 在本机 Cargo 1.80.1 上无法解析（`cargo metadata` 失败）。
  - 参考 TiDB v8.5.8；本地 PD/TiKV 为 9.0.0-beta.2.pre，需固定测试版本。
- **下一步（Phase 0 剩余）**：
  1. 根 `Cargo.toml` + `rust-toolchain.toml` + 空 `pkg/lib.rs` 骨架。
  2. `tools/oracle/` Go harness（parse 模式先行）。
  3. `project/scripts/parity_check.py` 可运行。
  4. TiKV/PD 连接 spike（TSO/Region/事务/Coprocessor）。

---

## 5. Gate 证据索引

| Gate | 报告/命令 | 状态 |
|---|---|---|
| Gate 0 | `project/reports/phase-00/`（待建） | 未通过 |
| Gate 1 | `project/reports/phase-01/`（待建） | 未开始 |
| Gate 2 | `project/reports/phase-02/`（待建） | 未开始 |
| Gate 3 | `project/reports/phase-03/`（待建） | 未开始 |
| Gate 4 | `project/reports/phase-04/`（待建） | 未开始 |
| Gate 5 | `project/reports/phase-05/`（待建） | 未开始 |
| Gate 6 | `project/reports/phase-06/`（待建） | 未开始 |
| Gate 7 | `project/reports/phase-07/`（待建） | 未开始 |
| Gate 8 | `project/reports/phase-08/`（待建） | 未开始 |
| Gate 9 | `project/reports/phase-09/`（待建） | 未开始 |

> 证据可以是：测试输出、Oracle diff 报告、命令退出码、CI 链接、抓包文件、benchmark 结果。  
> 不接受“口头完成”。

---

## 6. 已知偏差 / 技术债

| ID | 内容 | 影响 | 计划 |
|---|---|---|---|
| D1 | 当前 Rust 代码非 TiDB 移植 | 不能复用 | 归档后重建（ADR-0007） |
| D2 | edition 2024 vs Cargo 1.80 | 无法构建 | 改 edition 2021（ADR-0006） |
| D3 | PD/TiKV 9.0 beta vs TiDB 8.5.8 | 行为/协议风险 | 测试固定 8.5.x |
| D4 | sqlparser-rs 与 TiDB 语法差异 | Parser 覆盖不全 | bridge + 分批补齐（ADR-0003） |
| D5 | tikv-client 能力待验证 | 事务/Cop 可能缺 API | Phase 0 spike（ADR-0004） |
| D6 | 单 crate 编译时间 | 后期可能变慢 | 按需拆 workspace（ADR-0002） |

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
