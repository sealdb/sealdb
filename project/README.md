# SealDB × TiDB 计算层 Rust 重写工程手册

> 本目录是项目的**唯一工程事实来源（single source of truth）**。  
> 任何正式编码开始前，先读本目录；任何阶段开工前，先通过该阶段的 Gate。

---

## 1. 项目一句话

用 **Rust** 重写 **TiDB v8.5.8 的计算层**（SQL 协议 → 解析 → 优化 → 执行 → 元数据/事务编排），**完全复用 PD 与 TiKV** 的存储与分布式能力；目录名、文件名、核心算法与 TiDB 保持一致，语言语法糖按 Rust 习惯落地，解析器继续使用 **sqlparser-rs**。

---

## 2. 事实基线（Baseline）

| 项 | 值 | 说明 |
|---|---|---|
| TiDB 参考源码 | `/home/wslu/work/github/db/pingcap/tidb` | 本地目录 |
| TiDB 版本 | **v8.5.8** | `learn/README.md` 与 tag 一致 |
| TiDB 上游提交 | `f396bb4` | 本地 HEAD 仅额外提交 `learn/` |
| TiDB Go 版本 | `go 1.25.12` | 以参考仓库 `go.mod` 为准 |
| PD 本地源码 | `/home/wslu/work/github/db/pingcap/pd` | **v9.0.0-beta.2.pre** |
| TiKV 本地源码 | `/home/wslu/work/github/db/pingcap/tikv` | **v9.0.0-beta.2.pre** |
| Rust 工具链 | `rustc/cargo 1.80.1` | 当前仓库 edition 2024 无法被该 Cargo 解析 |
| SQL 解析 | `sqlparser` **0.63.x**（当前仓库写的是 0.35） | 本项目保留 sqlparser-rs |
| TiKV Rust 客户端 | `tikv-client` **0.4.x**（当前仓库写的是 0.3） | Phase 0 做能力 spike |
| 当前 Rust 代码 | `src/`，玩具级脚手架 | 允许整体丢弃/归档，见 ADR-0007 |

> **版本风险**：TiDB 8.5.8 与本地 PD/TiKV 9.0.0-beta 并非同一发行版组合。  
> 一致性测试优先使用 **PD/TiKV v8.5.x** 镜像；若只能使用本地 9.0 beta，必须在 `progress.md` 记录协议差异与验证结论。

---

## 3. 当前仓库状态（诚实评估）

现有 `src/` 不是 TiDB 的移植，而是另一套通用数据库脚手架：

- `src/sql/src/parser/parser.rs`：**没有真正解析 SQL**，只按关键字返回固定结构。
- `src/storage/src/engine/tikv.rs`：RawClient 可用，但 `begin_transaction` 直接返回未实现。
- `src/sql` 的 planner/optimizer/executor 是自研 RBO/CBO 形状，与 TiDB 目录和算法不对应。
- 根 `Cargo.toml` 使用 `edition = "2024"` / `resolver = "3"`，在 Cargo 1.80.1 上 **`cargo metadata` 直接失败**。
- 现有 `tests/` 是通用集成/回归框架，不是 TiDB 行为对齐测试。

结论：**保留 `docker/`、`monitoring/`、`config/` 作为基础设施参考；`src/` 与旧测试建议按 ADR-0007（待确认）归档后重建。**

---

## 4. 文档地图

| 文档 | 作用 | 谁必须读 |
|---|---|---|
| [requirements.md](./requirements.md) | 目标、范围、硬约束、验收定义 | 所有人 |
| [architecture.md](./architecture.md) | 目标架构、模块划分、目录/文件对齐规则 | 所有人 |
| [conventions.md](./conventions.md) | 编码、移植、错误处理、日志、提交规范 | 所有编码者 |
| [roadmap.md](./roadmap.md) | 阶段拆分、依赖、交付物、Gate | 所有人 |
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

1. **上游对齐**：每个移植文件必须标注 `PORT_SOURCE: <Go 路径> @ v8.5.8`，禁止凭记忆重写。
2. **行为优先于形式**：目录/文件名对齐是硬要求，但真正验收靠 Oracle 差分结果，不靠“看起来像”。
3. **禁止静默空缺**：暂不实现的文件/函数必须有 `PORT_STATUS: stub|omitted` 和原因；空文件必须带注释。
4. **先竖切、再铺开**：每个阶段交付一条可运行的 SQL 竖切链路，禁止“全部类型先写完再联调”。
5. **Gate 不过不进入下一阶段**：验收证据写入 `progress.md`；没有证据 = 未完成。

---

## 6. 推荐阅读顺序

```text
README → requirements → architecture → conventions
      → roadmap → progress
      → testing ⇄ debugging
      → decisions（按需）
      → skills（开工时）
```

---

## 7. 常用命令（规划，Phase 0 落地）

```bash
# 环境自检
./project/scripts/env_check.sh

# 目录/文件对齐审计
python3 project/scripts/parity_check.py \
  --tidb-root /home/wslu/work/github/db/pingcap/tidb \
  --rust-root .

# 安装本项目的 opencode skills
./project/scripts/install-skills.sh

# 构建与测试（Phase 0 之后）
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# Oracle 差分（Phase 0 之后）
./project/scripts/oracle_diff.sh --suite parser-smoke
```

---

## 8. 维护规则

- `progress.md` 每次工作结束必须更新；不允许“下次再记”。
- 任何跨阶段/跨模块的决策先写 ADR，再改代码。
- 任何“与 TiDB 不一致”的行为，必须在 `parity-matrix.md` 的「已知偏差」登记。
- 与本手册冲突的临时做法，必须在 PR/提交说明中引用 ADR 或新增 ADR。
