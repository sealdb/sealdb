# 对齐纪律：一比一复刻 × TDD × 生产级成熟度

> 协作公约。与 [testing.md](./testing.md)、[conventions.md](./conventions.md)、[progress.md](./progress.md)、[regression/waves/README.md](./regression/waves/README.md) 配套使用。  
> **进度事实来源**：`project/regression/manifest.json` 的 suite `green`。

---

## 1. 目标

SealDB = Rust 复刻 **TiDB v8.5.8 计算层**。每个子功能交付时必须同时满足：

1. **行为**：对同一金样与 TiDB 一致（Gate A 结果/errno；Gate B EXPLAIN 树形）。  
2. **形态**：同路径同名包/文件一比一移植算法，禁止“能过测”捷径。  
3. **成熟度**：该功能在 MemStorage 与 TiKV（PD 可用）上稳定可重复，达生产可用，而非 demo。

---

## 2. 一比一复刻（怎么改代码）

| 要求 | 做法 |
|---|---|
| 入口映射 | 失败 SQL → TiDB `Build` / `optRuleList` / `executorBuilder` / builtin → SealDB **同路径同名** `.rs` |
| 算法同构 | 控制流、错误分支、边界与 TiDB 对齐；语言差异仅限已登记偏差（如 sqlparser 桥接） |
| 完备对照 | 移植前对比 TiDB 同文件的调用链与测试覆盖；缺分支必须补齐或在 progress 记缺口与退出条件 |
| 禁止 | 改权威 `r/*.result`；另起平行实现“凑绿”；无 suite 证据把 parity 标 `ported` |
| 暂缓 | TiFlash / MPP / TiSpark → manifest `deferred`（不丢弃） |
| 录制环境 | TiDB 的 mysql-tester **所有 case 共用一条连接**，某些 `SET` 会泄漏到后续文件并写进金样。此类套件在 manifest 用 `session_init`（`SET` 列表）声明泄漏变量，runner 在套件首条语句前静默执行；不得改 `t/*.test` 或金样。 |

`parity_check.py` / 包级 `core-ported` **只做覆盖雷达**，不能替代 suite green。

---

## 3. TDD 驱动（怎么推进）

```text
triage 首败 → 分类(parse/结果/errno/EXPLAIN/runner)
  → 定位 TiDB 同名文件 → 一比一移植 → 单测 + 该套件
  → Gate A(+B) → 升 green → 更新 progress.md
```

```bash
./project/scripts/triage_itest.sh -t select --status in_progress -s memory
# 金样只来自 TiDB：
./project/scripts/record_tidb_itest.sh -t <suite> -r
```

波次依赖序：R1 → R2 → R3 → … → R7；D 延后。同一波内按失败用例自然序，不跳绿。

---

## 4. 生产级成熟度（何时算“完成一个功能”）

宣称某子功能完成前，清单必须勾满：

- [ ] 相关 suite（或明确子集）Gate A 绿；有 EXPLAIN 的同套件 Gate B 绿  
- [ ] MemStorage 与 TiKV 对**同一**金样通过（或 manifest `skip_tikv` + 书面原因）  
- [ ] 错误码/SQLSTATE/消息与 TiDB 金样一致（允许归一化的非语义差异写在 PARITY_NOTES）  
- [ ] 同名文件关键路径无 `todo!` / 静默吞错；并发与事务语义不弱于 TiDB 该路径  
- [ ] `progress.md` 当日条目写清：改了哪些同名文件、证据命令、下一缺口  
- [ ] 无 suite 证据不得把 `parity/status.json` 标为 `ported`

未达以上 = 仍 `in_progress`，不算交付。

---

## 5. 协作约定

| 动作 | 责任 |
|---|---|
| 开工套件 | manifest 标 `in_progress`；progress 写负责人/下一步 |
| 每完成一块 | **实时**更新 `progress.md` §2 / §4；必要时更新 waves README |
| 升 `green` | 附 Mem + TiKV 命令输出或 CI 链接 |
| Commit | 禁止 `Co-authored-by` trailer；消息说明 why |
| 冲突 | 以 TiDB v8.5.8 源码 + 金样为准，不以“本地自录”为准 |

---

## 6. 相关入口

- 金样与 runner：[testing.md](./testing.md)  
- 移植文件规范：[conventions.md](./conventions.md)  
- 波次与命令：[regression/waves/README.md](./regression/waves/README.md)  
- 进度看板：[progress.md](./progress.md)  
- TiDB 参考树：本地 `/Users/wslu/work/github/db/pingcap/tidb`（v8.5.8）
