# ADR-0006: Edition 2021 / MSRV 1.80.1

- 状态：**accepted**
- 日期：2026-10-19
- 关联：risks R8；architecture §2

## 背景

当前仓库根 `Cargo.toml`：

```toml
[workspace]
resolver = "3"
members = [ ... ]   # 多个成员使用 edition = "2024"
```

本机工具链：

```text
rustc 1.80.1
cargo 1.80.1
```

实际验证：

```text
error: failed to parse manifest at .../Cargo.toml
Caused by: feature `edition2024` is required
```

即：**当前代码在本机连 `cargo metadata` 都无法执行**。

Rust edition 2024 需要 Cargo ≥ 1.85。当前环境未安装更新的 stable；升级工具链可能受网络/环境限制。

## 决策

Phase 0–6 采用：

- **Edition 2021**
- **MSRV 1.80.1**（当前环境）
- 根 `Cargo.toml` 不使用 `resolver = "3"`（edition 2021 默认 resolver 2）
- 新增 `rust-toolchain.toml` 固定 `channel = "1.80.1"`（或团队约定的 stable），避免“本机能跑、别人不能跑”

### 升级路径

当满足以下条件时，另开 ADR 升级到 edition 2024 / 新 MSRV：

1. 团队统一安装 Rust ≥ 1.85；
2. CI 镜像包含新工具链；
3. 完成一次全量 `cargo test` + Oracle 回归；
4. 评估新 edition 的破坏性变更（如 `unsafe`/宏/生命周期语义）。

## 后果

### 正面

- 立即可构建；不阻塞 Phase 0。
- 语法保守，降低移植时的额外变量。

### 负面 / 代价

- 无法使用 edition 2024 的新特性（如部分 `gen`/捕获规则改进）。
- 部分新 crate 可能要求更高 MSRV；引入时需检查。

## 备选方案

| 方案 | 结论 |
|---|---|
| 立即升级到 1.85+ | 受环境/网络限制；暂不采用 |
| 各 crate 混用 edition | 增加复杂度，拒绝 |
| 不固定工具链 | 不可复现，拒绝 |

## 验证方式

```bash
rustc --version           # 1.80.1
cargo metadata --no-deps  # 成功
cargo check               # 成功
```

Phase 0 Gate 必须包含以上命令。

## 参考

- 根 `Cargo.toml`（待重写）
- `project/progress.md` D2
