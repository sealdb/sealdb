# ADR-0007: 旧 Rust 代码处置

- 状态：**accepted**
- 日期：2026-10-19
- 关联：requirements Q1

## 背景

旧 `src/` 是另一套通用数据库脚手架，不是计算层移植：

- Parser 只按关键字返回固定结构；
- 存储事务路径未实现；
- Planner/Executor 形状与参考实现不对应；
- 根 manifest 的 edition 与当时工具链不匹配。

用户确认：旧代码可以整体丢弃，不要让现有代码限制思路；处置方式为“挪到新目录，目录名体现其过时”。

## 决策

1. 新建 `legacy-rust-scaffold/`，移入：
   - `src/`
   - `tests/`
   - `docs/`
   - 旧 `Cargo.toml`
   - 旧 `Makefile`
2. 保留并继续使用：
   - `docker/`、`monitoring/`、`config/`（后续按需重写/固定版本）
   - `LICENSE`、`.gitignore`
3. 新实现放在仓库根：
   - `pkg/`（workspace crates）
   - `cmd/tidb-server`
   - `tools/oracle`
   - `tests/`（后续新建）
4. `legacy-rust-scaffold/` 不加入 workspace，不参与构建；只作历史参考。
5. 重写根 `README.md`，不再宣传旧实现能力。

## 后果

### 正面

- 消除误导，避免基于玩具代码扩展。
- 目录/依赖从零按目标架构建立。
- 构建在新工具链上可立即通过。

### 负面 / 代价

- 旧代码不再被 CI 覆盖（符合预期）。
- 少量 TiKV 连接实验只能从 git 历史或归档目录查看。

## 备选方案

| 方案 | 结论 |
|---|---|
| 保留并改造 | 目录/算法不对应，返工更大；未采用 |
| 新旧并存 | 混淆、重复依赖；未采用 |
| 只保留 storage | 与目标 `pkg/kv` 抽象不兼容；未采用 |

## 验证方式

```bash
test ! -e src
test -d legacy-rust-scaffold
cargo metadata --no-deps --format-version 1
cargo check --workspace
```

## 参考

- `legacy-rust-scaffold/README.md`
- 根 `README.md`
