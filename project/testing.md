# 测试与回归（Testing）

> **进度尺 = suite `green`**（TiDB v8.5.8 `tests/integrationtest` 金样）。  
> 包级 `parity_check` / `core-ported` 只是卫生检查，**不能**替代回归绿。  
> 权威语料：`tests/integrationtest/{t,r}` + `project/regression/manifest.json`。  
> **对齐纪律（一比一 × TDD × 生产级）**：[parity-discipline.md](./parity-discipline.md)。

---

## 1. 金样权威（硬约束）

| 来源 | 允许？ |
|---|---|
| `project/scripts/record_tidb_itest.sh`（TiDB `run-tests.sh -r`） | **唯一权威** |
| SealDB `run-tests.sh -r` / `--record` 覆盖 TiDB 语料 | **禁止** |
| `t/local/*` + `SEALDB_ITEST_ALLOW_SEALDB_RECORD=1` | 仅本地实验 |

导入 / 刷新：

```bash
./project/scripts/import_tidb_itest.sh
./project/scripts/record_tidb_itest.sh -t select -r   # needs tidb-server v8.5.8
```

---

## 2. Manifest 与 CI

`project/regression/manifest.json` 为每套件标注 `status` / `wave` / `owner_hint`。

| Status | CI 默认 | 含义 |
|---|---|---|
| `green` | 跑 | Gate A+B 已对 TiDB 金样通过 |
| `local` | 跑 | SealDB 自有 smoke（非 TiDB 金样） |
| `in_progress` | 需显式 | 当前 TDD 波（`--status in_progress`） |
| `pending` | 不跑 | 已导入、未开工 |
| `deferred` | 不跑 | TiFlash / MPP / TiSpark |

```bash
# CI / 日常闸门：只跑 green + local
./tests/integrationtest/run-tests.sh -s memory

# TDD：当前波（期望红）
./project/scripts/triage_itest.sh --wave R1 --status in_progress -s memory
SEALDB_ITEST_RUN_IN_PROGRESS=1 ./tests/integrationtest/run-tests.sh --wave R2
```

Gate A：结果集、`--error` 的 errno/SQLSTATE。  
Gate B：同套件内 `EXPLAIN` 算子族与树形（代价数字可归一化）。

MemStorage 与 TiKV（PD 可用）必须对同一金样；`skip_tikv` 仅用于已知 TiKV 缺口（记在 manifest）。

---

## 3. TDD 日常闭环

1. `triage_itest.sh` → 首条失败 SQL。  
2. 分类：parse / 结果 / errno / EXPLAIN / runner。  
3. 在 TiDB 定位同名包/文件（`Build` / `optRuleList` / `executorBuilder` / builtin）。  
4. 对照 TiDB 同文件调用链与测试，**一比一**补齐算法与错误分支（见 [parity-discipline.md](./parity-discipline.md)）。  
5. 只改 SealDB **同路径同名** `.rs`；禁止另起捷径。  
6. 单元测试 + 该套件 Gate A(+B)；Mem + TiKV 同金样；达生产级清单后升 `green`。  
7. **实时**更新 [progress.md](./progress.md)；无 suite 证据不得标 `ported`。  
8. **禁止**改权威 `r/*.result`。

波次依赖序见 [regression/waves/README.md](./regression/waves/README.md)（R1→R7，D 延后）。

---

## 4. 测试金字塔（辅助）

```text
                 ┌─────────────────────┐
                 │  integrationtest    │  TiDB 金样 · suite green = 进度
                 ├─────────────────────┤
                 │  cargo test         │  包内单元 / MVB
                 ├─────────────────────┤
                 │  parity_check.py    │  同名文件覆盖雷达（非进度）
                 └─────────────────────┘
```

历史 Oracle harness（`tools/oracle/`、`testdata/`）仍可用于专项差分；**生产对齐进度以 manifest suite 为准**。

---

## 5. 相关脚本

| 脚本 | 作用 |
|---|---|
| `project/scripts/import_tidb_itest.sh` | 同步 TiDB `t/r/s` + 生成 manifest |
| `project/scripts/record_tidb_itest.sh` | TiDB 录制/校验金样 |
| `project/scripts/triage_itest.sh` | 跑波/套件并落首失败 |
| `tests/integrationtest/run-tests.sh` | 本地/CI runner |

细节见 [tests/integrationtest/README.md](../tests/integrationtest/README.md)。
