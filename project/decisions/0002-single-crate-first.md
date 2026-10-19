# ADR-0002: 单 crate 优先，保持 TiDB 目录对齐

- 状态：**accepted**
- 日期：2026-10-19
- 关联：requirements Q2；architecture §2

## 背景

TiDB 是 Go 多包工程（`pkg/parser`、`pkg/planner`、`pkg/executor` ...）。Rust 有两种组织方式：

1. **单 crate + 模块树**：`pkg/lib.rs` 声明 `pub mod parser;` 等，目录与 TiDB 一一对应。
2. **多 crate workspace**：每个顶层包一个 crate，各自 `Cargo.toml`。

要求是“目录名、文件名与 TiDB 完全一致”。多 crate 会在每个目录增加 `Cargo.toml`，并引入跨 crate 可见性/依赖声明成本；单 crate 只需要根 `Cargo.toml` 和每目录 `mod.rs`。

## 决策

**Phase 0–6 采用单 crate**：

- 根 `Cargo.toml`：`[lib] path = "pkg/lib.rs"`；`[[bin]] path = "cmd/tidb-server/main.rs"`。
- `pkg/` 下按 TiDB 目录建模块；每个目录一个 `mod.rs`。
- 允许的额外文件：`Cargo.toml`、`Cargo.lock`、`rust-toolchain.toml`、`mod.rs`、`lib.rs`。
- 桥接代码放 `pkg/parser/sqlparser_bridge/`（显式允许）。

**触发拆分条件**（任一满足则评估 workspace）：

- `cargo check` 增量编译 > 2–3 分钟且影响日常开发；
- 需要独立发布/复用某个子模块（如 parser/types）；
- 模块边界频繁被违反，需要编译期强制。

## 后果

### 正面

- 目录最接近 TiDB；新增文件只需 `.rs` + 在 `mod.rs` 注册。
- 跨模块引用简单，移植初期速度快。
- 避免 20+ 个 `Cargo.toml` 的维护成本。

### 负面 / 代价

- Rust 编译器不强制包边界，可能出现反向依赖（parser 依赖 executor）。
- 单编译单元变大后，编译时间上升。
- 并行编译收益较低。

### 缓解

- 在 `architecture.md` 明确依赖方向；Reviewer 检查。
- 用 `pub(crate)` 控制可见性；禁止 `pub use` 泄漏内部类型。
- 定期跑 `cargo tree`/模块依赖脚本；发现反向依赖立即修。

## 备选方案

| 方案 | 结论 |
|---|---|
| 一开始就多 crate | 过度设计，增加初期摩擦；暂不采用 |
| 完全按 Go 包拆 crate | 包数量过多（40+），Cargo.toml 泛滥；拒绝 |
| 单文件巨型 crate | 不可维护；拒绝 |

## 验证方式

- Phase 0 空骨架 `cargo check` 时间记录。
- 每次 Phase Gate 记录编译时间；超过阈值触发拆分评估。
- 依赖方向审计脚本/Review checklist。

## 参考

- `architecture.md` §2
- `conventions.md` §4
