# 整体进展报告（Status Report）

> 生成时间：2026-11-07 19:10 (+08:00)
> 事实来源：`project/regression/manifest.json`（suite 状态）、`project/parity/status.json`（包级对齐）、`git log`
> 规范：[testing.md](./testing.md) · [parity-discipline.md](./parity-discipline.md) · [progress.md](./progress.md)

---

## 1. 一句话结论

TiDB v8.5.8 集成测试语料已导入 **254** 个 suite，其中 **25 个** 做到 1:1 金样对齐并在**内存引擎**上全绿（今日复验 `PASS: memory=25`）；R1 已收口，R2 完成 15/22，R3 完成 6/48 并正在主攻分区与 EXPLAIN 形状；架构侧包级对齐登记 178 个 TiDB 文件中 **150 partial / 21 stub / 4 reimplemented / 3 ported**，仍以「先计算层语义与执行计划形状 1:1，再补远端 Coprocessor/存储层」为主线。

---

## 2. 度量口径

- **唯一进度单位 = manifest 中 `green` 的 suite**（`r/*.result` 为 TiDB 权威金样，禁止手改）。
- `green` 必须 Gate A（结果/错误码/警告）+ Gate B（EXPLAIN 形状/estRows/task/access object）双通过，且 Mem + TiKV 双引擎复验。
- 功能波 `core-ported`、包级 parity 不计入本报告第 3 节的进度，只作为架构债务参考。

### 2.1 manifest 总览（254 suites）

| 状态 | 数量 | 占比 |
|---|---:|---:|
| `green` | 25 | 9.8% |
| `in_progress` | 29 | 11.4% |
| `pending` | 190 | 74.8% |
| `local`（SealDB 冒烟） | 8 | 3.1% |
| `deferred`（TiFlash/MPP） | 2 | 0.8% |

### 2.2 按波次

| Wave | green | in_progress | pending | 说明 |
|---|---:|---:|---:|---|
| R1（select / show / types） | 4 | 0 | 0 | **已收口** |
| R2（join / subquery / agg / window / cte） | 15 | 6 | 1 | 主波完成 ~68% |
| R3（point_get / index / partition / explain*） | 6 | 23 | 19 | 当前主攻波 |
| R4（plan cache / 会话） | 0 | 0 | 13 | 未开工 |
| R5 | 0 | 0 | 12 | 未开工 |
| R6 | 0 | 0 | 35 | 未开工 |
| R7 | 0 | 0 | 110 | 未开工 |
| D（TiFlash/MPP） | 0 | 0 | 2 | deferred |
| local | — | — | — | 8 套冒烟 |

---

## 3. 已绿 suite 清单（25）

| Wave | Suite |
|---|---|
| R1 | `select`, `show`, `types/const`, `types/time` |
| R2 | `agg_predicate_pushdown`, `executor/cte`, `executor/jointest/hash_join`, `executor/jointest/join`, `executor/union_scan`, `executor/window`, `explain_union_scan`, `globalindex/aggregate`, `globalindex/index_join`, `index_join`, `naaj`, `null_rejected`, `subquery`, `topn_pushdown`, `window_function` |
| R3 | `executor/partition/issues`, `executor/partition/partition_boundaries`, `executor/partition/partition_with_expression`, `executor/partition/table`, `explain-non-select-stmt`, `globalindex/point_get` |

**复验证据**

```text
./tests/integrationtest/run-tests.sh -s memory --status green
→ PASS: memory=25 tikv=0 skipped=229
```

TiKV 侧：本地 PD/TiKV 曾因沙箱网络限制无法连通（`Operation not permitted` 连 127.0.0.1:2379），已用完整权限重启集群（PD v8.5.0，store `127.0.0.1:20160` state=Up），25 套件 TiKV 复验随本轮 gate 一起跑（`PASS: memory=0 tikv=25 skipped=229`）。

---

## 4. 近期落地能力（自 2026-11-06 起，按提交）

| 提交 | 能力 |
|---|---|
| `14408dc` | GLOBAL 索引：解析、DDL、聚合计划（`globalindex/aggregate`）、EXPLAIN 渲染 |
| `b08c3d1` | GLOBAL 索引等值连接的 IndexJoin（inner side 的 `TableReader` 形态对齐） |
| `be7ea22` | GLOBAL 索引 Point_Get（`globalindex/point_get` 转绿） |
| `642209b` | DML 子计划（UPDATE/DELETE/INSERT 的 EXPLAIN 子节点）+ TiDB 投影列编号规则 |
| `0b42f12` | 引号内 `EXPLAIN` 关键字不再被重写（parser 保真） |
| `15c14aa` | `tidb_window_concurrency` 恢复为 session 级作用域 |
| `e8214ec` | 分区边界裁剪 parity：`BETWEEN` 下沉、`IN` 折叠、`HIGH_NOT_PRECEDENCE` 绑定、无分区命中 → `partition:dual`、value-count 选择性 |
| `725bab2` | 表达式分区键、字符串/数值比较强转、HASH 分区范围裁剪（`partition_with_expression` 转绿） |
| `fb42838` | `DIV` 运算符、`INVISIBLE` 索引选项、表达式分区键的 `IN` 裁剪 |
| `9d0ac09` | `SET GLOBAL tidb_partition_prune_mode`（含 `DEFAULT`）与 mysql-tester session 指令 |
| `3f4dfb5` | `ALTER TABLE ... EXCHANGE PARTITION ... WITH TABLE` 语法（AST + lower） |
| `e8391b7` | EXCHANGE PARTITION 交换分区/表 ID 的元数据（`update_table_with_new_id`） |
| `e712298` | 查询引用 `_tidb_rowid` 时的隐藏列投影（TableReader / IndexLookUp） |
| `d95740a` | 唯一键 LEFT JOIN 上的 `count(*)` 走覆盖索引单表计数 |
| `92e64dd` | 分区表跳过同步 auto-analyze（对齐 TiDB 异步分区统计） |

`executor/partition/issues` 相关的分区能力（已随本轮提交落地）：

- `INSERT ... PARTITION (p0,p1)` 目标分区过滤 + `INSERT IGNORE` 下 warning `1748` / 去重 `1062`（`pkg/executor/insert.rs`、`errcode/errname`）。
- 无符号 BIGINT 分区键：`Datum::U64` 按有符号位模式哈希；`hash_conjunction_range` 用 `i128` 承接 `i64::MAX+1` 边界值（`pkg/meta/model/partition.rs`）。
- 聚簇整数主键不走二级索引路径：`find_index_covering` / `find_unique_index_on` 排除 `pk_is_handle`；索引键写入/删除跳过聚簇主键（`pkg/meta/model/table.rs`、`pkg/executor/index.rs`）。
- 聚簇主键范围扫描：`TableReader.range_scan` + `TableRangeScan` EXPLAIN 渲染与 `keep order:false`（`physical_plans.rs` / `explain.rs` / `planbuilder.rs`）。
- HAVING 下推：无聚合函数时不再注入 `cast + count` 投影（`rule_having_push_down.rs`）。

---

## 5. 刚收口的 R3 套件

| Suite | 收口要点 |
|---|---|
| `executor/partition/issues` | 自连接派生谓词、空表 ANALYZE 仍为 pseudo、整域 `BETWEEN` 折叠为裸 `TableFullScan`、`inl_join(c)` 的 IndexJoin 形状、`desc` 的 `_N` 编号、`partition:p2` 裁剪 |
| `executor/partition/table` | `EXCHANGE PARTITION ... WITH TABLE` 语法与元数据换 ID、`_tidb_rowid` 隐藏列投影、唯一键 LEFT JOIN 的 `count(*)` 覆盖索引简化、分区表异步 auto-analyze、`SET GLOBAL tidb_partition_prune_mode` |
| `executor/partition/partition_boundaries` | `BETWEEN`/`IN` 下沉与折叠、`NOT` 绑定、`partition:dual`、value-counts 选择性 |
| `executor/partition/partition_with_expression` | 表达式分区键、字符串/数值强转、HASH 分区范围裁剪 |

下一个分区目标是 `executor/partition/write`（INSERT/UPDATE/DELETE 的分区写入路径与 EXPLAIN 子计划）。

---

## 6. R2 剩余与 R3 队列

- **R2 在研（6）**：`cte`, `explain_cte`, `topn_push_down`, `planner/core/join_reorder2`, `planner/core/join_reorder_through_projection`, `planner/core/casetest/rule/rule_derive_topn_from_window`
- **R3 在研（23）**：`access_path_selection`, `clustered_index`, `executor/batch_point_get`, `executor/explain`, `executor/index_merge_reader`, `executor/partition/write`, `executor/point_get`, `explain`, `explain_complex`, `explain_complex_stats`, `explain_easy`, `explain_easy_stats`, `explain_foreign_key`, `explain_generate_column_substitute`, `explain_indexmerge_stats`, `explain_join_stats`, `explain_shard_index`, `explain_stats`, `globalindex/mem_index_merge`, `globalindex/mem_index_merge_non_unique`, `index_merge`, `planner/core/point_get_plan`, `planner/core/casetest/point_get_plan`

策略：优先吃「同一根因能一次带绿多套」的项——EXPLAIN 节点编号/树形、IndexJoin hint、Point_Get/IndexMerge 统计形状。

---

## 7. 质量与纪律

- **TDD 1:1**：先跑红 → 在 TiDB 同包同文件定位行为 → SealDB 同路径实现 → Gate A/B 双过 → 再提交。
- **禁止修改金样**：`tests/integrationtest/r/**/*.result` 只能由 TiDB `record_tidb_itest.sh` 生成。
- **提交粒度**：一个可独立验证的子能力一个 commit，提交信息不含 `Co-authored-by`。
- **双引擎**：升 `green` 前必须 Mem + TiKV 复验（TiKV 不可用时不允许升绿，只能保留 `in_progress`）。

---

## 8. 风险 / 技术债

| ID | 内容 | 影响 | 处置 |
|---|---|---|---|
| S1 | 本地 PD/TiKV 会被沙箱网络限制打死（`Operation not permitted`） | TiKV Gate B 无法复验 | 启动集群时使用完整权限；状态用 `run_local_pd_tikv.sh status` 先探活 |
| S2 | EXPLAIN 节点编号（`_N`）目前与 TiDB 的分配顺序不一致 | 所有 `desc/explain` 段 diff 的共同根因之一 | 按 TiDB `planID` 分配顺序重排 walk |
| S3 | 优化器 hint（`inl_join` 等）尚未生效 | R3/R6 多套件被阻塞 | 先做 IndexJoin 物理算子与 hint 解析的最小闭环 |
| S4 | 远端 Coprocessor DAG 未落地（谓词在 scan 侧过滤） | 真集群下推路径不一致 | 后续完整性波 |
| S5 | `local/agg` 冒烟金样因投影列名去反引号漂移而红 | 本地冒烟信号失真 | 待确认后由 `record` 流程重录（非 TiDB 语料） |
| S6 | 190 套 pending 体量大 | 进度曲线前慢后快 | 按波次批量吃同根因 |

---

## 9. 下一步（有序队列）

1. `executor/partition/write`：EXCHANGE PARTITION / 分区写入路径对齐后升 `green`。
2. `executor/partition/{batch_point_get,point_get,index_merge_reader}`。
3. `explain_*` 家族与 `point_get` 家族（EXPLAIN 编号与统计形状一次带绿多套）。
4. 回到 R2 剩余 `cte` / `explain_cte` / `topn_push_down` / `join_reorder*`。
5. 按波次推进 R4–R7；TiFlash/MPP 维持 deferred。

---

## 10. 附：代码规模

| 项 | 数值 |
|---|---:|
| Rust 源文件（pkg/cmd/tests/tools） | 258 |
| Rust 代码行 | ~80,957 |
| 集成测试输入 `t/**/*.test` | 255 |
| 权威金样 `r/**/*.result` | 258 |
| 包级对齐登记文件 | 178 |
