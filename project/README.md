# SealDB 计算层工程手册

> 本目录是项目的**唯一工程事实来源（single source of truth）**。  
> 任何正式编码开始前，先读本目录；任何阶段开工前，先通过该阶段的 Gate。

---

## 1. 项目一句话

用 **Rust** 重写 **TiDB v8.5.8 的计算层**（SQL 协议 → 解析 → 优化 → 执行 → 元数据/事务编排），**完全复用 PD 与 TiKV** 的存储与分布式能力；目录名、文件名、核心算法与 TiDB 保持一致，语言语法糖按 Rust 习惯落地，解析器继续使用 **sqlparser-rs**。

> 源码中不写“移植自 TiDB”之类的来源标记；对齐状态统一维护在 `project/parity/status.json`。

---

## 2. 事实基线（Baseline）

| 项 | 值 | 说明 |
|---|---|---|
| TiDB 参考源码 | `/home/wslu/work/github/db/pingcap/tidb` | 本地目录 |
| TiDB 版本 | **v8.5.8** | `learn/README.md` 与 tag 一致 |
| TiDB 上游提交 | `f396bb4` | 本地 HEAD 仅额外提交 `learn/` |
| TiDB Go 版本 | `go 1.25.12` | 以参考仓库 `go.mod` 为准 |
| PD 基线 | **v8.5.8** | 本地 checkout 可能是 9.0 beta；测试用 8.5.8 |
| TiKV 基线 | **v8.5.8** | 本地 checkout 可能是 9.0 beta；测试用 8.5.8 |
| Rust 工具链 | **1.99.1**（edition 2024） | 已通过 rsproxy 安装 |
| SQL 解析 | `sqlparser` **0.63.x** | 本项目保留 sqlparser-rs |
| TiKV Rust 客户端 | `tikv-client` **0.4.x** | Phase 0 做能力 spike |
| 旧 Rust 代码 | （已删除） | 原 `legacy-rust-scaffold/`；见 ADR-0007 / git 历史 |

> **版本统一**：TiDB / PD / TiKV 统一到 **v8.5.8** 发行线。  
> 本地 PD/TiKV 若仍在 9.0 beta，仅用于快速冒烟；一致性测试必须使用 8.5.8 镜像或切到对应 tag。

---

## 3. 当前仓库状态

- 旧 `src/`、`tests/`、`docs/`、`Cargo.toml`、`Makefile` 曾归档为 `legacy-rust-scaffold/`，现已删除（ADR-0007）。
- 新 workspace 已建立：`pkg/` 下按 TiDB 顶层包粒度拆 crate，`cmd/tidb-server` 为入口。
- `cargo check --workspace` / `cargo test --workspace` / clippy `-D warnings` 通过（Rust 1.99.1）。
- U0–U3 与 F01 已 `core-ported`：codec 向量、`--parse --ast-json`、`SELECT 1`、算术/比较/逻辑/`CAST`/`IF`。
- `tools/oracle` 的 `parse` 模式可运行；功能清单在 `project/features.md`。
- `project/scripts/parity_check.py` 与 `feature_gate.sh` 已可运行。

---

## 4. 文档地图

| 文档 | 作用 | 谁必须读 |
|---|---|---|
| [requirements.md](./requirements.md) | 目标、范围、硬约束、验收定义 | 所有人 |
| [architecture.md](./architecture.md) | 目标架构、模块划分、目录/文件对齐规则 | 所有人 |
| [conventions.md](./conventions.md) | 编码、移植、错误处理、日志、提交规范 | 所有编码者 |
| [roadmap.md](./roadmap.md) | 阶段拆分、依赖、交付物、Gate | 所有人 |
| [features.md](./features.md) | **功能级 backlog**（日常迭代单位） | 所有人 |
| [progress.md](./progress.md) | **唯一进度看板** | 每次开工/收工 |
| [testing.md](./testing.md) | Oracle、一致性、差分、集成测试 | 编码 + QA |
| [debugging.md](./debugging.md) | 分层排障、EXPLAIN、Trace、故障手册 | 编码 + on-call |
| [parity-matrix.md](./parity-matrix.md) | Go→Rust 包/文件对齐状态 | 所有人 |
| [risks.md](./risks.md) | 风险登记、开放问题、假设 | 负责人 |
| [glossary.md](./glossary.md) | 术语表 | 新人 |
| [decisions/](./decisions/) | ADR（架构决策记录） | 所有人 |
| [skills/](./skills/) | 可复用的移植/审计/差分/调试 Skill | Agent + 人 |
| [scripts/](./scripts/) | parity 检查、skill 安装等脚本 | 所有人 |

---

## 5. 五条铁律

1. **上游对齐**：移植前必须读上游实现、调用方与测试；禁止凭记忆重写。来源与状态记录在 `project/parity/status.json`，不写进源码。
2. **行为优先于形式**：目录/文件名对齐是硬要求，但真正验收靠 Oracle 差分结果，不靠“看起来像”。
3. **禁止静默空缺**：暂不实现的文件/函数必须在 parity manifest 标 `stub|omitted` 并说明原因；源码中只保留通用 `todo!()`/错误返回，不写来源说明。
4. **先功能竖切、再完整性波**：一次迭代只交付一条 SQL 能力的核心路径；边角记 `gaps[]`，全部功能 `core-ported` 后再查漏补缺。
5. **功能 Gate 不过不宣称完成**：`core-ported` 证据写入 `progress.md` 与 `project/reports/features/`；没有证据 = 未完成。基础设施 U0/U1 仍是硬门。

---

## 6. 推荐阅读顺序

```text
README → requirements → architecture → conventions
      → roadmap → features → progress
      → testing ⇄ debugging
      → decisions（按需）
      → skills（开工时）
```

---

## 7. 常用命令

```bash
# 环境自检
./project/scripts/env_check.sh

# 目录/文件对齐审计
python3 project/scripts/parity_check.py \
  --tidb-root /home/wslu/work/github/db/pingcap/tidb \
  --rust-root .

# 安装本项目的 opencode skills
./project/scripts/install-skills.sh

# 构建与测试
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# Oracle 冒烟
cd tools/oracle && go run ./cmd/tidb-oracle -sql "select 1" -pretty
```

---

## 8. 维护规则

- `progress.md` 每次工作结束必须更新；不允许“下次再记”。
- 任何跨阶段/跨模块的决策先写 ADR，再改代码。
- 任何“与参考实现不一致”的行为，必须在 `parity-matrix.md` 的「已知偏差」登记。
- 与本手册冲突的临时做法，必须在 PR/提交说明中引用 ADR 或新增 ADR。
