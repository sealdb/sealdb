# ADR-0007: 旧 Rust 代码处置

- 状态：**accepted**（后续已删除归档目录）
- 日期：2026-10-19
- 关联：requirements Q1

## 背景

旧 `src/` 是另一套通用数据库脚手架，不是计算层移植：

- Parser 只按关键字返回固定结构；
- 存储事务路径未实现；
- Planner/Executor 形状与参考实现不对应；
- 根 manifest 的 edition 与当时工具链不匹配。

用户确认：旧代码可以整体丢弃，不要让现有代码限制思路；最初处置为“挪到新目录，目录名体现其过时”，随后在仓库中删除该归档。

## 决策

1. 曾将旧 `src/`、`tests/`、`docs/`、旧 `Cargo.toml`、旧 `Makefile` 移入 `legacy-rust-scaffold/`，且不加入 workspace、不参与构建。
2. 保留并继续使用：`docker/`、`monitoring/`、`config/`、`LICENSE`、`.gitignore`。
3. 新实现放在仓库根：`pkg/`、`cmd/tidb-server`、`tools/`、`tests/`。
4. **现行状态**：`legacy-rust-scaffold/` 已从仓库删除；如需查看旧脚手架，以 git 历史为准。
5. 根 `README.md` 不再宣传旧实现能力。

## 后果

### 正面

- 消除误导，避免基于玩具代码扩展。
- 目录/依赖从零按目标架构建立。
- 构建在新工具链上可立即通过。
- 删除归档后工作树更干净。

### 负面 / 代价

- 旧代码仅能从 git 历史查看。

## 备选方案

| 方案 | 结论 |
|---|---|
| 保留并改造 | 目录/算法不对应，返工更大；未采用 |
| 新旧并存 | 混淆、重复依赖；未采用 |
| 只保留 storage | 与目标 `pkg/kv` 抽象不兼容；未采用 |
| 长期保留归档目录 | 已改为删除 |

## 验证方式

```bash
test ! -e src
test ! -e legacy-rust-scaffold
cargo metadata --no-deps --format-version 1
cargo check --workspace
```

## 参考

- 根 `README.md`
