# 功能清单（Feature Inventory）

> 日常 backlog 的**唯一功能事实来源**。文件级对齐仍看 [parity-matrix.md](./parity-matrix.md) 与 [parity/status.json](./parity/status.json)。  
> 机器可读状态：[features/status.json](./features/status.json)。
>
> **功能是迭代单位，不是目录单位。** F01/F02 只决定先写哪条 SQL 能力；crate、目录、文件名继续跟 TiDB `pkg/<name>/<file>.go` 对齐。不要为功能新建 `pkg/f01/` 或改名。上游命名若和 Rust 硬约束冲突，先登记偏差再调整。

条目从 TiDB v8.5.8 的入口导出，不凭记忆发明：

- `pkg/planner/core/planbuilder.go` `Build`
- `pkg/planner/core/optimizer.go` `optRuleList`
- `pkg/executor/builder.go` `build*`
- `pkg/expression` `GetBuiltinList()`
- `pkg/ddl` job type
- `tests/integrationtest/t/*.test`

## 状态

| 状态 | 含义 |
|---|---|
| `not-started` | 尚未开工 |
| `in-progress` | 本轮正在做 |
| `core-ported` | 核心路径通过冻结语料；边角记入 `gaps[]` |
| `ported` | 完整性波收口，leftover 达标 |
| `deferred` | 明确不做或后置（Phase 10） |

## Unlock（硬门）

| ID | 能力 | 出门标准 |
|---|---|---|
| U0 | 工作区可编译、依赖、CI、`parity --strict`、TiKV spike 书面结论 | `cargo check/test/fmt/clippy` + CI 绿 |
| U1 | types / codec / tablecodec / errno / chunk 核心 | 整数/无符号/浮点/字节编码向量一致；row/index key 布局一致 |
| U2 | sqlparser_bridge + TiDB 形 AST + `--parse --ast-json` | 与 Go oracle 对 `SELECT 1` 同形 |
| U3 | Session `ExecuteStmt` + Dual/Projection | `sealdb-server --execute "SELECT 1"` 输出 `1` |

## 功能波

| ID | 功能 | 依赖 | Phase |
|---|---|---|---|
| F01 | 常量/算术/比较/逻辑/`CAST`/`IF` | U3 | 2 |
| F02 | `CREATE/DROP DATABASE\|TABLE` | F01 | 3 |
| F03 | `INSERT` + TableReader / `SELECT *` | F02 | 4/5 |
| F04 | `WHERE` / Selection | F03 | 4/5 |
| F05 | `ORDER BY` / `LIMIT` / TopN | F04 | 4/5 |
| F06 | PointGet / BatchPointGet | F03 | 5 |
| F07 | `UPDATE` / `DELETE` | F03 | 5 |
| F08 | `COUNT/SUM/AVG/MIN/MAX` + `GROUP BY` | F04 | 5 |
| F09 | 两表 Inner/Outer HashJoin | F04 | 5 |
| F10 | 二级索引 + IndexLookUp | F06 | 5 |
| F11 | `SHOW` / `DESC` / `information_schema` | F02 | 3 |
| F12 | `BEGIN/COMMIT/ROLLBACK` + 乐观 RR | F03 | 6 |
| F13 | `SELECT ... FOR UPDATE` + 悲观 RR | F12 | 6 |
| F14 | 子查询 / Apply / `EXISTS` / `IN` | F09 | 8 |
| F15 | `UNION` / `UNION ALL` | F08 | 8 |
| F16 | Window | F08 | 8 |
| F17 | CTE | F14 | 8 |
| F18 | 在线 DDL：`ALTER` / `CREATE\|DROP INDEX` | F02 | 8 |
| F19 | 分区裁剪（range/hash 基础） | F10 | 8 |
| F20 | `GRANT/REVOKE`（可先 SkipGrant） | F11 | 8 |
| F21 | `ANALYZE` + 真实统计 / CBO | F08 | 7 |
| F22 | MySQL 协议 `COM_QUERY` | F03 | 9 |

RBO 规则跟着功能走：F04 带 PPD/ColumnPrune，F09 带 JoinReorder 基础 DP，F10 带索引路径枚举。`optRuleList` 顺序与上游一致，未实现规则保持 no-op。

## 完整性波

| ID | 工作 |
|---|---|
| C1 | 把每个功能的 `gaps[]` 与 TiDB leftover testdata 收成新分母 |
| C2 | 计划形状 / 代价 / Skyline / JoinReorder 精细对齐 |
| C3 | 事务隔离边角、SchemaChecker、交叉读写 |
| C4 | Prepared / Plan Cache / Binding |
| C5 | 错误码、SQL Mode、charset/collation 剩余项 |
| C6 | 明确不做：Cascades、TiFlash/MPP、BR/Lightning、插件 |

## 单个功能出门标准（`core-ported`）

1. 上游关键文件已读，并在 `project/parity/status.json` 登记。
2. 冻结语料：`project/reports/features/<ID>/corpus.lock.json`（8–30 条，来自 TiDB testdata/`*_test.go`）。
3. 能测到的层都过（parse / plan / result / 交叉读写）。
4. 旧功能 corpus 不回退。
5. 不支持语法返回明确错误。
6. 边角写入 `project/reports/features/<ID>/gaps.md`，不在本轮打磨。

## 命令

```bash
./project/scripts/feature_gate.sh F01
./project/scripts/leftover_inventory.py --tidb-root "$TIDB_ROOT" --out project/features/leftover.md --json project/features/leftover.json --check
```
