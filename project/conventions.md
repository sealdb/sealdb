# 编码与移植规范（Conventions）

> 目标：让不同人/Agent 产出的代码**可读、可对齐、可回归**。  
> 与本文冲突的写法，必须在评审中说明理由；长期偏差写入 ADR。

---

## 1. Rust 基础规范

| 项 | 规则 |
|---|---|
| Edition | **2024**（见 ADR-0006） |
| MSRV | **1.99.1**；升级需 ADR |
| 格式化 | `cargo fmt --all`，禁止手调对齐 |
| Lint | `cargo clippy --workspace --all-targets -- -D warnings` |
| 命名 | 类型 `CamelCase`；函数/变量 `snake_case`；常量 `SCREAMING_SNAKE_CASE`；模块 `snake_case` |
| 可见性 | 默认私有；跨模块用 `pub(crate)`；对外 API 才 `pub` |
| Unsafe | 默认 `#![forbid(unsafe_code)]`；确需时 ADR + 最小封装 + 安全注释 + 测试 |
| 依赖 | 版本集中到根 `[workspace.dependencies]`；禁止随意引入重依赖 |
| 错误 | 核心库用 `thiserror`；二进制入口可用 `anyhow`；禁止空错误处理 |
| 日志 | 用 `tracing`；库内禁止 `println!`；测试可用 `eprintln!` |
| 异步 | 统一 `tokio`；禁止在 async 中执行阻塞 IO/长 CPU 循环（必要时 `spawn_blocking`） |
| 时间 | 统一 UTC；时间戳用 `u64`（TiDB 语义），展示层再格式化 |
| 数值 | 金额/精确小数用 `MyDecimal`（自实现/移植），禁止 `f64` 代替 DECIMAL |

---

## 2. 移植工作流（每个文件）

### 2.1 读上游（必须先做）

1. 找到 TiDB 对应 `.go` 文件与**所有调用方**（`rg "FuncName\("`）。
2. 找到对应测试（`*_test.go`）与 testdata。
3. 记录：输入/输出、错误分支、并发假设、与其它包的契约。
4. 若文件是生成代码，找到生成器/文法，理解语义后再写 Rust 替代。

### 2.2 写 Rust

1. 建同名 `.rs`，在 `project/parity/status.json` 登记状态与说明（见 §3）。
2. 先翻译类型/数据结构，再翻译函数；保持函数名与职责可对应（Rust 命名可调整大小写）。
3. 保留上游注释中的**关键算法说明**；删除纯语言噪音注释。
4. 错误码/错误消息：优先保持一致；无法一致时加 `PARITY_NOTES`。
5. 未实现部分：用 `todo!()` **仅限** manifest 标 `stub` 的文件，并在函数上写通用原因/计划阶段（不写来源路径）。
6. 不得在移植文件中顺手“优化算法”；优化另开任务/ADR。

### 2.3 测试

1. 优先把 Go 表驱动测试翻译为 Rust `#[test]`/`#[tokio::test]`。
2. 至少覆盖：正常路径、边界、错误路径。
3. 涉及编码/类型/计划的文件，必须加入 Oracle 差分用例（见 `testing.md`）。
4. 测试命名：`test_<行为>_<条件>`，与上游测试名尽量可对应。

### 2.4 收尾

- [ ] `cargo fmt` / `clippy` / `test` 通过
- [ ] 更新 `progress.md` 与 `parity-matrix.md`
- [ ] 记录已知差异
- [ ] 如引入新依赖，说明 License 与替代方案

---

## 3. 来源与状态记录

源码中**不写**来源/移植标记（C9）。对齐状态统一登记在 `project/parity/status.json`：

```json
{
  "baseline": "v8.5.8",
  "files": {
    "pkg/planner/core/optimizer.go": {
      "status": "ported",
      "rust": "pkg/planner/core/optimizer.rs",
      "phase": 4,
      "notes": "rule order aligned; cascades not enabled"
    },
    "pkg/executor/analyze.go": {
      "status": "omitted",
      "phase": 7,
      "notes": "statistics phase"
    }
  }
}
```

规则：

- `rust` 可省略；默认与 Go 文件同路径的 `.rs`。
- `ported` / `partial` / `reimplemented` / `stub`：`.rs` 必须存在。
- `omitted`：不要求 `.rs`；`notes` 必填（原因 + 计划阶段）。
- 未登记：默认 `not-started`。
- `.rs` 存在但未登记：`invalid`（CI 失败）。
- 源码中的未实现只写通用说明，例如：

```rust
pub fn analyze_table() -> Result<()> {
    todo!("phase 07: statistics")
}
```

```rust
pub fn analyze_table() -> Result<()> {
    Err(Error::Unsupported("ANALYZE is not available in this phase"))
}
```

禁止在源码中写 `// ported from ...`、`// upstream: ...`、`// TiDB ...` 等来源信息。

---

## 4. 目录/文件对齐规范

| 场景 | 做法 |
|---|---|
| 新增 TiDB 文件 | 先建同名 `.rs`（即使 `omitted`），再逐步实现 |
| 删除 TiDB 文件 | 不允许直接删除；改为 `omitted` + 原因 |
| 重命名 | 不允许；Rust 侧用类型/函数别名兼容 |
| 合并文件 | 不允许（会破坏 parity）；可在 `mod.rs` 重导出 |
| 拆分文件 | 不允许新增 TiDB 树内文件；桥接代码放 `sqlparser_bridge/` 或 `tools/` |
| 测试文件 | 默认内联；独立文件用 `foo_test.rs` 并在 `mod.rs` 声明 |
| 生成文件 | 用 `reimplemented` + 说明生成方式；禁止提交无法维护的巨型生成代码 |

---

## 5. 错误处理规范

```rust
// 推荐：结构化错误
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("unknown table `{0}`")]
    UnknownTable(String),
    #[error("can't find a proper physical plan for this query")]
    NoPhysicalPlan,
}
```

规则：

1. 库函数返回 `Result<T, E>`，错误类型具体；不要返回 `Box<dyn Error>`。
2. 需要跨层传递时，用 `#[from]` 包装，保留 source。
3. 错误码（MySQL errno / SQLSTATE）在协议层映射；核心层保留语义错误。
4. 禁止用 `unwrap()` 处理外部输入；初始化常量可 `expect("invariant: ...")`。
5. 不允许“记录日志后继续”掩盖错误；要么恢复，要么返回。
6. 与 TiDB 错误消息对齐的测试，使用 `assert_eq!(err.code(), ...)` 或 `contains`，避免脆弱的整串比较。

---

## 6. 并发与异步规范

- 一个 Session 一个逻辑执行上下文；共享状态用 `Arc` + 明确同步原语。
- 禁止在持有锁时 `.await`（除非使用 `tokio::sync` 且证明无死锁）。
- 长 CPU 任务（排序/聚合/编码）用 `spawn_blocking` 或专用线程池；不得阻塞 tokio worker。
- 取消：所有长任务接受 `CancellationToken`/`AtomicBool`；`Close` 必须可中断。
- 重试：KV/Cop 错误使用统一 Backoffer（对齐 TiDB 语义）；禁止无限重试。

---

## 7. 日志与可观测性规范

- 使用结构化字段：`tracing::info!(conn_id, stmt_id, table_id, "message")`。
- 禁止打印完整 SQL 中的敏感信息（密码、密钥）；需要时用 `SecureText`。
- 每个阶段至少一个 span；错误路径记录 `error!`，正常路径用 `debug!`。
- 性能敏感路径避免字符串拼接；用 `enabled!` 判断后再构造字段。

---

## 8. 依赖规范

| 类别 | 允许 | 说明 |
|---|---|---|
| 异步 | `tokio` | 统一运行时 |
| 序列化 | `serde`, `serde_json`, `prost` | prost 用于 protobuf |
| 错误 | `thiserror` | 核心库 |
| 日志 | `tracing`, `tracing-subscriber` | 统一 |
| SQL 解析 | `sqlparser` 0.63.x | 固定 minor；升级需回归 |
| TiKV | `tikv-client` 0.4.x | 封装在 store/driver |
| 编码 | 自实现 `util/codec` | 必须与 TiDB 字节级一致 |
| 并发 | `parking_lot`, `dashmap` | 按需；优先 std/tokio |
| 测试 | `tempfile`, `criterion`, `proptest` | 按需 |

禁止：

- 引入与 TiDB 语义冲突的“智能”ORM/查询库。
- 用 `regex` 解析 SQL（文本预处理除外，且需测试覆盖）。
- 引入 GPL/AGPL 依赖（本项目 Apache-2.0）。

---

## 9. Git / 提交规范

- 除非用户明确要求，**不执行 `git commit`**。
- 提交信息格式：

```text
<area>: <short description>

Upstream: pkg/planner/core/optimizer.go @ v8.5.8
Phase: 04-planner-rbo
Notes: ...
```

- 一个提交只做一件事；不要混合“移植 + 重构 + 格式化”。
- 提交前必须 `git diff` 自查；禁止提交构建产物、密钥、临时文件。
- 大文件/testdata 用 Git LFS 或按需裁剪（先评估体积）。

---

## 10. 文档规范

- 工程文档用中文；代码标识符、类型名、上游路径用英文。
- 每个阶段结束时更新：`progress.md`、`parity-matrix.md`、对应 phase 文档（若有）。
- 任何“为什么这样做”的决策写 ADR，不写在代码注释里长篇解释。
- 注释只解释**非显然**的意图/算法；不要复述代码。

---

## 11. Parity 状态字段

状态存于 `project/parity/status.json`：

| 状态 | 含义 | 允许的占位实现 |
|---|---|---|
| `ported` | 核心逻辑已实现，测试通过 | 无 |
| `partial` | 部分分支/函数已实现 | 未实现分支返回明确错误 |
| `reimplemented` | 因语言/库差异重写，行为对齐 | `notes` 说明差异 |
| `stub` | 仅有接口/骨架，未实现 | `todo!()` 或 `Err(Unsupported)` |
| `omitted` | 本阶段不需要 | 不要求 `.rs`；`notes` 说明原因与阶段 |
| `not-started` | 尚未开始（manifest 缺省） | 允许（由 parity 脚本报告） |

---

## 12. 评审检查清单（Reviewer）

- [ ] `project/parity/status.json` 已登记，状态与实现一致。
- [ ] 源码中没有来源/移植标记（C9）。
- [ ] 算法/分支与上游一致；差异有注释/ADR。
- [ ] 错误路径可测试；无静默 `unwrap`。
- [ ] 并发/取消/超时考虑完整。
- [ ] 测试覆盖关键行为；Oracle 用例已加。
- [ ] `parity-matrix.md` / `progress.md` 已更新。
- [ ] 没有顺手重构无关代码。
