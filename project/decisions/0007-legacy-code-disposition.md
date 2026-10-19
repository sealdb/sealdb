# ADR-0007: 旧 Rust 代码处置

- 状态：**proposed**（需用户确认）
- 日期：2026-10-19
- 关联：requirements Q1；progress D1

## 背景

现有 `src/` 是另一套通用数据库脚手架，不是 TiDB 计算层移植：

- Parser 只按关键字返回固定结构，没有真正解析。
- `TiKVEngine::begin_transaction` 直接返回未实现。
- Planner/Optimizer/Executor 是自研形状，与 TiDB 目录/算法不对应。
- 根 `Cargo.toml` edition 2024 在本机无法解析。
- `tests/` 是通用集成/回归框架，不是 TiDB 行为对齐测试。

用户已明确表示：**“当前的代码可以全部丢弃重新开始，不要让现有代码限制思路。”**

## 决策（建议）

1. **保留非代码基础设施**：
   - `docker/`（但固定镜像版本，替换 `:latest`）
   - `monitoring/`（Prometheus/Grafana 配置）
   - `config/`（重写为与 TiDB 配置对齐）
   - `LICENSE`、`.gitignore`、`.editorconfig`
2. **归档旧 Rust 代码**：
   - 创建分支/tag `legacy-rust-scaffold`（不改历史）。
   - 删除 `src/`、`tests/`（旧）、`Makefile` 中旧目标、旧 README 内容。
   - 如需参考，从 tag 取，不留在主分支。
3. **重建**：
   - 按 `architecture.md` 建 `pkg/`、`cmd/`、`tools/`、`tests/`、`testdata/`。
4. **README 重写**：明确“Rust 重写 TiDB 计算层，复用 PD/TiKV”，删除与现状不符的宣传（如“完整 PostgreSQL RBO”“数千并发”等）。

## 后果

### 正面

- 消除误导，避免有人基于玩具代码扩展。
- 目录/依赖从零按 TiDB 对齐，减少历史包袱。
- 构建可在 1.80 上立即跑通。

### 负面 / 代价

- 丢失少量可复用的 TiKV 连接 demo（可从 tag/历史恢复）。
- 需要重写 CI/Makefile/文档。

## 备选方案

| 方案 | 结论 |
|---|---|
| 保留并在旧代码上改造 | 目录/算法不对应，返工更大；拒绝 |
| 新旧并存（`src/` + `pkg/`） | 混淆、重复依赖；拒绝 |
| 只删 parser/executor，保留 storage | `storage` 也是自研抽象，与 TiDB `pkg/kv` 不兼容；建议一并归档 |

## 执行前检查

- [ ] 用户确认（本 ADR 状态改 accepted）。
- [ ] 打 tag/分支（不 push，除非用户要求）。
- [ ] `git status` 干净；记录当前 commit。
- [ ] 备份 `docker/`、`monitoring/`、`config/` 中要保留的内容。
- [ ] 删除后跑 `cargo metadata` / `cargo check` 空骨架。

## 验证方式

- 主分支不再有旧 `src/` 引用。
- README/文档与目录一致。
- Phase 0 Gate 通过。

## 参考

- `README.md`（待重写）
- `progress.md` D1
