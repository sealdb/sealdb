---
name: phase-gate
description: 按 roadmap 的阶段 Gate 清单执行验收，收集证据、生成报告、更新进度，并判断是否允许进入下一阶段。适用于“Phase X 是否可以收尾/开工下一阶段”。
---

# Skill: 阶段 Gate 验收

## 适用

- 阶段开发完成，准备收尾；
- 进入下一阶段前的强制检查；
- 用户询问“现在到哪了/能不能开始下一阶段”。

## 输入

- 阶段编号（如 `02`）；
- `project/roadmap.md` 中该阶段的 Gate 清单；
- 当前 `project/progress.md`。

## 步骤

### 1. 冻结范围与语料

- 确认本阶段 `--require` 的包列表。
- 确认语料分母（`corpus.lock.json`）已冻结。
- 确认没有“临时放宽”的测试/归一化规则。

### 2. 运行自动检查

```bash
# 基础质量
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# Parity
python3 project/scripts/parity_check.py \
  --tidb-root /home/wslu/work/github/db/pingcap/tidb \
  --rust-root . \
  --require <本阶段包> \
  --strict \
  --json > project/reports/phase-XX/parity.json

# 一致性
./project/scripts/oracle_diff.sh --suite <本阶段 suite> \
  --out project/reports/phase-XX/oracle.json
```

### 3. 人工检查

- [ ] Gate 清单逐项有证据（命令/报告/链接）。
- [ ] `invalid=0`；`missing-dir` 有解释。
- [ ] `stub/omitted` 均已登记且不阻塞主链路。
- [ ] 已知偏差已写入 `parity-matrix.md`。
- [ ] 下一阶段依赖项已满足。

### 4. 生成报告

`project/reports/phase-XX/summary.md`：

```markdown
# Phase XX Gate Report

- 日期：
- 提交：
- 环境（Rust/PD/TiKV/TiDB）：
- 语料版本/数量：

## 结果
| 检查 | 命令 | 结果 | 证据 |
|---|---|---|---|

## 通过率
- Parser: x/y (z%)
- Plan: ...
- Result: ...

## 已知偏差
- ...

## 结论
- [ ] PASS / [ ] FAIL
- 阻塞项：
- 下一阶段建议：
```

### 5. 更新进度

- `progress.md`：阶段状态改 `done` 或 `blocked`；写日期/证据。
- `parity-matrix.md`：更新状态。
- 若 FAIL：写清阻塞项与解除条件，**不得**进入下一阶段。

## 判定规则

| 情况 | 结论 |
|---|---|
| 所有 Gate 项通过 + 无未登记偏差 | PASS |
| 有失败项但属于已知/后置且有 ADR | 可 PASS（需用户确认） |
| 有静默 stub / 未登记偏差 / 语料被稀释 | FAIL |
| 构建/lint/测试失败 | FAIL |

## 禁止

- ❌ 先宣布完成再补证据。
- ❌ 用“大部分通过”替代 Gate 清单。
- ❌ 把失败项挪到下一阶段而不记录。
- ❌ 修改语料分母让通过率变高。

## 完成标准

- [ ] 报告归档。
- [ ] 进度/矩阵更新。
- [ ] 用户可见的结论（PASS/FAIL + 理由）。
