# 目录/文件对齐矩阵（Parity Matrix）

> 目标：**任何时刻都能回答**：TiDB 的某个包/文件，Rust 侧是否存在、处于什么状态、由谁负责、差异是什么。  
> 本文件是人工摘要；精确报告由 `project/scripts/parity_check.py` 生成。

---

## 1. 状态定义

| 状态 | 含义 |
|---|---|
| `ported` | 已移植，核心逻辑一致，测试通过 |
| `partial` | 部分实现，未实现分支有明确错误 |
| `reimplemented` | 因语言/库差异重写，行为对齐 |
| `stub` | 接口/骨架存在，未实现 |
| `omitted` | 本阶段不需要，保留文件名/说明 |
| `not-started` | 尚未创建对应 `.rs` |
| `n/a` | 非计算层/不移植（如 `br/`、`dumpling/`） |

---

## 2. 顶层目录

| TiDB 目录 | Rust 目录 | 状态 | 阶段 | 说明 |
|---|---|---|---|---|
| `cmd/tidb-server` | `cmd/tidb-server` | `partial` | 2/9 | CLI `--parse/--execute` |
| `pkg/parser` | `pkg/parser` | `partial` | 2/3 | sqlparser bridge + SELECT/DDL AST |
| `pkg/types` | `pkg/types` | `partial` | 1 | Datum/Decimal/Time 核心 |
| `pkg/kv` | `pkg/kv` | `stub` | 1/6 | 接口先行 |
| `pkg/meta` | `pkg/meta` | `partial` | 3 | 兼容 meta key + DB/Table JSON |
| `pkg/infoschema` | `pkg/infoschema` | `partial` | 3 | 内存 Schema 快照 |
| `pkg/domain` | `pkg/domain` | `partial` | 3/6 | Domain reload |
| `pkg/expression` | `pkg/expression` | `partial` | 2/5 | F01 标量 |
| `pkg/planner` | `pkg/planner` | `partial` | 4/7 | Dual/Projection |
| `pkg/executor` | `pkg/executor` | `partial` | 5/8 | Dual/Projection |
| `pkg/session` | `pkg/session` | `partial` | 2/3/6 | ExecuteStmt 常量 SELECT + DDL |
| `pkg/sessionctx` | `pkg/sessionctx` | `not-started` | 2/6 | 上下文 |
| `pkg/sessiontxn` | `pkg/sessiontxn` | `not-started` | 6 | 事务 Provider |
| `pkg/store` | `pkg/store` | `not-started` | 5/6 | driver/copr |
| `pkg/distsql` | `pkg/distsql` | `not-started` | 5 | Cop 请求 |
| `pkg/ddl` | `pkg/ddl` | `partial` | 3/8 | CREATE/DROP 立即生效，无 job 队列 |
| `pkg/statistics` | `pkg/statistics` | `not-started` | 7 | 统计 |
| `pkg/table` | `pkg/table` | `not-started` | 5 | 表抽象 |
| `pkg/tablecodec` | `pkg/tablecodec` | `partial` | 1 | row/index key |
| `pkg/errno` | `pkg/errno` | `partial` | 1 | 核心错误码 |
| `pkg/util/chunk` | `pkg/util/chunk` | `partial` | 1/5 | Chunk |
| `pkg/util/codec` | `pkg/util/codec` | `ported` | 1 | int/uint/float/bytes |
| `pkg/privilege` | `pkg/privilege` | `not-started` | 8 | 权限 |
| `pkg/bindinfo` | `pkg/bindinfo` | `not-started` | 8 | Binding |
| `pkg/config` | `pkg/config` | `not-started` | 0/9 | 配置 |
| `pkg/server` | `pkg/server` | `not-started` | 9 | MySQL 协议 |
| `br/` | `br/` | `n/a` | — | 占位 README |
| `dumpling/` | `dumpling/` | `n/a` | — | 占位 README |
| `lightning/` | `lightning/` | `n/a` | — | 占位 README |
| `tiflash/` | `tiflash/` | `n/a` | — | 占位 README |
| `tispark/` | `tispark/` | `n/a` | — | 占位 README |

---

## 3. 文件级报告（自动生成）

状态唯一来源：`project/parity/status.json`。`parity_check.py` 输出：

```text
pkg/parser/parser.go                         -> pkg/parser/parser.rs        [stub]
pkg/parser/lexer.go                          -> pkg/parser/lexer.rs         [partial]
pkg/executor/analyze.go                      -> (omitted)                   [omitted]
pkg/executor/foo.go                          -> (missing)                   [not-started]
pkg/executor/foo.rs                          -> (unmapped)                  [invalid]
pkg/parser/sqlparser_bridge/lower.rs         -> (extra, allowed)            [bridge]
```

### 3.1 使用

```bash
python3 project/scripts/parity_check.py \
  --tidb-root /home/wslu/work/github/db/pingcap/tidb \
  --rust-root . \
  --json > project/reports/parity.json

python3 project/scripts/parity_check.py \
  --tidb-root /home/wslu/work/github/db/pingcap/tidb \
  --rust-root . \
  --require pkg/parser,pkg/types,pkg/util/codec \
  --strict
```

### 3.2 判定

| 情况 | 结果 |
|---|---|
| manifest 标 `ported/partial/stub/reimplemented` 且 `.rs` 存在 | 计入对应状态 |
| manifest 标 `omitted` | `omitted`（不要求 `.rs`） |
| manifest 未登记 | `not-started` |
| `.rs` 存在但 manifest 未登记 | `invalid`（`--strict` 失败） |
| 额外 `.rs` 不在白名单 | `extra`（评审确认） |
| 目录缺失 | `missing-dir`（阶段 Gate 失败） |

---

## 4. 已知偏差（Known Deviations）

| ID | 偏差 | 原因 | 影响 | 处理 |
|---|---|---|---|---|
| K1 | `pkg/parser/sqlparser_bridge/` 额外目录 | sqlparser-rs → TiDB AST 桥接 | 目录不完全 1:1 | 允许；见 architecture R9 |
| K2 | `mod.rs` / `lib.rs` | Rust 模块系统 | 额外构建文件 | 允许；R7 |
| K3 | `parser.y` / `hintparser.y` 仅参考 | 不移植 goyacc | 文法文件不参与编译 | 保留占位注释 |
| K4 | `parser.go` 对应 `parser.rs` 为手写 | goyacc 生成代码不可直接移植 | 实现方式不同，行为对齐 | `reimplemented` |
| K5 | workspace 顶层包粒度 | 目录对齐 + 编译边界 | 每包一个 `Cargo.toml` | ADR-0002 |
| K6 | `tikv-client` 替代 Go 客户端 | Rust 生态 | 部分 API 适配/兜底 | ADR-0004 |
| K7 | Rust 1.99.1 / edition 2024 | 近期工具链 | 需安装 1.99.1 | ADR-0006 |
| K8 | Mock stats（Phase 4） | 分阶段 | CBO 精度后置 | Phase 7 替换 |
| K9 | 暂不移植 `br/`、`dumpling/`、`lightning/`、`tiflash/`、`tispark/` | 非计算层主链路 | 目录占位 | n/a |
| K10 | 源码不含来源标记 | 用户要求 | 追溯依赖 manifest | `status.json` |
| K11 | `legacy-rust-scaffold/` | 归档旧代码 | 不参与构建 | ADR-0007 |
| K12 | `pkg/parser/mysql/const.rs` 模块名为 `r#const` | `const` 是 Rust 关键字 | 文件名仍是 `const.rs` | 允许；`pub use r#const::*` |
| K13 | `foo/foo.rs` 触发 `module_inception` | 对齐 `foo/foo.go` | 模块路径多一层 | 允许 clippy；禁止并进 `mod.rs` |

> 新增偏差必须同时更新：本表 + ADR + `requirements.md` §3（如涉及硬约束）。

---

## 5. 更新规则

1. 新增/修改文件后，运行 `parity_check.py`，把摘要粘贴到本文件 §2（如变化）。
2. 文件从 `stub` → `partial` → `ported`，必须同时更新 `progress.md`。
3. 不允许手工把 `not-started` 改成 `ported` 而没有测试证据。
4. 每次 Phase Gate 生成 `project/reports/phase-XX/parity.json` 并归档。
