# ADR-0002: Workspace 按 TiDB 顶层包粒度拆分

- 状态：**accepted**
- 日期：2026-10-19
- 关联：requirements Q2；architecture §2

## 背景

需要同时满足两个目标：

1. 目录名、文件名与 TiDB 保持对齐；
2. 工程粒度适合长期开发，避免单编译单元过大。

可选方案：

- 单 crate + `pkg/` 模块树；
- 每个 TiDB 顶层包一个 crate（`pkg/parser`、`pkg/planner` ...）；
- 按层拆成少数 crate（parser/types/kv/planner/executor/...），但会破坏顶层目录一一对应。

## 决策

采用 **Cargo workspace，每个 TiDB 顶层包一个 crate**：

- `pkg/<name>/Cargo.toml` + `pkg/<name>/lib.rs`。
- crate 名称统一为 `sealdb-<name>`（构建元数据，不写来源说明）。
- TiDB 的子包（如 `pkg/parser/ast`、`pkg/planner/core`）作为该 crate 内的 Rust 模块，目录不变。
- `cmd/tidb-server` 作为独立 bin crate。
- Phase 0 先建立全部顶层 crate 的空骨架；依赖按阶段逐步加入，避免早期循环依赖。

当前 workspace 共 **25 个 package**（24 个 `pkg/*` + 1 个 `cmd/*`）。

## 后果

### 正面

- 目录与 TiDB 顶层包一一对应，parity 检查直观。
- 编译可并行；单个 crate 改动不会触发全量重编。
- 编译期可强制包边界，减少反向依赖。

### 负面 / 代价

- 每个顶层包多一个 `Cargo.toml`（允许的构建文件）。
- 跨 crate 类型需要显式依赖声明；初期样板代码较多。
- 如果出现无法绕过的循环依赖，需要拆出接口 crate 或调整边界。

### 触发再评估的条件

- 编译时间仍然过长；
- 出现跨 crate 循环依赖；
- 某个 crate 过大或职责混杂。

出现上述情况时单独讨论，不在实现中临时打破边界。

## 备选方案

| 方案 | 结论 |
|---|---|
| 单 crate | 目录最干净，但编译单元大、边界弱；未采用 |
| 每个 TiDB 子包一个 crate | crate 数量过多，Cargo.toml 泛滥；未采用 |
| 按架构层合并 crate | 会破坏 `pkg/<name>` 目录对应；未采用 |

## 验证方式

```bash
cargo metadata --no-deps --format-version 1
cargo check --workspace
```

Phase 0 已通过（25 个 package）。

## 参考

- `Cargo.toml`
- `architecture.md` §2
- `conventions.md` §4
