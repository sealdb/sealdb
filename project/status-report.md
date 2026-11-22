# 整体进展报告（Status Report）

> 生成时间：2026-11-22 15:30 (+08:00)
> 事实来源：`project/regression/manifest.json`（suite 状态）、`project/parity/status.json`（包级对齐）、`git log`
> 规范：[testing.md](./testing.md) · [parity-discipline.md](./parity-discipline.md) · [progress.md](./progress.md)

---

## 1. 一句话结论

TiDB v8.5.8 集成测试语料已导入 **254** 个 suite，其中 **126 个** 登记为 `green`（R1 4 / R2 16 / R3 31 / R5 4 / R6 19 / R7 52）。本轮新增 `ddl/db_partition` 转绿：LIST/LIST COLUMNS 字面量与日期归一、RANGE COLUMNS 尾列类型强制、1526/1654 错误码分流、RANGE REORGANIZE PARTITION 行迁移与 `_tidb_rowid` 冲突重分配，以及 PREPARE 中 `\n` 转义与 INTERVAL RANGE 语句。R1 已收口，R2 完成 16/22，R3 完成 31/48，R7 完成 52/110；架构侧仍以「先计算层语义与执行计划形状 1:1，再补远端 Coprocessor/存储层」为主线。254 套的逐套状态与进度见 [§11 全量清单](#11-附254-套-suite-全量清单)。

---

## 2. 度量口径

- **唯一进度单位 = manifest 中 `green` 的 suite**（`r/*.result` 为 TiDB 权威金样，禁止手改）。
- `green` 必须 Gate A（结果/错误码/警告）+ Gate B（EXPLAIN 形状/estRows/task/access object）双通过，且 Mem + TiKV 双引擎复验。
- 功能波 `core-ported`、包级 parity 不计入本报告第 3 节的进度，只作为架构债务参考。

### 2.1 manifest 总览（254 suites）

| 状态 | 数量 | 占比 |
|---|---:|---:|
| `green` | 126 | 49.6% |
| `in_progress` | 9 | 3.5% |
| `pending` | 109 | 42.9% |
| `local`（SealDB 冒烟） | 8 | 3.1% |
| `deferred`（TiFlash/MPP） | 2 | 0.8% |

逐套明细（254 行，含状态/进度/金样/TiKV）见 [§11 全量清单](#11-附254-套-suite-全量清单)。

### 2.2 按波次

| Wave | green | in_progress | pending | 说明 |
|---|---:|---:|---:|---|
| R1（select / show / types） | 4 | 0 | 0 | **已收口** |
| R2（join / subquery / agg / window / cte） | 16 | 5 | 1 | 主波完成 ~73% |
| R3（point_get / index / partition / explain*） | 31 | 4 | 13 | 当前主攻波 |
| R4（plan cache / 会话） | 0 | 0 | 13 | 未开工 |
| R5 | 4 | 0 | 8 | `session/txn` / `db_integration` / `session/variable` 已绿 |
| R6 | 19 | 0 | 16 | DDL / infoschema 起步 |
| R7 | 52 | 0 | 58 | 零散长尾 |
| D（TiFlash/MPP） | 0 | 0 | 2 | deferred |
| local | — | — | — | 9 套冒烟 |

---

## 3. 已绿 suite 清单（125）

| Wave | Suite |
|---|---|
| R1 | `select`, `show`, `types/const`, `types/time` |
| R2 | `agg_predicate_pushdown`, `executor/cte`, `executor/jointest/hash_join`, `executor/jointest/join`, `executor/union_scan`, `executor/window`, `explain_union_scan`, `globalindex/aggregate`, `globalindex/index_join`, `index_join`, `naaj`, `null_rejected`, `subquery`, `topn_push_down`, `topn_pushdown`, `window_function` |
| R3 | `access_path_selection`, `clustered_index`, `ddl/db_partition`, `ddl/exchange_partition_global_index`, `executor/batch_point_get`, `executor/explain`, `executor/explainfor`, `executor/index_lookup_pushdown_partition`, `executor/index_merge_reader`, `executor/partition/issues`, `executor/partition/partition_boundaries`, `executor/partition/partition_with_expression`, `executor/partition/table`, `executor/partition/write`, `executor/point_get`, `explain`, `explain-non-select-stmt`, `explain_easy`, `explain_easy_stats`, `explain_foreign_key`, `explain_generate_column_substitute`, `explain_join_stats`, `explain_shard_index`, `explain_stats`, `expression/explain`, `globalindex/mem_index_merge`, `globalindex/mem_index_merge_non_unique`, `globalindex/point_get`, `partition`, `planner/core/casetest/point_get_plan`, `planner/core/point_get_plan` |
| R5 | `session/txn`, `session/variable`, `db_integration`, `session/bootstrap_upgrade` |
| R6 | `ddl/affinity`, `ddl/attributes_sql`, `ddl/column`, `ddl/column_change`, `ddl/column_modify`, `ddl/column_type_change`, `ddl/db_cache`, `ddl/db_change`, `ddl/db_foreign_key`, `ddl/db_rename`, `ddl/ddl_error`, `ddl/fail_db`, `ddl/index_modify`, `ddl/resource_group`, `ddl/table_modify`, `infoschema/cluster_tables`, `infoschema/v2`, `statistics/integration`, `statistics/lock_table_stats` |
| R7 | `black_list`, `bindinfo/bind`, `bindinfo/session_handle`, `bindinfo/temptable`, `executor/adapter`, `executor/charset`, `executor/ddl`, `executor/distsql`, `executor/executor_txn`, `executor/index_advise`, `executor/index_lookup_join`, `executor/index_lookup_merge_join`, `executor/index_lookup_pushdown`, `executor/stale_txn`, `expression/enum_set`, `expression/format`, `expression/time`, `expression/vitess_hash`, `globalindex/expression_index`, `globalindex/information_schema`, `globalindex/insert`, `globalindex/mem_index_non_unique`, `globalindex/update`, `parser/integration`, `planner/cardinality/trace`, `planner/core/casetest/index/partialindex`, `planner/core/memtable_predicate_extractor`, `executor/inspection_common`, `planner/core/binary_plan`, `planner/core/preprocess`, `sessiontxn/externals`, `table/cache`, `table/tables`, `table/temptable`, `util/admin`, `sessionctx/setvar`, `planner/core/indexjoin`, `planner/core/indexmerge_intersection`, `executor/compact_table`, `expression/constant_fold`, `executor/kv`, `planner/core/rule_result_reorder`, `table/index`, `executor/rowid`, `executor/analyze`, `executor/delete`, `executor/update`, `executor/insert`, `expression/cast`, `expression/noop_functions`, `planner/core/casetest/expression_rewriter`, `expression/misc` |

**复验证据**

```text
./tests/integrationtest/run-tests.sh -s memory -t executor/index_lookup_pushdown
→ PASS: memory=1
./tests/integrationtest/run-tests.sh -s tikv -k -t executor/index_lookup_pushdown
→ PASS: tikv=1
./tests/integrationtest/run-tests.sh -s memory -t ddl/db_partition
→ PASS: memory=1
./tests/integrationtest/run-tests.sh -s tikv -k -t ddl/db_partition
→ PASS: tikv=1
./tests/integrationtest/run-tests.sh -s memory -t executor/index_lookup_pushdown_partition
→ PASS: memory=1
./tests/integrationtest/run-tests.sh -s tikv -k -t executor/index_lookup_pushdown_partition
→ PASS: tikv=1
./tests/integrationtest/run-tests.sh -s memory -t executor/index_lookup_join
→ PASS: memory=1
./tests/integrationtest/run-tests.sh -s tikv -k -t executor/index_lookup_join
→ PASS: tikv=1
./tests/integrationtest/run-tests.sh -s memory -t planner/core/indexjoin
→ PASS: memory=1
./tests/integrationtest/run-tests.sh -s memory -t expression/explain
→ PASS: memory=1
./tests/integrationtest/run-tests.sh -s tikv -k -t expression/explain
→ PASS: tikv=1
./tests/integrationtest/run-tests.sh -s memory
→ PASS: memory=133 skipped=121（125 green + 8 local）
```

TiKV 侧：全量 `-s tikv` gate 在清空本地 store 后可稳定跑通。此前全量 TiKV gate 反复卡死的根因是本地 store 长期复用后膨胀到 **51 GB**（tombstone / 失效 region，日志持续 `skip invalid split key`）。处理方式：`project/scripts/run_local_pd_tikv.sh stop` + `rm -rf /tmp/sealdb-pd-tikv` 后重启全新单节点集群。实测每轮全量 gate 约增 20 GB store，因此全量 TiKV gate 前统一从空 store 起，并先用 `du -sh /tmp/sealdb-pd-tikv` 确认。

---

## 4. 近期落地能力（自 2026-11-06 起，按提交）

| 提交 | 能力 |
|---|---|
| `b2fbcd8` | meta persist 文件锁自愈：记录 owner PID，`kill -0` 判定放弃；同进程 `META_PERSIST_MUTEX` 串行化，PID 被复用或超时 120s 的锁可回收 |
| `3746f35` | `explain_foreign_key` 波文档 |
| `5ee4d15` | 外键检查/级联计划节点 `Foreign_Key_Check` / `Foreign_Key_Cascade`（含排序与 EXPLAIN 渲染） |
| `abfc1cd` | `UPDATE t1, t2 …` 逗号多表语法 |
| `072658b` | `FOREIGN KEY` 约束落 `TableInfo` + 隐式外键索引 |
| `e4dd60d` | 单索引 DNF 等值区间：一次 `IndexRangeScan` 多区间，不再误建 IndexMerge |
| `522a281` | `mysql` 系统库 bootstrap：`user`/`global_priv`/`global_grants`/`db` 等真实表 + `i_user` 索引 |
| `b310c1a` | `DESCRIBE` 的 `Null`/`Key` 列（`PRI`/`UNI`/`MUL`），视图常量列 NOT NULL，CAST 字符长度 |
| `aa0b8f3` | `group_concat` 按 TiDB 在 `var_string(20)` 投影列上聚合 |
| `a5c7fb9` | ONLY_FULL_GROUP_BY 接受 TiDB 全部聚合函数（`group_concat` 等） |
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

### 4.1 2026-11-11 → 2026-11-13 波次（统计驱动的访问路径 + EXPLAIN 形状）

| 提交 | 能力 |
|---|---|
| `9c64c1c` | `LIMIT` expected count 下推进 `HashJoin`/`MergeJoin`/`IndexJoin`（`calcChildExpectedCnt`，含排序比项）、`TIDB_INLJ` 补 `Limit` + 覆盖索引探针 + 伪统计 NDV `0.8×行数` |
| `a6fc264` | TopN 下推穿过左外连接并重排成 IndexJoin 链、`TIDB_SMJ` 有序索引扫描与 `TIDB_HJ` 左外 build 侧、`IN (SELECT … LIMIT n)` 去关联 `Apply` 渲染（`topn_push_down` 转绿） |
| `f5d968e` | 外连接 ON 条件拆分与 `OrigTblName` 保真 |
| `1a526c5` | IndexMerge DNF 统计多项式与 join 条件列表顺序 |
| `faff8e8` | 等值冲突作用域、标量子查询视图列 |
| `5aa5f30` | mysql-tester session 默认值与单元格正则 |
| `f5d3d18` | PointGet 键按存储类型重建（含 join key） |
| `3b3c3a4` | PointGet / handle 前缀 / EXPLAIN 形状对齐 |
| `94d7242` | MySQL 索引前缀长度落 `IndexColumn.length` |
| `98d8f45` | `dual` 上的 no-op `LIMIT` 折叠 |
| `b64a7c8` | 谓词被索引直方图覆盖时优先该索引 |
| `126f938` | 常量投影别名上的 `ORDER BY` 丢弃 |
| `5303a6b` | IndexMerge 臂与残余按真实统计计费 |
| `4f93446` | `test` schema bootstrap（对齐 TiDB） |
| `64f87b3` | NOT NULL 折叠按 join 别名定界 |
| `b708a99` | 隐藏列未被引用时保留 `*` 投影 |
| `bc8bca9` | `explain_easy_stats` 转绿 |
| `35e05ae` | null rejection / star 投影 / 统计对齐 |
| `470177c` | null rejection 与伪索引区间估计 |
| `1e7bca1` | 字面量折叠、相关 semi join 的 EXPLAIN |
| `72e51a2` | mysql 黑名单表 bootstrap + `ADMIN RELOAD` |
| `f39ced6` | `LIMIT` 扫描缩放 + PK 区间上保留残余过滤 |

`executor/partition/issues` 相关的分区能力（已随本轮提交落地）：

- `INSERT ... PARTITION (p0,p1)` 目标分区过滤 + `INSERT IGNORE` 下 warning `1748` / 去重 `1062`（`pkg/executor/insert.rs`、`errcode/errname`）。
- 无符号 BIGINT 分区键：`Datum::U64` 按有符号位模式哈希；`hash_conjunction_range` 用 `i128` 承接 `i64::MAX+1` 边界值（`pkg/meta/model/partition.rs`）。
- 聚簇整数主键不走二级索引路径：`find_index_covering` / `find_unique_index_on` 排除 `pk_is_handle`；索引键写入/删除跳过聚簇主键（`pkg/meta/model/table.rs`、`pkg/executor/index.rs`）。
- 聚簇主键范围扫描：`TableReader.range_scan` + `TableRangeScan` EXPLAIN 渲染与 `keep order:false`（`physical_plans.rs` / `explain.rs` / `planbuilder.rs`）。
- HAVING 下推：无聚合函数时不再注入 `cast + count` 投影（`rule_having_push_down.rs`）。

### 4.2 2026-11-18 → 2026-11-19 波次（`expression/cast` 转绿 + strict DML 溢出）

| 方向 | 能力 |
|---|---|
| 数值 → 时间 | `parseDateTimeFromNum` 的范围分段（7 位数字加宽为 `YYYYMMDD`、`YYMMDDHHMMSS`、`YYYYMMDDHHMMSS` 的边界与溢出处理），字符串字面量走按位数定形的 `datetime_from_numeric_string`（`'2008261'` → `2020-08-26 01:00:00`） |
| 宽松时间前缀 | 完整的日期时间 + 小数秒后可跟时区文本/垃圾（`'2020-01-01 12:00:00.123456 +0600 PST'` → `12:00:00.123`） |
| `CAST` 语义 | 字符串 → `DECIMAL` 保留数字前缀（`'61qw'` → 61，查询 warning 1292、strict DML 报错 1292）；`'14:15'` → `14:15:00`；`CAST(... AS CHAR CHARACTER SET gbk)` 输出 GBK 字节（`ORDER BY` 按 GBK 排序）；JSON 列的 `date/time` 仅转换 JSON 字符串，JSON 数字/布尔/null 为 NULL |
| `EXPLAIN` | `select null as a union all select 'a' as a`：`UnionAll` 下常量投影折叠（`<nil>->Column#3` / `a->Column#3`） |
| strict DDL | 整数 `0` 经 `ALTER TABLE ... MODIFY COLUMN` 变为 `DATE/DATETIME/TIMESTAMP` 时，strict SQL mode 下报 1292（保留非 strict 的零值 + warning 路径） |
| strict DML 溢出 | `CAST('18446744073709551616' AS UNSIGNED/SIGNED)`：查询中按界饱和 + 1292，strict DML 中报 1690 `BIGINT [UNSIGNED] value is out of range`（`executor/delete`、`executor/insert` 的 `TestNonStrictDeleteUpdateOverflowExpression` 收口） |

**验证**

```text
./tests/integrationtest/run-tests.sh -s both --status green
→ PASS: memory=108 tikv=107 skipped=147
```

---

### 4.3 2026-11-19 波次（`expression/noop_functions` 转绿 + GROUP BY 排序 / 星号投影）

| 方向 | 能力 |
|---|---|
| noop 三态门 | `tidb_enable_noop_functions`：`OFF` 报 1235 error、`WARN` 记 1235 warning、`ON` 静默；变量类型改为 `Text` 以接受 `WARN` |
| 覆盖语法 | `SQL_CALC_FOUND_ROWS`、`LOCK IN SHARE MODE`（含 point-get 快路径）、`GROUP BY expr ASC\|DESC`、`START/SET TRANSACTION READ ONLY`、`SET tx_read_only\|transaction_read_only=1`；带 `AS OF TIMESTAMP` 的只读事务仍是真实 stale read |
| parser | `rewrite_group_by_order` 按字节扫描（跳过字符串 / 反引号 / 注释），在 `GROUP BY` 深度删掉 `ASC`/`DESC` 并记录，session 侧接入 noop 门 |
| 星号投影 | `SELECT * … GROUP BY`：`*` 以标记下沉，`HashAggExec` 在 `open` 后按子节点 schema 展开（裸列去歧义、保留 HAVING 槽位） |
| 聚簇主键扫描 | `IndexLookUp` 对聚簇整数主键的整域 `IndexFullScan` 直接读表行（对齐 TiDB 聚簇索引 cop 任务），covering 时按索引列回填 |

**验证**

```text
./tests/integrationtest/run-tests.sh -s both -t expression/noop_functions
→ 双引擎 PASS（memory=1 / tikv=1）
./tests/integrationtest/run-tests.sh -s memory
→ PASS: memory=117 tikv=0 skipped=137（109 green + 8 local）
./tests/integrationtest/run-tests.sh -s tikv
→ PASS: memory=0 tikv=115 skipped=139（108 green + 7 local，local/txn skip_tikv）
```

---

### 4.4 2026-11-19 波次（`black_list` 转绿 + ENUM 覆盖索引聚合 / enum 黑名单门控）

| 方向 | 能力 |
|---|---|
| 覆盖索引标量聚合 | `SELECT max(a) FROM t`（无 `GROUP BY`/`WHERE`）在 EXPLAIN 构建期改走覆盖聚合参数的最窄二级索引（`IndexFullScan`），跳过表达式索引、invisible 索引、聚簇主键与 `_tidb_rowid` |
| 聚合提示 | 标量聚合遵循 `/*+ HASH_AGG() */` / `/*+ STREAM_AGG() */`，提示同时决定根聚合与 cop 分片形态（`data:HashAgg` / `index:StreamAgg`） |
| 分片列号 | `HashAgg` 取 `out_col + 1`；`StreamAgg` 覆盖索引按「每个索引键 3 个 id 减 1」；聚合参数为真实列时（如 `max(a)` over `idx(b, a)`）参数列占一个 id，分片列号固定为 `out_col + 2` |
| enum 下推门控 | `mysql.expr_pushdown_blacklist(name='enum')` 时 `cast_enum` 不可下推：标量聚合整体留根（`funcs:max(t.a)->Column#5`），谓词不参与范围推导（回退全表扫 + 根 `Selection`，`estRows` 取 0.8 选择率） |
| ENUM 渲染 | 算术语境下 enum/set 列打印隐式强转 `cast(t.a, double BINARY)`；`Datum::Enum` 在 `Selection` 中打印序号、在范围边界按 Go `strconv.Quote` 打印成员名（`range:["a" "a","a" "a"]`）；字符串不等值按成员展开为点范围 |

**验证**

```text
./tests/integrationtest/run-tests.sh -s memory -t black_list
→ PASS: memory=1 tikv=0 skipped=253
./tests/integrationtest/run-tests.sh -s memory -t clustered_index
→ PASS: memory=1 tikv=0 skipped=253
```

---

### 4.5 2026-11-19 波次（`ddl/db_cache` 转绿 + 缓存表元数据与 DDL 门控）

| 方向 | 能力 |
|---|---|
| 元数据 | `TableInfo.cached_table`（TiDB `PlacementPolicyRef` 的缓存标记），`ALTER TABLE … CACHE / NOCACHE` 落库并递增 schema version |
| SHOW CREATE | 缓存表在 `ENGINE/CHARSET/COLLATE` 后追加 `/* CACHED ON */`；`NOCACHE` 后该注释消失 |
| 拒绝矩阵 | 分区表 `CACHE` → 8242 `'partition mode' is unsupported on cache tables.`；视图 → 1347 `'db.v' is not BASE TABLE`；缓存表上的其它 `ALTER TABLE` → 8242 `'Alter Table' …`；`CREATE INDEX` → 8242 `'Create Index' …` |
| 幂等 | 重复 `NOCACHE`、未缓存表 `NOCACHE`、`CACHE → NOCACHE → CACHE` 均无告警 |

**验证**

```text
./tests/integrationtest/run-tests.sh -s memory -t ddl/db_cache
→ PASS: memory=1 tikv=0 skipped=253
./tests/integrationtest/run-tests.sh -s tikv -t ddl/db_cache
→ PASS: memory=0 tikv=1 skipped=253
```

---

## 5. 刚收口的 R3 套件

| Suite | 收口要点 |
|---|---|
| `executor/partition/issues` | 自连接派生谓词、空表 ANALYZE 仍为 pseudo、整域 `BETWEEN` 折叠为裸 `TableFullScan`、`inl_join(c)` 的 IndexJoin 形状、`desc` 的 `_N` 编号、`partition:p2` 裁剪 |
| `executor/partition/table` | `EXCHANGE PARTITION ... WITH TABLE` 语法与元数据换 ID、`_tidb_rowid` 隐藏列投影、唯一键 LEFT JOIN 的 `count(*)` 覆盖索引简化、分区表异步 auto-analyze、`SET GLOBAL tidb_partition_prune_mode` |
| `executor/partition/partition_boundaries` | `BETWEEN`/`IN` 下沉与折叠、`NOT` 绑定、`partition:dual`、value-counts 选择性 |
| `executor/partition/partition_with_expression` | 表达式分区键、字符串/数值强转、HASH 分区范围裁剪 |
| `explain_foreign_key` | 外键约束落元数据、隐式外键索引、`Foreign_Key_Check` / `Foreign_Key_Cascade` 计划节点；多表 DML 的 `WHERE` 等值键提升 + 跨侧谓词镜像 + `HashJoin CARTESIAN` / `MergeJoin` 形状选择 |
| `executor/index_merge_reader` | IndexMerge 臂与残余按真实统计计费，`TableRowIDScan` 探针基数与 `limit embedded` 形状 |
| `executor/point_get` | PointGet 键按列存储类型重建（含无符号 handle）、join key 上的点查、`Selection` 残余基数 |
| `explain_easy_stats` | 标量子查询 `Apply`、相关内层 `AdjustRowCountForIndexScanByLimit`、`stats:partial[c:missing]`、PK 直方图区间 est |
| `explain_join_stats` | NDV 驱动的 join 基数、`HashAgg` vs `StreamAgg` 选择、`TIDB_INLJ` 的 `IndexJoin` 形状 |
| `explain_shard_index` | `tidb_shard(a) = const` 重写为 `tidb_shard(a)=hash AND a=const`，走 `uk_expr` 的 PointGet / IndexRangeScan |
| `executor/executor_txn` | 显式事务里失败语句的语句级回滚（隐藏 savepoint）、TiKV `SAVEPOINT`/`ROLLBACK TO` 按写集 overlay 重放、`ON COMMIT DELETE ROWS` 提交清表 |

下一个分区目标是 `executor/partition/write`（INSERT/UPDATE/DELETE 的分区写入路径与 EXPLAIN 子计划）——**已转绿**。

---

## 6. R2 剩余与 R3 队列

- **R2 在研（5）**：`cte`, `explain_cte`, `planner/core/join_reorder2`, `planner/core/join_reorder_through_projection`, `planner/core/casetest/rule/rule_derive_topn_from_window`
- **R3 在研（4）**：`explain_complex`, `explain_complex_stats`, `explain_indexmerge_stats`, `index_merge`

策略：优先吃「同一根因能一次带绿多套」的项——EXPLAIN 节点编号/树形、IndexJoin hint、Point_Get/IndexMerge 统计形状。

### 6.1 在研套件的根因盘点（2026-11-13 复跑 triage）

| 套件 | 首差 | 需要的根能力 |
|---|---|---|
| `index_merge` | `c3 IN (SELECT …)` 留在 `Apply`、`use_index_merge` 未生效（`globalindex/mem_index_merge*` 已先行转绿：GLOBAL 索引 `_tidb_tid` 投影、保序 union 的 LIMIT 下沉、交叉臂 residual Selection） | 去关联成 semi/anti（含 marker 列）+ IndexMerge 提示 |
| `executor/explain` | 视图 `EXPLAIN` 权限（1345/1142）、`explain analyze` 运行时统计、`statements_summary` | 视图权限位 + 运行时统计 + infoschema 表（**已转绿**） |
| `explain_complex*` / `explain_indexmerge_stats` | 统计形状 + IndexMerge + 去关联的组合 | 同上 |
| `cte` / `explain_cte` | `cte` 仅余 1 段（sub case-3）：共享非递归物化 CTE 上的 EXISTS 未转 `Apply` 半连接、缺 Non-Recursive CTE producer；`explain_cte` 同族 + 列号基数 | 物化非递归 CTE 的消费/生产渲染 |
| `join_reorder2` / `join_reorder_through_projection` | MergeJoin 选择与 `Column#N` 编号 | cost 驱动 join 顺序 |

结论：R3 剩余项仍是四块基础设施——**统计驱动访问路径/基数**、**子查询去关联（semi/anti/marker）**、**表达式（函数）索引**、**权限 + 运行时统计**；其中统计与去关联两块已在 `explain_easy_stats` / `explain_join_stats` / `executor/point_get` 上验证成型，接下来按套件收口而不是重做基础设施。

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
| S3 | 优化器 hint 只覆盖 `TIDB_INLJ` / `INL_JOIN` / `MERGE_JOIN` / `INL_HASH_JOIN` 的最小闭环 | `TIDB_SMJ` / `TIDB_HJ` 的物理选择与有序索引扫描仍缺 | 补 hint → 物理算子映射（含 `keep order:true` 索引扫描） |
| S4 | 远端 Coprocessor DAG 未落地（谓词在 scan 侧过滤） | 真集群下推路径不一致 | 后续完整性波 |
| S5 | `local/agg` 冒烟金样因投影列名去反引号漂移而红 | 本地冒烟信号失真 | 待确认后由 `record` 流程重录（非 TiDB 语料） |
| S6 | 190 套 pending 体量大 | 进度曲线前慢后快 | 按波次批量吃同根因 |

---

## 9. 下一步（有序队列）

1. `index_merge`：IN 子查询去关联成 semi/anti + `use_index_merge` 提示（`globalindex/mem_index_merge*` 已转绿，IndexMerge 侧骨架可直接复用）。
2. `explain_indexmerge_stats`：统计驱动的交叉臂选择与 sel 估算。
3. 回到 R2 剩余 `cte` / `explain_cte` / `join_reorder*`。
4. 按波次推进 R4–R7；TiFlash/MPP 维持 deferred。

---

## 10. 附：代码规模

| 项 | 数值 |
|---|---:|
| Rust 源文件（pkg/cmd/tests/tools） | 271 |
| Rust 代码行 | ~110,820 |
| 集成测试输入 `t/**/*.test` | 254 |
| 权威金样 `r/**/*.result` | 258 |
| 包级对齐登记文件 | 178 |

---

## 11. 附：254 套 suite 全量清单

> 口径：`green` = Gate A（结果/错误码/警告）+ Gate B（EXPLAIN 形状/estRows/task/access object）双过，且在 Mem + TiKV 复验；`in_progress` 的进度 = `(首个差异行 − 1) / 金样总行数`，由 `run-tests.sh -s memory --status in_progress -t <suite>` 于 2026-11-13 逐套复跑得出；`pending` 未开工记 0%。金样列 `✓` = 有 TiDB 权威 `r/*.result`，`✗` = 缺金样；TiKV 列 `✓` = 双引擎复验通过，`skip` = manifest 标记跳过。

### R1 — select / show / types（4 套，green 4）

| # | Suite | 状态 | 进度 | 金样 | TiKV |
|---:|---|---|---:|:--:|:--:|
| 1 | `select` | green | 100% | ✓ | ✓ |
| 2 | `show` | green | 100% | ✓ | ✓ |
| 3 | `types/const` | green | 100% | ✓ | ✓ |
| 4 | `types/time` | green | 100% | ✓ | ✓ |

### R2 — join / subquery / agg / window / cte（22 套，green 16）

| # | Suite | 状态 | 进度 | 金样 | TiKV |
|---:|---|---|---:|:--:|:--:|
| 1 | `agg_predicate_pushdown` | green | 100% | ✓ | ✓ |
| 2 | `collation_agg_func` | pending | 0% | ✗ | — |
| 3 | `cte` | in_progress | 93%（首差 862，物质化非递归 CTE 的 EXISTS 半连接与 producer） | ✓ | — |
| 4 | `executor/cte` | green | 100% | ✓ | ✓ |
| 5 | `executor/jointest/hash_join` | green | 100% | ✓ | ✓ |
| 6 | `executor/jointest/join` | green | 100% | ✓ | ✓ |
| 7 | `executor/union_scan` | green | 100% | ✓ | ✓ |
| 8 | `executor/window` | green | 100% | ✓ | ✓ |
| 9 | `explain_cte` | in_progress | 2%（34 处差异：CTE 列号基数 + 物化/内联渲染） | ✓ | — |
| 10 | `explain_union_scan` | green | 100% | ✓ | ✓ |
| 11 | `globalindex/aggregate` | green | 100% | ✓ | ✓ |
| 12 | `globalindex/index_join` | green | 100% | ✓ | ✓ |
| 13 | `index_join` | green | 100% | ✓ | ✓ |
| 14 | `naaj` | green | 100% | ✓ | ✓ |
| 15 | `null_rejected` | green | 100% | ✓ | ✓ |
| 16 | `planner/core/casetest/rule/rule_derive_topn_from_window` | in_progress | 3%（首差 12/333） | ✓ | — |
| 17 | `planner/core/join_reorder2` | in_progress | 3%（首差 10/267） | ✓ | — |
| 18 | `planner/core/join_reorder_through_projection` | in_progress | 1%（首差 18/2354） | ✓ | — |
| 19 | `subquery` | green | 100% | ✓ | ✓ |
| 20 | `topn_push_down` | green | 100% | ✓ | ✓ |
| 21 | `topn_pushdown` | green | 100% | ✓ | ✓ |
| 22 | `window_function` | green | 100% | ✓ | ✓ |

### R3 — point_get / index / partition / explain*（48 套，green 28）

| # | Suite | 状态 | 进度 | 金样 | TiKV |
|---:|---|---|---:|:--:|:--:|
| 1 | `access_path_selection` | green | 100% | ✓ | ✓ |
| 2 | `clustered_index` | green | 100% | ✓ | ✓ |
| 3 | `ddl/db_partition` | green | 100% | ✓ | ✓ |
| 4 | `ddl/exchange_partition_global_index` | green | 100% | ✓ | ✓ |
| 5 | `ddl/partition` | pending | 0% | ✓ | — |
| 6 | `ddl/reorg_partition` | pending | 0% | ✓ | — |
| 7 | `executor/batch_point_get` | green | 100% | ✓ | ✓ |
| 8 | `executor/explain` | green | 100% | ✓ | ✓ |
| 9 | `executor/explainfor` | green | 100% | ✓ | ✓ |
| 10 | `executor/index_lookup_pushdown_partition` | green | 100% | ✓ | ✓ |
| 11 | `executor/index_merge_reader` | green | 100% | ✓ | ✓ |
| 12 | `executor/partition/issues` | green | 100% | ✓ | ✓ |
| 13 | `executor/partition/partition_boundaries` | green | 100% | ✓ | ✓ |
| 14 | `executor/partition/partition_with_expression` | green | 100% | ✓ | ✓ |
| 15 | `executor/partition/table` | green | 100% | ✓ | ✓ |
| 16 | `executor/partition/write` | green | 100% | ✓ | ✓ |
| 17 | `executor/point_get` | green | 100% | ✓ | ✓ |
| 18 | `explain` | green | 100% | ✓ | ✓ |
| 19 | `explain-non-select-stmt` | green | 100% | ✓ | ✓ |
| 20 | `explain_complex` | in_progress | 37%（首差 110/293） | ✓ | — |
| 21 | `explain_complex_stats` | in_progress | 49%（首差 124/253） | ✓ | — |
| 22 | `explain_easy` | green | 100% | ✓ | ✓ |
| 23 | `explain_easy_stats` | green | 100% | ✓ | ✓ |
| 24 | `explain_foreign_key` | green | 100% | ✓ | ✓ |
| 25 | `explain_generate_column_substitute` | green | 100% | ✓ | ✓ |
| 26 | `explain_indexmerge_stats` | in_progress | 96%（首差 196/203） | ✓ | — |
| 27 | `explain_join_stats` | green | 100% | ✓ | ✓ |
| 28 | `explain_shard_index` | green | 100% | ✓ | ✓ |
| 29 | `explain_stats` | green | 100% | ✓ | ✓ |
| 30 | `expression/explain` | green | 100% | ✓ | ✓ |
| 31 | `globalindex/mem_index_merge` | green | 100% | ✓ | ✓ |
| 32 | `globalindex/mem_index_merge_non_unique` | green | 100% | ✓ | ✓ |
| 33 | `globalindex/point_get` | green | 100% | ✓ | ✓ |
| 34 | `index_merge` | in_progress | 1%（首差 12/969） | ✓ | — |
| 35 | `partition` | green | 100% | ✓ | ✓ |
| 36 | `planner/core/casetest/partition/integration_partition` | pending | 0% | ✓ | — |
| 37 | `planner/core/casetest/partition/partition_pruner` | pending | 0% | ✓ | — |
| 38 | `planner/core/casetest/physicalplantest/physical_plan` | pending | 0% | ✓ | — |
| 39 | `planner/core/casetest/point_get_plan` | green | 100% | ✓ | ✓ |
| 40 | `planner/core/cbo` | pending | 0% | ✓ | — |
| 41 | `planner/core/integration_partition` | pending | 0% | ✓ | — |
| 42 | `planner/core/partition_pruner` | pending | 0% | ✓ | — |
| 43 | `planner/core/physical_plan` | pending | 0% | ✓ | — |
| 44 | `planner/core/plan_cost_ver2` | pending | 0% | ✓ | — |
| 45 | `planner/core/point_get_plan` | green | 100% | ✓ | ✓ |
| 46 | `session/clustered_index` | pending | 0% | ✓ | — |
| 47 | `table/partition` | pending | 0% | ✓ | — |
| 48 | `util/ranger` | pending | 0% | ✓ | — |

### R4 — plan cache / 会话（13 套，green 0）

| # | Suite | 状态 | 进度 | 金样 | TiKV |
|---:|---|---|---:|:--:|:--:|
| 1 | `collation_check_use_collation` | pending | 0% | ✗ | — |
| 2 | `collation_misc` | pending | 0% | ✗ | — |
| 3 | `collation_pointget` | pending | 0% | ✗ | — |
| 4 | `common_collation` | pending | 0% | ✓ | — |
| 5 | `executor/prepared` | pending | 0% | ✓ | — |
| 6 | `expression/charset_and_collation` | pending | 0% | ✓ | — |
| 7 | `expression/plan_cache` | pending | 0% | ✓ | — |
| 8 | `new_character_set` | pending | 0% | ✓ | — |
| 9 | `new_character_set_builtin` | pending | 0% | ✓ | — |
| 10 | `new_character_set_invalid` | pending | 0% | ✓ | — |
| 11 | `planner/core/plan_cache` | pending | 0% | ✓ | — |
| 12 | `planner/core/tests/prepare/issue` | pending | 0% | ✓ | — |
| 13 | `planner/core/tests/prepare/prepare` | pending | 0% | ✓ | — |

### R5（12 套，green 4）

| # | Suite | 状态 | 进度 | 金样 | TiKV |
|---:|---|---|---:|:--:|:--:|
| 1 | `db_integration` | green | 100% | ✓ | ✓ |
| 2 | `ddl/db_integration` | pending | 0% | ✓ | — |
| 3 | `privilege/privileges` | pending | 0% | ✓ | — |
| 4 | `session/bootstrap_upgrade` | green | 100% | ✓ | ✓ |
| 5 | `session/common` | pending | 0% | ✓ | — |
| 6 | `session/nontransactional` | pending | 0% | ✓ | — |
| 7 | `session/privileges` | pending | 0% | ✓ | — |
| 8 | `session/session` | pending | 0% | ✓ | — |
| 9 | `session/temporary_table` | pending | 0% | ✓ | — |
| 10 | `session/txn` | green | 100% | ✓ | ✓ |
| 11 | `session/variable` | green | 100% | ✓ | — |
| 12 | `session/vars` | pending | 0% | ✓ | — |

### R6（35 套，green 19）

| # | Suite | 状态 | 进度 | 金样 | TiKV |
|---:|---|---|---:|:--:|:--:|
| 1 | `ddl/affinity` | green | 100% | ✓ | ✓ |
| 2 | `ddl/attributes_sql` | green | 100% | ✓ | ✓ |
| 3 | `ddl/bdr_mode` | pending | 0% | ✓ | — |
| 4 | `ddl/column` | green | 100% | ✓ | ✓ |
| 5 | `ddl/column_change` | green | 100% | ✓ | ✓ |
| 6 | `ddl/column_modify` | green | 100% | ✓ | ✓ |
| 7 | `ddl/column_type_change` | green | 100% | ✓ | ✓ |
| 8 | `ddl/constraint` | pending | 0% | ✓ | — |
| 9 | `ddl/db` | pending | 0% | ✓ | — |
| 10 | `ddl/db_cache` | green | 100% | ✓ | ✓ |
| 11 | `ddl/db_change` | green | 100% | ✓ | ✓ |
| 12 | `ddl/db_foreign_key` | green | 100% | ✓ | ✓ |
| 13 | `ddl/db_rename` | green | 100% | ✓ | ✓ |
| 14 | `ddl/db_table` | pending | 0% | ✓ | — |
| 15 | `ddl/ddl_error` | green | 100% | ✓ | ✓ |
| 16 | `ddl/default_as_expression` | pending | 0% | ✓ | — |
| 17 | `ddl/fail_db` | green | 100% | ✓ | ✓ |
| 18 | `ddl/foreign_key` | pending | 0% | ✓ | — |
| 19 | `ddl/index_modify` | green | 100% | ✓ | ✓ |
| 20 | `ddl/integration` | pending | 0% | ✓ | — |
| 21 | `ddl/modify_column` | pending | 0% | ✓ | — |
| 22 | `ddl/multi_schema_change` | pending | 0% | ✓ | — |
| 23 | `ddl/primary_key_handle` | pending | 0% | ✓ | — |
| 24 | `ddl/resource_group` | green | 100% | ✓ | ✓ |
| 25 | `ddl/sequence` | pending | 0% | ✓ | — |
| 26 | `ddl/serial` | pending | 0% | ✓ | — |
| 27 | `ddl/table_modify` | green | 100% | ✓ | ✓ |
| 28 | `infoschema/cluster_tables` | green | 100% | ✓ | ✓ |
| 29 | `infoschema/infoschema` | pending | 0% | ✓ | — |
| 30 | `infoschema/tables` | pending | 0% | ✓ | — |
| 31 | `infoschema/v2` | green | 100% | ✓ | ✓ |
| 32 | `statistics/handle` | pending | 0% | ✓ | — |
| 33 | `statistics/integration` | green | 100% | ✓ | ✓ |
| 34 | `statistics/lock_table_stats` | green | 100% | ✓ | ✓ |
| 35 | `statistics/overflow_calc` | pending | 0% | ✓ | — |

### R7（110 套，green 48）

| # | Suite | 状态 | 进度 | 金样 | TiKV |
|---:|---|---|---:|:--:|:--:|
| 1 | `bindinfo/bind` | green | 100% | ✓ | ✓ |
| 2 | `bindinfo/session_handle` | green | 100% | ✓ | ✓ |
| 3 | `bindinfo/temptable` | green | 100% | ✓ | ✓ |
| 4 | `black_list` | green | 100% | ✓ | Mem+TiKV |
| 5 | `executor/adapter` | green | 100% | ✓ | skip |
| 6 | `executor/admin` | pending | 0% | ✓ | — |
| 7 | `executor/aggregate` | pending | 0% | ✓ | — |
| 8 | `executor/analyze` | green | 100% | ✓ | ✓ |
| 9 | `executor/autoid` | pending | 0% | ✓ | — |
| 10 | `executor/charset` | green | 100% | ✓ | ✓ |
| 11 | `executor/chunk_reuse` | pending | 0% | ✓ | — |
| 12 | `executor/cluster_table` | pending | 0% | ✓ | — |
| 13 | `executor/compact_table` | green | 100% | ✓ | ✓ |
| 14 | `executor/ddl` | green | 100% | ✓ | 1 |
| 15 | `executor/delete` | green | 100% | ✓ | ✓ |
| 16 | `executor/distsql` | green | 100% | ✓ | ✓ |
| 17 | `executor/executor` | pending | 0% | ✓ | — |
| 18 | `executor/executor_txn` | green | 100% | ✓ | Mem+TiKV |
| 19 | `executor/expand` | pending | 0% | ✓ | — |
| 20 | `executor/foreign_key` | pending | 0% | ✓ | — |
| 21 | `executor/grant` | pending | 0% | ✓ | — |
| 22 | `executor/import_into` | pending | 0% | ✓ | — |
| 23 | `executor/index_advise` | green | 100% | ✓ | ✓ |
| 24 | `executor/index_lookup_join` | green | 100% | ✓ | ✓ |
| 25 | `executor/index_lookup_merge_join` | green | 100% | ✓ | ✓ |
| 26 | `executor/index_lookup_pushdown` | green | 100% | ✓ | ✓ |
| 27 | `executor/infoschema_reader` | pending | 0% | ✓ | — |
| 28 | `executor/insert` | green | 100% | ✓ | ✓ |
| 29 | `executor/inspection_common` | green | 100% | ✓ | ✓ |
| 30 | `executor/issues` | pending | 0% | ✓ | — |
| 31 | `executor/kv` | green | 100% | ✓ | ✓ |
| 32 | `executor/merge_join` | pending | 0% | ✓ | — |
| 33 | `executor/parallel_apply` | pending | 0% | ✓ | — |
| 34 | `executor/password_management` | pending | 0% | ✓ | — |
| 35 | `executor/revoke` | pending | 0% | ✓ | — |
| 36 | `executor/rowid` | green | 100% | ✓ | ✓ |
| 37 | `executor/sample` | pending | 0% | ✓ | — |
| 38 | `executor/set` | pending | 0% | ✓ | — |
| 39 | `executor/show` | pending | 0% | ✓ | — |
| 40 | `executor/simple` | pending | 0% | ✓ | — |
| 41 | `executor/split_table` | pending | 0% | ✓ | — |
| 42 | `executor/stale_txn` | green | 100% | ✓ | ✓ |
| 43 | `executor/statement_context` | pending | 0% | ✓ | — |
| 44 | `executor/update` | green | 100% | ✓ | ✓ |
| 45 | `executor/write` | pending | 0% | ✓ | — |
| 46 | `expression/builtin` | pending | 0% | ✓ | — |
| 47 | `expression/cast` | green | 100% | ✓ | ✓ |
| 48 | `expression/constant_fold` | green | 100% | ✓ | ✓ |
| 49 | `expression/enum_set` | green | 100% | ✓ | ✓ |
| 50 | `expression/format` | green | 100% | ✓ | ✓ |
| 51 | `expression/issues` | pending | 0% | ✓ | — |
| 52 | `expression/json` | pending | 0% | ✓ | — |
| 53 | `expression/misc` | green | 100% | ✓ | ✓ |
| 54 | `expression/multi_valued_index` | pending | 0% | ✓ | — |
| 55 | `expression/noop_functions` | green | 100% | ✓ | ✓ |
| 56 | `expression/time` | green | 100% | ✓ | ✓ |
| 57 | `expression/vitess_hash` | green | 100% | ✓ | ✓ |
| 58 | `generated_columns` | pending | 0% | ✓ | — |
| 59 | `globalindex/analyze` | pending | 0% | ✓ | — |
| 60 | `globalindex/ddl` | pending | 0% | ✓ | — |
| 61 | `globalindex/expression_index` | green | 100% | ✓ | ✓ |
| 62 | `globalindex/information_schema` | green | 100% | ✓ | ✓ |
| 63 | `globalindex/insert` | green | 100% | ✓ | ✓ |
| 64 | `globalindex/mem_index_lookup` | pending | 0% | ✓ | — |
| 65 | `globalindex/mem_index_non_unique` | green | 100% | ✓ | ✓ |
| 66 | `globalindex/mem_index_reader` | pending | 0% | ✓ | — |
| 67 | `globalindex/misc` | pending | 0% | ✓ | — |
| 68 | `globalindex/multi_valued_index` | pending | 0% | ✓ | — |
| 69 | `globalindex/update` | green | 100% | ✓ | ✓ |
| 70 | `imdbload` | pending | 0% | ✓ | — |
| 71 | `parser/integration` | green | 100% | ✓ | ✓ |
| 72 | `planner/cardinality/selectivity` | pending | 0% | ✓ | — |
| 73 | `planner/cardinality/trace` | green | 100% | ✓ | ✓ |
| 74 | `planner/cascades/integration` | pending | 0% | ✓ | — |
| 75 | `planner/core/binary_plan` | green | 100% | ✓ | ✓ |
| 76 | `planner/core/casetest/expression_rewriter` | green | 100% | ✓ | ✓ |
| 77 | `planner/core/casetest/hint/hint` | pending | 0% | ✓ | — |
| 78 | `planner/core/casetest/index/index` | pending | 0% | ✓ | — |
| 79 | `planner/core/casetest/index/partialindex` | green | 100% | ✓ | ✓ |
| 80 | `planner/core/casetest/integration` | pending | 0% | ✓ | — |
| 81 | `planner/core/casetest/predicate_simplification` | pending | 0% | ✓ | — |
| 82 | `planner/core/casetest/pushdown/push_down` | pending | 0% | ✓ | — |
| 83 | `planner/core/casetest/rule/rule_join_reorder` | pending | 0% | ✓ | — |
| 84 | `planner/core/casetest/rule/rule_result_reorder` | pending | 0% | ✓ | — |
| 85 | `planner/core/expression_rewriter` | pending | 0% | ✓ | — |
| 86 | `planner/core/grouped_ranges_order_by` | pending | 0% | ✓ | — |
| 87 | `planner/core/indexjoin` | green | 100% | ✓ | ✓ |
| 88 | `planner/core/indexmerge_intersection` | green | 100% | ✓ | ✓ |
| 89 | `planner/core/indexmerge_path` | pending | 0% | ✓ | — |
| 90 | `planner/core/integration` | pending | 0% | ✓ | — |
| 91 | `planner/core/issuetest/planner_issue` | pending | 0% | ✓ | — |
| 92 | `planner/core/lateral_join` | pending | 0% | ✓ | — |
| 93 | `planner/core/memtable_predicate_extractor` | green | 100% | ✓ | ✓ |
| 94 | `planner/core/partial_order_topn` | pending | 0% | ✓ | — |
| 95 | `planner/core/plan` | pending | 0% | ✓ | — |
| 96 | `planner/core/preprocess` | green | 100% | ✓ | ✓ |
| 97 | `planner/core/range_scan_for_like` | pending | 0% | ✓ | — |
| 98 | `planner/core/rule_constant_propagation` | pending | 0% | ✓ | — |
| 99 | `planner/core/rule_join_reorder` | pending | 0% | ✓ | — |
| 100 | `planner/core/rule_outer2inner` | pending | 0% | ✓ | — |
| 101 | `planner/core/rule_result_reorder` | green | 100% | ✓ | ✓ |
| 102 | `planner/funcdep/only_full_group_by` | pending | 0% | ✓ | — |
| 103 | `sessionctx/setvar` | green | 100% | ✓ | ✓ |
| 104 | `sessiontxn/externals` | green | 100% | ✓ | ✓ |
| 105 | `table/cache` | green | 100% | ✓ | ✓ |
| 106 | `table/index` | green | 100% | ✓ | ✓ |
| 107 | `table/tables` | green | 100% | ✓ | ✓ |
| 108 | `table/temptable` | green | 100% | ✓ | ✓ |
| 109 | `tpch` | pending | 0% | ✓ | — |
| 110 | `util/admin` | green | 100% | ✓ | ✓ |

### D — TiFlash / MPP（2 套，deferred）

| # | Suite | 状态 | 进度 | 金样 | TiKV |
|---:|---|---|---:|:--:|:--:|
| 1 | `ddl/ddl_tiflash` | deferred | — | ✓ | — |
| 2 | `planner/core/enforce_mpp` | deferred | — | ✓ | — |

### local — SealDB 冒烟（8 套）

| # | Suite | 状态 | 进度 | 金样 | TiKV |
|---:|---|---|---:|:--:|:--:|
| 1 | `local/agg` | local | — | ✓ | — |
| 2 | `local/basic` | local | — | ✓ | — |
| 3 | `local/dml` | local | — | ✓ | — |
| 4 | `local/errors` | local | — | ✓ | — |
| 5 | `local/infoschema` | local | — | ✓ | — |
| 6 | `local/join` | local | — | ✓ | — |
| 7 | `local/predicate` | local | — | ✓ | — |
| 8 | `local/txn` | local | — | ✓ | skip |

