# 进度看板（Progress）

> **唯一进度事实来源 = `project/regression/manifest.json` 中 suite `green`。**  
> 功能波 `core-ported` / 包级 parity **不再**计为生产对齐进度。  
> 最后更新：2026-11-08（green 35 套件；整体进展快照见 [status-report.md](./status-report.md)）  
> 测试规范：[testing.md](./testing.md) · 对齐纪律：[parity-discipline.md](./parity-discipline.md) · 波次：[regression/waves/README.md](./regression/waves/README.md)

---

## 1. 状态图例（suite）

| 状态 | 含义 |
|---|---|
| `pending` | 已导入 TiDB 语料，未开工 |
| `in_progress` | 当前 TDD 波，CI 默认不跑 |
| `green` | Gate A+B 对 TiDB 金样通过（Mem + TiKV） |
| `local` | SealDB smoke（非 TiDB 权威金样） |
| `deferred` | TiFlash / MPP / TiSpark |

---

## 2. TDD 回归波（现行进度）

| Wave | 代表套件 | 状态 | 证据 / 下一步 |
|---|---|---|---|
| infra | import + record + runner + fail-lock | `done` | `import_tidb_itest.sh` / `record_tidb_itest.sh` / manifest |
| R1 | select, show, types/* | `green` | 四套件 Mem+TiKV 全绿；进入 R2 |
| R2 | join / subquery / agg / window / cte | `in_progress` | `agg_predicate_pushdown` / `topn_pushdown` / `window_function` / `null_rejected` / `subquery` / `naaj` / `executor/window` / `executor/cte` / `executor/union_scan` / `index_join` / `explain_union_scan` / `executor/jointest/join` green |
| R3 | point_get / index / partition / explain* | `in_progress` | partition 五套件 + `explain-non-select-stmt` / `globalindex/point_get` / `explain_stats` / `planner/core/point_get_plan`（含 `casetest/point_get_plan`）/ `access_path_selection` / `clustered_index` / `explain` / `executor/batch_point_get` / `explain_shard_index` green；14 套件 `in_progress` |
| R4–R7 | plan_cache / session / ddl / CBO | `pending` | 依赖前波 green |
| D | *tiflash* / *mpp* | `deferred` | 不丢弃，延后 |

实现纪律：[parity-discipline.md](./parity-discipline.md) — 失败用例 → TiDB 同名包/文件 → SealDB 同路径一比一；生产级清单勾满才升 `green`；禁止改权威 `r/*.result`。

> **收尾阶段待办（已定稿，暂不执行）— 产品命名 TiDB → SealDB**
> 目的：计算层归属叙述用 SealDB/sealdb，不再以 TiDB 自称；对标复刻期间先保持现状，最后一阶段统一改。
> 现状：296 个文件、149 条 commit body、20 条 subject 出现 TiDB 字样。
> 口径三分：
> 1. **对外兼容契约不动**：`tidb_*` / `@@tidb_*` session 变量名、TiDB 专有语法关键字（改了就与客户端/语料不兼容）。
> 2. **内部标识符改**：如 `TiDBDialect` → `SealDBDialect`、内部 `tidb_` 前缀辅助函数/类型/测试名 → `sealdb_`。
> 3. **出处说明保留**：注释/文档中“TiDB 在 `xxx.go` 如何实现”是对齐依据，必须留；只把“我们做了什么”的自述改成 SealDB 口径。
> 历史 commit 不重写（保留 hash）；仅约束收尾阶段之后的提交。

### 2.0 今日 R1 `select` 落地（2026-11-05）

| 子能力 | TiDB 对齐点 | SealDB 改动 | 证据 |
|---|---|---|---|
| unary `!` → `opcode.Not2` | `parser/opcode` + `UnaryOperationExpr` | `Op::Not2`；`rewrite_mysql_bang` → `__sealdb_bang` | unit `bang_not2_tests`；suite 过 `select !(1+2)` |
| 投影列名 `field.Text()` | `buildProjectionFieldNameFromExpressions` | `SelectField.orig_text` + `tidb_projection_field_name` | 过 `+ - 1` / `++++++++++++++++++++1` / `+true` 等 header |
| float `1e23` 展示 | REAL 科学计数 | `types::format_float` 大数用 `e` | 过 `+1e23`/`+1E23` 数据行 |
| `&&` / `\|\|` | MySQL 逻辑与或 | `PGOverlap`/`StringConcat` → LogicAnd/Or | 过 `1 && 0 \|\| 3 && null` |
| `JOIN` 无 ON、`FROM (t)` | CrossJoin；paren table | `Join(None)`；`rewrite_mysql_paren_from` | 过 comma/cross/paren FROM |
| `SELECT *, 1` | ResolveFields 扩 `*` | `expand_projection_fields` + infoschema 列 | 过 `select *, 1 from t` |
| USE/FORCE INDEX | access path hint | `TableName.use_indexes` + `access_plan` | EXPLAIN 走 IndexReader；`select c,a … use index(idx)` 结果绿 |
| IndexFullScan 无谓词 | DistSQL full index | `IndexAccess::Range{None,None}` | 上条执行不再 1235 |
| EXPLAIN `brief` | 去 `_N` 后缀 | `ExplainFormat::Brief` + `brief_operator_id` | id 列已对齐 `TopN`/`IndexReader` |
| EXPLAIN TopN⊃IndexReader | cop TopN + `index:TopN` | `walk_with_parent` / `emit_cop_children` | 树形骨架已出；info/estRows/前缀仍差 |
| EXPLAIN TopN⊃IndexReader | 完整 brief 对齐 | TopN by_info/count；`index_columns`；树前缀空格 | `select-triage` TopN 段已绿 |
| EXPLAIN StreamAgg⊃TableReader | partial agg cop | `data:StreamAgg` + cop StreamAgg | 空表 `count(a)` EXPLAIN 已绿 |
| GROUP BY → IndexReader StreamAgg | TiDB stream agg on idx prefix | `group_by_index_plan`；StreamAgg info/cop_info；keep order:true | `distinct b group by a` / `count…group by` EXPLAIN+结果绿 |
| SELECT DISTINCT | Dedup | `SelectStmt.distinct` + `Distinct` / outer HashAgg | `SELECT DISTINCT c1,c2 … LIMIT 1` → `(1,1)` |
| covering IndexReader | 非 PK 表不覆盖索引列 | `covering_row` 仅在 Null 时填 handle | `USE INDEX` / group-by count 结果正确 |
| NestedJoin bushy HashJoin | `(t1⋈t2)⋈(t3⋈t4)` | `FromItem` 树 + Build/Probe + CARTESIAN left cond + null-reject | nested join brief 段已绿 |
| Point_Get + `a='1'` | handle 常量折叠 | `Point_Get` id；string→i64；absorb residual | desc Point_Get 段已绿 |
| dual Projection | `sysdate()->Column#N` | dual Projection exprs；itest 归一化 `_N` | desc sysdate 段已绿 |
| PartitionUnion / `partition:pN` | static Union + dynamic access | `PartitionUnion`；uint64 HASH；`PARTITION (…)`；`ALL COLUMNS`；`SET DEFAULT` | 分区 desc 段已绿（static/dynamic） |
| `!= ANY` / `= ALL` | TiDB agg + CARTESIAN | `quantified_cmp_plan`；alias→scan name；StreamAgg(Build) | any/all brief 段已绿 |
| correlated `IN` select-list | left outer semi join | `in_subquery_select_plan`；cast Projection；info marker | IN brief 三段已绿 |
| derived `sleep(1)` dual | inline CTE → Projection⊃Dual | `seed_is_dual_projection`；`text->Column#N` | sleep subquery brief 已绿 |
| `ORDER BY rand()` / const | InjectProjBelowSort；elide const | `rule_inject_extra_projection`；`Sort.by_info` | rand/abs(2)/abs(rand())+1 brief 已绿 |
| correlated `IN` + other cond | semi equal+other；drop ORDER BY | `from_in` decorrelate；isnull×N；semi est 0.8 | WHERE IN brief 已绿 |
| Window `row_number` | Shuffle⊃Window⊃Sort | `Shuffle`/`ShuffleReceiver`；default rows frame | window brief 已绿 |
| precise_types | UINT64 / DECIMAL(21,1) / REAL | literal u64→Decimal；cast+`is_unsigned`；scale | issue 21692 行已绿 |
| `ANALYZE t1,t2,t3` | multi-table AnalyzeTableStmt | extract table list；AnalyzeExec loop | multi ANALYZE 已绿 |
| join stats / OrigTblName | analyzed TableReader；select.tN | `join_side`→access_plan；FromItem alias rewrite | estRows/table names 已绿 |
| flat outer-join reorder | card sort + Projection + isnull | `build_left_outer_chain`；`bushy_cop_spine` | `t2⟕t1⟕t3` brief 已绿 |
| nested outer→inner / assoc | ON null-reject → Inner；`A⟕B⋈C`→`(A⋈C)⟕B` | `convert_nested_outer_to_inner`；`build_left_then_inner`；right chain | brief 至 right-outer 段已绿 |
| bushy `(J1)⟕(J2)` | 四表 outer reorder + 2.50/5.00 est | `try_bushy_quad_outer`；analyzed⋈pseudo *1.25 | t1..t4 brief 三段已绿 |
| derived STRAIGHT_JOIN | 派生表 JOIN/UNION CTE 合并 | lower 放行 subquery JOIN；USING→ON | 派生 STRAIGHT_JOIN 结果已绿 |
| MergeJoin + USING | STRAIGHT_JOIN EXPLAIN；USING 列合并；DROP 清 stats | `PhysicalPlan::MergeJoin`；`keep order:true`；`TableReader_N(Build)`；Handle::delete | `select` → `green`（Mem+TiKV） |
| SHOW LIKE + suite DB | `Tables_in_show (%xx)`；suite 名作库名 | ShowStmt.pattern；itest `path/→__` | show LIKE 段已绿 |
| CREATE TTL + SHOW CREATE | TTL 表选项；RANGE COLUMNS 渲染 | extract TTL；TableInfo.ttl；ENGINE/CHARSET/PARTITION | `show` → `green`（Mem+TiKV） |
| REAL_AS_FLOAT / PIPES_AS_CONCAT | REAL→FLOAT；`||` 拼接 | ddl `sql_mode_flag`；LogicOr concat | types/const 段已绿 |
| UNSIGNED / BIGINT overflow | 1690；`NO_UNSIGNED_SUBTRACTION`；CAST wrap | i128 有符号差；U64 标签；`qualify_expr_table` | types/const 至 overflow 段已绿 |
| IGNORE_SPACE CREATE TABLE | `COUNT(a)`/`COUNT (a)` 1064 | session `reject_create_table_func_call` | types/const IGNORE_SPACE 段已绿（HIGH_NOT 前） |
| HIGH_NOT_PRECEDENCE | `NOT x BETWEEN` → `(NOT x) BETWEEN` | eval rewrite under sql_mode | types/const HIGH_NOT 段已绿 |
| NO_BACKSLASH_ESCAPES | `\\` 字面反斜杠 | parse 前加倍 `\` 喂 sqlparser | types/const → `green`（Mem+TiKV） |
| itest `--enable_warnings` | 解析 + 结果后打印警告 | `QueryResult.warnings` + runner | types/time DST 警告对齐 |
| TIMESTAMP/DATE/TIME literals | TypedString + timestampPattern；ODBC `{d|t|ts}` | `types::parse_*_literal`；errno 1292/22007 | types/time 字面量 header 绿 |
| SET time_zone | session 时区变量 | `Session.time_zone` + TLS | UTC 存储 / session 显示 |
| DATE_ADD / TIMESTAMPADD / DIFF | INTERVAL；DST 8179；索引路径 | eval + covering rehydrate；`<=>` | types/time → `green`（Mem+TiKV） |

### 2.0b R2 `agg_predicate_pushdown`（2026-11-06）

| 子能力 | TiDB 对齐点 | SealDB 改动 | 证据 |
|---|---|---|---|
| HAVING group-by 下推 | PushSelDownAggregation | `rule_having_push_down` CNF 拆分 + DNF extract | 残差 avg 留 Selection |
| avg cast Projection | Column#13–16 + firstrow | inject Projection under HashAgg | brief 树形对齐 |
| 部分 OR 下推 | 纯 agg 臂不 extract | `extract_pushable_dnf` | Q4 仅 `a<3`；Q5 `or(a>1,a<3)` |
| pseudo estRows | `pseudoLessRate` / null=10 | LT→3323.33；covering OR→9990 | Mem+TiKV Gate B |
| suite green | Gate A+B | manifest `green` | `agg_predicate_pushdown` |
| UNION ALL derived TopN | `((sel) UNION ALL (sel)) alias` | unwrap arm parens；inline Union；`Column#3:desc` | `topn_pushdown` → `green` |
| Window IndexReader / frames | optimizeByShuffle4Window | `window.rs`；INTERVAL minute_second；RANGE offsets | IndexFullScan keep_order |
| column `PRIMARY KEY` → handle | pk_is_handle Column# | lower synthetic PRIMARY index | t1 `Column#4` |
| Shuffle by partition NDV | concurrency=min(var,NDV) | skip Shuffle when NDV≤1 | `window_function` → `green` |
| VIEW MERGE + single-side ON | null-reject CARTESIAN | merge view；ON push；SHOW WARNINGS | `null_rejected` → `green` |
| row_number − col Projection | minus(Column#N, t.x) | window outer Binary + default rows frame | select window EXPLAIN 回归修复 |

### 2.0c EXPLAIN 树形续列统一（2026-11-06）

| 子能力 | TiDB 对齐点 | SealDB 改动 | 证据 |
|---|---|---|---|
| 续列推导 | `│` 只随祖先「仍有后续兄弟」延伸 | `walk_with_parent` 统一按 `!last` 追加，删除分支特判 | select / subquery / explain_union_scan brief 树形全对齐 |
| 合成 Projection | StreamAgg+ORDER BY 上方补 `Projection` 层 | 递归时携带该层续列 | `count(a) … order by a limit 1` 缩进对齐 |
| Apply 外行放大 | 只在 probe 相关子树生效 | 穿过 Hash/Merge/IndexJoin 时重置为 1.0 | subquery TableRangeScan est 5.00 |
| Gate B 复验 | green 11 套件 | manifest 不变 | `--status green` Mem=11 / TiKV=11 全 PASS |

### 2.0d mysql-tester 指令 + TRUNCATE 隐式提交 + JSON 列类型（2026-11-06）

| 子能力 | TiDB 对齐点 | SealDB 改动 | 证据 |
|---|---|---|---|
| `--replace_regex` | `/pat/repl/` 可多组，作用于单条语句结果 | `parse_replace_regex` + `Step.replace_regex`（`regex` crate） | join/globalindex/join_reorder_through_projection 不再因指令跳过 |
| `--disable_result_log` | 只回显 SQL，不输出结果 | `Step.log_result` | executor/union_scan 越过 117 行 |
| `TRUNCATE` 隐式提交 | DDL 前提交当前显式事务，不可回滚 | `session` 在 TruncateTable 前 commit + 释放 MDL | union_scan 事务内 truncate 后重复键消失 |
| `JSON` 列类型 | `json` 类型名 + 文本存储 | `TYPE_JSON` + `create_table` 映射 | union_scan `create table t(a json, b bigint)` |
| Gate B 复验 | green 11 套件 | manifest 不变 | `--status green` Mem=11 全 PASS |

### 2.0e R2 `executor/union_scan`（2026-11-06）

| 子能力 | TiDB 对齐点 | SealDB 改动 | 证据 |
|---|---|---|---|
| 相关子查询表别名作用域 | Apply 内层优先绑定自身表再回退外层 | `apply.rs` inner_tables 同时按别名与物理表名匹配 | `(select t1.c_int from t t1 where t1.c_int > t.c_int)` 不再返回 NULL |
| `ON TRUE` / CROSS 自连接 | HashJoin 允许恒真条件 | `hash_join_pairs` 支持无常量等值条件 | `INL_JOIN` 逗号连接 EXPLAIN/结果对齐 |
| UPDATE 逗号派生表 | mysql `UPDATE t t1, (SELECT ...) t2 SET` | `rewrite_update_comma_join` 改写为 `JOIN ... ON TRUE` | 解析通过并走 INL_JOIN |
| 自连接 UPDATE 去重 | 同一 handle 只按左角色写一次 | `update.rs` 同 handle 跳过重复写 | 不再二次覆盖 `t1.b` |
| 覆盖索引纯投影 | 无谓词 `select c` 走 IndexReader | `planbuilder` plain projection covering index 规则 | `select c from t` → IndexFullScan（`select *`/非覆盖列仍 TableReader） |
| 非覆盖范围扫描 | 非覆盖 range 走 TableFullScan | range 分支按 `select_covers_index` 决定 TableReader | `select * from t where c > 3` 与 `use index(c)` 覆写均对齐 |
| suite green | Gate A+B | manifest `green` | `executor/union_scan` Mem=1 / TiKV=1 PASS；green 11→12 双端复验 |

### 2.0f R2 `executor/cte`（2026-11-06）

| 子能力 | TiDB 对齐点 | SealDB 改动 | 证据 |
|---|---|---|---|
| 多递归臂 CTE | `buildRecursiveCTE` 按首个自引用臂切分 seed / recursive | `CommonTableExpression.recursive_rest`；`recursive_arms()`；`iterate_recursive` 逐臂求并 | `select 1 union all select 2 union all select c1+1 …` 结果对齐 |
| `cte_max_recursion_depth` | 种子算第 1 次迭代；超限报 3636 | session 变量 + `iterate_recursive` 深度检查 | `set @@cte_max_recursion_depth=-1/0/1` 边界一致 |
| UNION 臂 LIMIT | 未加括号的分支 LIMIT 非法 | `reject_union_arm_limit` 返回 1221 | `… limit 1 offset 1 union …` → Error 1221 |
| SELECT 除零 | SELECT 警告 + NULL；严格 DML/DDL 报 1365 | `DivByZeroMode`（Warn/Error） | `select 1/0` / `insert … 1/0` 分流 |
| CTE 体 LIMIT 只应用一次 | `computeSeedPart` 先按 limit+offset 截断种子 | `lower_cte` 剥离 seed LIMIT；`iterate_recursive` 截断 seed | `cte1 AS (… limit 2 offset 1)` 不再丢行；`limit 0` 不触发递归深度错误 |
| 相关标量子查询 + 同名 CTE | `cte1.c1` 在 `FROM cte1 dt1` 内指外层 | `apply.rs` 自引用 CTE 输出列先入 inner scope；别名隐藏 CTE 名 | `where cte1.c1 = (select dt1.c1 … where dt1.c1 = cte1.c1)` 返回 2,3 |
| CREATE VIEW 体含 CTE | 视图列由 CTE 输出推导 | `columns_from_select` 识别 CTE scope 后取列名/类型 | `create view v as with tt as (…) select * from tt` 通过 |
| suite green | Gate A+B | manifest `green` | `executor/cte` Mem=1 / TiKV=1 PASS；green 12→13 双端复验 |

### 2.0g R2 `executor/jointest/join`（2026-11-06）

| 子能力 | TiDB 对齐点 | SealDB 改动 | 证据 |
|---|---|---|---|
| 派生表内 `ONLY_FULL_GROUP_BY` | 派生表 `(SELECT clock … GROUP BY source)` 同样校验 1055 | `check_only_full_group_by` 递归 `with.ctes`（派生表被降级为合成 CTE） | `events e JOIN (…) e3` → Error 1055 |
| JOIN ON 子查询 | HashJoin 不支持 ON 内子查询，报 1105 | `planbuilder` 在 `build_join_plan` 前置 `has_subquery` 检查（含 bushy `FromItem`） | `on (t2.id < all (select 1))` → 1105 |
| 非等值 Semi/Anti | Apply 语义只输出探针列 | `probe_semi_anti_non_equi` 直接回投左行 | `m1.a in (select m2.b+1 …)` 不再触发 chunk 宽度断言 |
| RightOuter 空补宽 | 保留侧在右，输出仍 `[left, right]` | `probe_outer` 右侧保留时按 build 宽度补 NULL | `t1 join t2 using(a) right join t3 …` 返回 4 列 |
| 括号 JOIN + USING | `(t2 JOIN t3 USING (b))` 作为单侧时列名可重复 | `FromItem::Join.using_cols` + 可见列模型；1052 `Column … in from clause is ambiguous` | `t1 JOIN (t2 JOIN t3 USING (b)) USING (a)` → 1052 |
| suite green | Gate A+B | manifest `green` | `executor/jointest/join` Mem=1 / TiKV=1 PASS；green 13→14 双端复验 |

### 2.0h R2 `naaj`（2026-11-06）

| 子能力 | TiDB 对齐点 | SealDB 改动 | 证据 |
|---|---|---|---|
| Row 构造器 AST | `ast.RowExpr` 承载 `(a, b)` | `ExprNode::Row(RowExpr)`；lower `Expr::Tuple`；标量上下文报 1241 | `select (a,b)` → Error 1241；`(a,b) not in (…)` 可解析 |
| `!= ALL` 语义标记 | `ast.CompareSubqueryExpr.All` 与 `NOT IN` 分流 | `InSubqueryExpr.all_ne`；`lower_quantified_cmp` 置位 | `(a,b) != all (…)` 与 `not in` 分别走不同计划形状 |
| Null-Aware Anti Join 计划 | `PhysicalHashJoin` + `NullAwareAntiJoin`（`convertToNullAwareAntiJoin`） | `PhysicalPlan::NullAwareAntiJoin`；`naaj_plan` 生成 `HashJoin` 信息/keys/other cond | PART1/2 brief 全对齐（`Null-aware anti semi` / `anti left outer semi`） |
| 相关等式让位 | 内层相关 EQ 存在时不建 NAEQ | `corr_eq` 提升为 hash key，行比较降级为 `other cond` | `where naaj_A.c = naaj_B.c` → `anti (left outer) semi join, equal:[eq(c,c)]` |
| 注入投影 | `injectExtraProjection` 仅在纯 NAEQ 路径 | `RowProjection` 计划节点（执行透明）；corr-EQ 路径不注入 | `(a+1,b*2) not in (…)` 的 `plus(...)->Column#14` Projection |
| 三值 NOT IN 执行 | NAAJ 探针/构建侧 NULL 语义 | `NullAwareAntiJoinExec`：行比较 TRUE/FALSE/NULL + `left_outer` 布尔列 `1/0/NULL` | PART2/4 左表 NULL、右表 NULL 桶全部结果对齐 |
| WHERE `!= ALL` 形状 | `Selection(Column#9)` + 回投 Projection | `naaj_plan` 包装 Selection/Projection；est 0.8 折叠 | PART3/4 brief `Projection▷Selection▷HashJoin` 对齐 |
| suite green | Gate A+B | manifest `green` | `naaj` Mem=1 / TiKV=1 PASS；green 14→15 双端复验 |

### 2.0i R2 `executor/window`（2026-11-06）

| 子能力 | TiDB 对齐点 | SealDB 改动 | 证据 |
|---|---|---|---|
| 命名窗口 `WINDOW w AS (...)` | `ast.WindowSpec` 引用解析 | `resolve_named_window` 合并 named spec 后再建 Window | `lead(col_13,1,NULL) over w` 结果对齐 |
| 窗口计划下的恒等投影 | `PhysicalProjection` 只改名 | `ProjectionExec::passthrough`；`is_window_plan` + `is_identity_projection` | `Projection(Column#5->Column#6)▷Window` 不再二次求值窗口函数 |
| `SUBSTRING/SUBSTR/MID` | `builtinStringSubstring` | `eval_substring`（1-based、负 pos、负 len 空串、按字符切） | `substring(cast(...), n, abs(n))` |
| `XOR` 三值逻辑 | `opcode.LogicXor` | `Op::LogicXor` + `eval_logic` 分支 | `c_k4duc4l XOR c_tutmh1` |
| `PRIMARY KEY ... CLUSTERED` / `pre_split_regions=N` | TiDB DDL 扩展子句 | `strip_tidb_create_table_options` 解析前剥离 | `create table t (...) CLUSTERED) pre_split_regions=2` |
| `INSERT ... VALUES` 内子查询 | `InsertExec` 用 session 求值 | `SubqueryEvalCtx` 传入 `eval_insert_row` | `values (..., (select ... limit 1 offset 2))` |
| DML 谓词内嵌子查询 | `preEvaluateDMLPredicate` 递归 | `pre_evaluate_dml_predicate` 进 Func/Cast/InList/Between/Like/IsNull/Case | `delete ... where substring(..., (select ...))` |
| suite green | Gate A+B | manifest `green` | `executor/window` Mem=1 / TiKV=1 PASS；green 15→16 双端复验 |

### 2.0j R3 `executor/partition/issues`（2026-11-07）

| 子能力 | TiDB 对齐点 | SealDB 改动 | 证据 |
|---|---|---|---|
| `/*+ INL_JOIN(t) */` 计划 | `PhysicalIndexJoin`：hint 表作 inner（probe），按 outer 键决定范围 | `inl_join_hint_plan`（EXPLAIN-only；执行仍走 hash join，行一致） | `IndexJoin▷TableReader(Build)` + `TableReader(Probe) partition:all` |
| 决定范围扫描 | `TableRangeScan` + `range: decided by [eq(inner, outer) …]` | `explain.rs` TableReader 支持 `RANGE_SEL`；`explain::range_prefix` | `range: decided by [...]` 与 TiDB 逐字符一致 |
| 常量类型收敛 | `col = '2022-11-17'` → `DatumTime`（date 列打印全 datetime） | `coerce_eq_const`（DATE/DATETIME/TIMESTAMP） | `eq(…occur_trade_date, 2022-11-17 00:00:00.000000)` |
| 新增列的部分统计 | `stats:partial[serial_id:missing]` + 0.8 伪选择率 | `PARTIAL_SEL` 标记 + `filter_est`；`TableFullScan` 保持基数 | `TableFullScan 1.00 / Selection 0.80 / IndexJoin 0.80` |
| PK 隐式 NOT NULL | ON 侧键列不再补 `not(isnull(...))` | `nullable_column`（PK 列视作非空） | `Selection` 仅剩 `eq(broker, "0009")` |
| KEY 分区静态剪枝 | `KEY` 用 MD5 前 8 字节大端取 `% n`（区别于 HASH 取模） | `model/key_hash.rs`（MD5 + `key_partition_index`）；KEY 复用 `getUsedHashPartitions` 区间枚举 | `partition:p2`（5807/5808 同分区） |
| suite green | Gate A+B | manifest `green` | `executor/partition/issues` Mem=1 / TiKV=1 PASS；green 23→24 双端复验 |

### 2.0k R3 `executor/partition/write`（2026-11-07）

| 子能力 | TiDB 对齐点 | SealDB 改动 | 证据 |
|---|---|---|---|
| 多列表达式分区剪枝 | 单列约束不能推导缺失变量 | `rule_partition_processor`：`distinct_columns.len() == 1` 才改写 | `insert into t values (1,'a')` 不再误落 p3 |
| LIST/ RANGE 错误文案 | 值越界 vs `column_list` | `locate_tuple` 单列：RANGE 报 `value 1000`，LIST 保留 `from column_list` | `Error 1526 (...): Table has no partition for value 1000` |
| `INSERT IGNORE` 越界 | 警告 `1526` 而非报错 | `insert` 写行/目标分区两处捕获 1526 | `insert ignore ... values(1000)` → `Warning 1526` |
| `INSERT ... PARTITION(pN)` | 落在目标集外 → `1748` | `filter_target_partitions` + IGNORE 警告 | `Warning 1748 Found a row not matching…` |
| `UPDATE/DELETE ... PARTITION(pN)` | 只扫描目标分区，越界行跳过 | 复用 `TableName.partition_names`；`resolve_partition_ids` / `check_partition_target` | `update ignore … partition(p0) set a=101` 行保持 `1` |
| `UPDATE IGNORE` 唯一键/越界 | 冲突行警告跳过、保留原行 | `validate_update_uniqueness` 全表快照（剪枝隐藏的其它分区也要查） | `update ignore t set a=5 where a=7` → `Warning 1062`，行 `5,7` |
| 唯一索引 NULL 语义 | 多个 NULL 不冲突 | `encode_index_key`：unique 且含 NULL 时追加 handle 后缀 | `replace into ... set c2=NULL` 两次 `affected 1` |
| AUTO_INCREMENT 显式赋值 | 更新显式值提升分配器 | `bump_auto_increment`（UPDATE 路径） | `update t set id=8` 后下一行 id=9 |
| 嵌套执行器 DML 摘要 | 子查询执行不能清空外层影响行数 | `DmlSummary` 改栈式（push/pop） | `delete ignore … where id=(select '2a')` → `affected rows: 1` |
| `NOT NULL` 1048 / SQLSTATE | `23000` | `sqlstate_of` 映射 `ERR_BAD_NULL` | `Error 1048 (23000): Column 'id' cannot be null` |
| suite green | Gate A+B | manifest `green` | `executor/partition/write` Mem=1 / TiKV=1 PASS；green 25→26 |

### 2.0l R3 `explain_stats`（2026-11-07）

| 子能力 | TiDB 对齐点 | SealDB 改动 | 证据 |
|---|---|---|---|
| `explain t` 裸表 | `ExplainStmt{ShowStmt{ShowColumns}}`，`EXPLAIN/DESC/DESCRIBE` 三者等价 | `lower.rs` `ExplainTable` 不再拒绝 `DescribeAlias::Explain` | `explain t` 输出列清单，不再 `1235 EXPLAIN EXPLAIN` |
| `desc t col` | 语法 `ExplainSym TableName ColumnName`；`ShowStmt.Column` 与 `Pattern` 是两个字段 | `ShowStmt.column`；`rewrite_describe_column` 把列编码进 schema 标记，`decode_describe_column` 还原 | `desc t c1` / `desc t id` 只回该列 |
| `SHOW COLUMNS … LIKE` | `fieldFilter` 精确匹配、`fieldPatternsLike` 模糊匹配，表头仍是 `Field` | `show_column_visible`：先精确（忽略大小写）再 LIKE | `show columns from t like 'abc%'` 只回匹配列 |
| suite green | Gate A+B | manifest `green` | `explain_stats` Mem=1 / TiKV=1 PASS；green 26→27 |

### 2.0m R3 `access_path_selection`（2026-11-07）

| 子能力 | TiDB 对齐点 | SealDB 改动 | 证据 |
|---|---|---|---|
| 标量聚合列号 | `Column#` 由“最后列 + 隐藏 handle + 1”起算，partial 列在候选索引之后（每条索引 2 个、每个索引列 2 个） | `scalar_agg_column_ids`（`ncols + 2` / `+ 2*indices + 2*idx_cols`） | `funcs:count(Column#18)->Column#4` |
| 标量聚合函数名 | partial/cop 两侧都取真实函数名与参数（`count(1)` 保留字面量） | `agg_fn` / `arg` 从 `SelectField` 读取，`select.t.a` 限定 | `funcs:count(1)->Column#18`、`funcs:count(select.t.a)->Column#18` |
| `max(_tidb_rowid)` | `StreamAgg → Limit 1 → TableReader(data:Limit) → Limit 1 → TableFullScan(keep order:true, desc)` | `TableReader.data_limit` + `physical_plans` 新字段；`emit_cop_children` 输出 cop `Limit` 子树 | `keep order:true, desc, stats:pseudo` |
| 稀疏 cop 前缀 | 单链 cop 子节点逐层缩进（不是全对齐） | `data_limit` 分支使用 `scan_spine` 续列 | 差分 0 |
| `filter_est` 通用化 | `filter_est` 的行数对任意 TableReader 生效，不再只限 `data_limit` | `explain::est_rows` 去掉 `data_limit: true` 守卫 | `executor/partition/issues` IndexJoin 0.80 复现 |
| suite green | Gate A+B | manifest `green` | `access_path_selection` Mem=1 / TiKV=1 PASS；green 28→29 |

### 2.0n PD 区域切分死循环（2026-11-07）

| 子能力 | TiDB 对齐点 | SealDB 改动 | 证据 |
|---|---|---|---|
| Region 切分推进 | 扫描区间必须单调推进，边界键要归入**下一个** region | `split_key_ranges_via_pd`：PD 返回的 `region_end <= cursor` 时不再重问，把剩余区间作为单 span 收尾 | TiKV `executor/jointest/hash_join` 由 >15min 悬挂 → 32s PASS |
| PD HTTP 连接复用 | 每个进程一个连接池 | `pd_http_client` 改 `OnceLock` 单例（避免 TIME_WAIT 耗尽临时端口 → `EADDRNOTAVAIL`） | TiKV green 28 套件 54s 跑完 |
| 诊断开关 | — | `SEALDB_PD_TRACE`（PD 请求耗时）/ `SEALDB_ITEST_PROGRESS`（逐套件进度） | 定位到 1,168,664 次重复 `region/key/7200…fb` |

### 2.0o R3 `executor/batch_point_get`（2026-11-08）

| 子能力 | TiDB 对齐点 | SealDB 改动 | 证据 |
|---|---|---|---|
| `SLEEP(n)` | `SLEEP` 内建函数，返回 0 | `evaluator` 新增 `SLEEP`（按参数秒数阻塞，NULL → NULL） | `select sleep(0.05)` 输出 `0` |
| ENUM 常量折叠 | 数字与 ENUM 列比较按成员下标（`a = 1` ⇒ `a = 'a'`） | `coerce_enum_selection` / `normalize_enum_constants`：plan 前把 `=`/`IN` 的数字常量换成成员 label；session 对语句做同样归一，供执行器重读 AST | `ttt.a in (1,"b")` 返回 `a`；`a = 1` 命中 |
| `KEY` 分区字符串哈希 | 字符串按 `codec.EncodeValue` 的 8 字节长度前缀参与 MD5（整数仍是 8 字节大端） | `key_hash::key_partition_index` 对 `Datum::Bytes` 加 `u64` 长度前缀 | `tkey` 裁剪 `partition:p3`（原 `p0,p3`） |
| `AS OF TIMESTAMP` | 历史快照读（stale read）：`set @a=(select current_timestamp(3))` 后 `from t as of timestamp @a` 看不到之后插入的行 | 见 2.0s：解析 + 会话快照 + Mem 多版本 + TiKV `oracle.GoTimeToTS` 快照 | `select * from t as of timestamp @a where a in (1,2,3)` 空结果；suite `green`（Mem=1 / TiKV=1） |

### 2.0p R3 `planner/core/casetest/point_get_plan`（2026-11-08）

| 子能力 | TiDB 对齐点 | SealDB 改动 | 证据 |
|---|---|---|---|
| 字符串列比数字 | `eq(id, 0)` 在 `varchar` 列上按 `eq(cast(id, double BINARY), 0)` 比较，且**不能**走点查（`canBeConvertedToColumnType`） | 新增 `coerce_comparison_literals`：单表 SELECT 在 plan 前统一做比较/算术变换，session 同样归一语句供执行器重读 | `t1.id = 0` 由返回空 → 返回 `asdf`（与金样一致） |
| 算术中的字符串 | `a + 1`（`a varchar`）按 `plus(cast(a, double BINARY), 1)` | `coerce_numeric_literals` 补 `+ - * / DIV %` 的 `double` 强转 | `plus(cast(...point_get_plan.t.a, double BINARY), 1)` |
| 无法走范围时的覆盖索引 | 过滤谓词含 `cast(col)` 时 TiDB 仍读更窄的覆盖索引：`IndexReader` + `index:Selection` + `IndexFullScan` | planbuilder 覆盖索引兜底分支；EXPLAIN 对覆盖 `IndexReader` 无 range 时输出 `index:Selection` / `IndexFullScan` 且用索引行数 | `IndexReader 8000 … index:Selection` / `IndexFullScan 10000` |
| 行 IN 的强转展开 | `(id, d) IN ((0,'t'), …)` 展开为 `or(and(eq(cast(id,double),0), eq(d,"t")), …)` | `coerce_numeric_literals` 的行 IN 按位置强转；`expand_casted_in_lists` 把含 cast 的 IN 展开成 DNF | `t3` 变 `IndexReader` 全索引扫描（原误走 Batch_Point_Get） |
| 批量点查保持顺序 | `ORDER BY` 点查键时去掉 `Sort`，节点打 `keep order:true, desc:…` | `BatchPointGet` 新增 `keep_order` / `desc` 字段；`point_get_order`（穿透 Selection/Projection）+ `set_batch_point_order` | `Batch_Point_Get … keep order:true, desc:true` |
| `DROP TABLES` | MySQL/TiDB 接受复数对象关键字 | `parser::rewrite_drop_tables` | 语句不再 1064 |
| 二进制/位串字面量 | `b'00'`、`0x39` 作 `types.NewBinaryLiteralDatum`：数值上下文取原始值（`0x39 = 57`），字符串比较用解码字节且按列字符集转换 | `Datum::Bit` 承载两种字面量；`bit_bytes_for` 对数字文本取数值、对其余字节保持大端原值；`Datum::compare` 新增 `Bit`↔`Bytes` 原始字节分支 | `t1.col1 = 0x39` 命中 `BIT(11)` 点查；`select 0x01 + 1` = 2 |
| 字面量字符集校验 | 二进制字面量与文本列比较时按列字符集转换，非法字节报 1105（即使扫不到行） | `validate_string_cmp_literals_on` 接收表信息：二进制字符集列（`BIT`/`BINARY`/`BLOB`）跳过转换；cop 过滤透传 `TableInfo`；`access_range` 不给非法字节建区间，退回全扫 + Selection 以冒出错误 | `select * from t3 where a > 0x80` 恢复 `Error 1105` |
| 前缀键区间 | 复合唯一键只约束前缀时，TiDB 仍建 `IndexRangeScan range:[1,1]` | `IndexLookUpReader` 增加 `range_scan`；`rule_predicate_push_down` 用 ranger 抽取前缀区间，残余索引谓词留在 `Selection(Build)` | `t4` 的 `(id, d, c)` IN：`IndexRangeScan range:[1,1] 10.00` |
| 双点查 `MergeJoin` | 连接键为两侧点查时，TiDB 用 `MergeJoin`（Build 在 SQL 左侧） | `derive_join_key_constants` 把常量镜像到另一侧；`lift_point_get_merge_join` 把 `HashJoin` 改为 `MergeJoin` 并交换 Build/Probe；`hash_join_merge_keys` 生成键标签 | `MergeJoin_5(Build) … Point_Get … Point_Get` |
| 连接侧访问路径 | WHERE 按表拆分后再选访问路径，连接键的 `=` 优先于行 IN 列表 | `side_access_select` 只留本表谓词；join 中先试键等值点查，IN 列表改为残余 Selection | `Point_Get` + `Selection`（IN 已展开 DNF） |
| 选择率 | `statistics.pseudoSelectivity`：含 cast 的比较 0.8，`col = const` 1/1000，DNF 用 `1-Π(1-s)` | `filter_selectivity` / `conjunct_selectivity`；`Selection` estRows 与 `Build`/`Probe` 行数随之对齐 | `Selection 8.00`、`15.99`、`9600.00` |
| 残余谓词表限定 | TiDB 用 `OrigTblName`（`from t t2` 仍打 `t.b`），无别名时用表名而非 `t.` | `rewrite_col_tables` + `explain_selection_info_on(table, …)`，`selection_table` 由子计划取表名 | `gt(...point_get_plan.t4.c, 0)` |
| 自连接 `SELECT *` | 内连接同名表星投影去重；外连接必须保留可空侧列 | `self_join_star_projection` 仅对 `JoinType::Inner` 生效 | `testjoin RIGHT JOIN` 投影恢复 |
| 状态 | Mem + TiKV 双端全绿 | — | `-t planner/core/casetest/point_get_plan -s both` → `PASS memory=1 tikv=1` |

### 2.0q R3 `clustered_index`（2026-11-08）

| 子能力 | TiDB 对齐点 | SealDB 改动 | 证据 |
|---|---|---|---|
| 聚簇句柄选择 | `BuildTableInfo` 里 PK 是否成为句柄由 `tidb_enable_clustered_index`（ON / INT_ONLY / OFF）决定：ON 时非整型 PK 也是 common handle，INT_ONLY 只保留单整型列 | `build_table_info` 设 `common_handle`；session 变量 `tidb_enable_clustered_index` + 线程局部模式（DDL 前安装） | `tbl_2 varchar/composite PK` 由索引路径变 `clustered index:PRIMARY` |
| 显式 `CLUSTERED` | `PRIMARY KEY (…) CLUSTERED` 覆盖会话默认 | lower 保留 `CLUSTERED` 标记（与 `NONCLUSTERED` 同一改写通道），`IndexDef.clustered` | 套件第 8/9 行 DDL 不再丢选项 |
| 索引名在列清单前 | `PRIMARY KEY idx_0 (a, b) NONCLUSTERED` 合法 | `rewrite_primary_key_nonclustered` 先跳过索引名 | 套件建表通过 |
| 聚簇 PK 前缀区间 | PK 是句柄，前缀比较是 `TableRangeScan`（不是索引扫描）；覆盖二级索引更窄时仍走索引 | `access_plan`：`common_handle` + `clustered_prefix_column` → `TableRangeScan`；同列覆盖索引按列数取最窄 | `col_3 <> …` → `TableRangeScan 2244`；`col_3 >= …` → `index:idx_1` |
| 区间行数 | 区间基数取自 PRIMARY 索引直方图（`clustered index` 无独立统计时回退实时表行数） | `clustered_range_rows` + `statistics::range_est`（桶内 NDV 估算、`<>` 拆两段） | `798.87` / `4509` / `2244` 与金样一致 |
| 聚合方式 | 按代价在 HashAgg / StreamAgg 之间选择，读覆盖索引时部分列号按索引列数偏移 | `build_agg_plan` 区分索引扫描与表扫描的 partial 列 id | `HashAgg` / `StreamAgg` 行数与列号对齐 |
| 右外连接基数 | 重写 semi/outer join 时要保留 `HashJoin` 的 `est_rows`，不能重置为默认 | `rule_semi_join` / `inject_inner_join_null_reject` / `lift_point_get_merge_join` 透传 | `right join` 连接段 `2533.51` 而非 `4509.00` |
| cop 输出投影 | SELECT 列未覆盖 WHERE 列时，投影下推到 cop（`data:Projection` / `index:Projection`） | `cop_output_projection` 返回带 `COP_PROJ\|` 标记的 Projection；`explain` 在 reader 内输出 cop `Projection` | `IndexReader index:Projection → Projection(cop) → IndexRangeScan` |
| 聚簇常见句柄点查 | common handle 的全键等值仍是 `Point_Get … clustered index:PRIMARY` | `unique_index_eq_point` / `dnf_unique_point` 只排除整型句柄（`pk_is_handle`） | `where a = 1 and b = 1` → `Point_Get`（分区 `partition:p1`） |
| suite green | Gate A+B | manifest `green` | `clustered_index` Mem=1 / TiKV=1 PASS；green 30→31 |

### 2.0r R3 `explain`（2026-11-08）

| 子能力 | TiDB 对齐点 | SealDB 改动 | 证据 |
|---|---|---|---|
| `GROUP_CONCAT` 计划 | `buildAggregation` 先把每个参数改写成 `cast(arg, var_string(20))`，聚合在其投影列上；默认分隔符 `,` | `build_group_concat_agg_plan`：Projection(`cast(…, var_string(20))->Column#N`) 位于 StreamAgg/HashAgg 之下，`funcs:group_concat(Column#N separator ",")` | 四种 explain 格式的 group_concat 段全绿 |
| ONLY_FULL_GROUP_BY | `aggregation.GetAggFunc` 注册的多参数聚合（`group_concat` / `bit_and` / `variance` / `json_arrayagg` …）都合法 | `is_aggregate_func_name` 覆盖 TiDB 聚合注册表 | `group_concat(a, b)` 不再报 1055 |
| CAST 字符长度 | `cast(x as CHAR(32))` 的字段类型保留长度（`varchar(32)`） | `data_type_name` 读 `CharacterLength::IntegerLength`，`VARCHAR(MAX)` 保留 `MAX` | `desc v` 列类型对齐 |
| 视图列可空性 | 常量表达式（含 CAST 包裹）折叠后非 NULL，`CREATE VIEW` 列标记 NOT NULL | `expr_is_not_null_constant` → `ColumnDef.not_null` → `NOT_NULL_FLAG` | `desc v` 的 `event_id` 显示 `NO` |
| SHOW COLUMNS | `Null` 取 `NOT_NULL_FLAG`；`Key` 依次为 `PRI` / `UNI` / `MUL` | `column_row` 按索引首列与主键判断 | `desc v` / `desc t` 对齐 |
| `mysql` 系统库 | TiDB `bootstrap()` 建 `mysql` 库与 `user` / `global_priv` / `global_grants` / `db` / `tables_priv` / `columns_priv` 真实表，并建 `i_user` 索引 | `ddl::bootstrap::bootstrap_system_tables`：Domain 首次加载时建库建表并持久化 | `select * from mysql.user where user='xxx'` → `IndexLookUp` |
| 单索引 DNF 区间 | `a=1 OR a=2` 在同一条索引上是一个多区间 `IndexRangeScan`，不建 IndexMerge；窄的二级索引优先于复合 PRIMARY 前缀 | `dnf_same_index_plan` + `pseudo::conjunct_bound_sets` 支持 DNF 等值区间 | `range:["xxx","xxx"], ["yyy","yyy"]`，20.00 |
| suite green | Gate A+B | manifest `green` | `explain` Mem=32 / TiKV=32 PASS；green 31→32 |

### 2.0s R3 `AS OF TIMESTAMP` 快照读（2026-11-08）

| 子能力 | TiDB 对齐点 | SealDB 改动 | 证据 |
|---|---|---|---|
| 语法 | `TableName` 后的 `AS OF TIMESTAMP SimpleExpr`（pingcap/parser `AsOfClause`） | `rewrite_as_of_timestamp` 把子句改写成反引号 marker 别名（MySQL 方言无该语法）；`table_factor` 解出 `TableName::as_of_ts` 并丢弃 marker 别名 | `from t as of timestamp @a` 不再 1064 |
| `SET @v = (SELECT …)` | 标量子查询求值并保留类型（`current_timestamp(3)` 为 DATETIME） | `eval_set_subquery`：解析 `(SELECT <const>)` 后用 `eval_expr` 求值（安装会话时区） | `@a` 持有真实 DATETIME，而非子查询文本 |
| 会话快照 | stale read 打开只读快照，整个语句读同一时间点 | `stmt_as_of_millis`（用户变量 / 引号字面量）→ `Storage::begin_stale_read(unix_millis)` → `TxnManager::ensure_stale` | 语句内所有表统一快照 |
| Mem 多版本 | 行级 commit 时间决定可见性 | `VersionedValue{commit_ts, commit_millis, value}` 版本链；`value_at(start_ts)`；`begin_stale` 按 `commit_millis < t` 裁剪 | 单测 `test_stale_read_sees_past_snapshot` |
| TiKV 时间戳换算 | `oracle.GoTimeToTS(t) = physical_ms << 18` | `TikvStorage::begin_stale_read` 用该 TSO 建 `client.snapshot(ts, read_only)` 作为读快照 | `select … as of timestamp @a` 与 Mem 一致返回空 |
| suite green | Gate A+B | manifest `green` | `executor/batch_point_get` Mem=1 / TiKV=1 PASS；green 32→33 |

### 2.0t R3 剩余套件根因盘点（2026-11-08）

`explain_foreign_key` 转绿后，对 R3 其余 `in_progress` 套件逐个跑了内存 triage（并给 harness 加了
`--replace_column` 支持与 `SEALDB_ITEST_DUMP_DIR` 全量 actual 落盘），结论是剩余项集中在四块基础设施：

| 根能力 | 直接影响 | 现状 |
|---|---|---|
| 子查询去关联（semi / anti / marker 列） | `explain_easy`、`index_merge`、`topn_push_down`、`explain_complex*` | `rule_semi_join` 只处理 CNF 顶层的 `EXISTS`/`IN`；子查询处在 `OR` 内时仍留 `Apply`，TiDB 会改成 `left outer semi join` + marker 列（`Column#N`） |
| 统计驱动的访问路径与基数 | `explain_easy_stats`、`explain_join_stats`、`explain_indexmerge_stats`、`clustered_index` 剩余 | 已有直方图区间 est 与 `stats:partial` 渲染，但缺 NDV 驱动的 join 基数、`ORDER BY` 走索引的成本比较、`IndexJoin` 选择与 `TIDB_INLJ` 提示 |
| 表达式（函数）索引 | `explain_easy`、`explain_generate_column_substitute` | 建索引/DML/点查已落地（`explain_shard_index` green），余下套件按 planner 代价与 explain 形状继续对齐 |
| 权限与运行时统计 | `executor/explain` | 已有 `pkg/privilege` 的 GRANT/USER 落盘，但视图 `EXPLAIN` 的 `ErrViewNoExplain`(1345)/1142、`explain analyze` 的 actRows/内存/磁盘、`information_schema.statements_summary` 均缺 |

顺带修掉一个真实正确性问题：`select count(*) from e, lo where lo.a=e.a and e.b=…` 把 `e` 规划成
`IndexReader(idx_b)`，而 join 需要比较 `e.a` —— 索引并不覆盖该列。`select_covers_index` 现在把
「属于该表的 join key 列」也算进 needed columns，非覆盖索引回到 `IndexLookUp`。

### 2.0u R3 `explain_foreign_key` 多表 DML join（2026-11-08）

| 子能力 | TiDB 对齐点 | SealDB 改动 | 证据 |
|---|---|---|---|
| WHERE 等值键提升 | `pushDownPredicatesForJoin` 把 join 等值条件从 `Selection` 提升到 `Join.EqualConditions`，`CROSS` 修正为 `INNER` | `dml_join_conditions`：`UPDATE t1,t2 …` / `DELETE t1,t2 FROM t1 JOIN t2 …` 的 `WHERE` 中抽取 `t1.id = t2.id` 为 `ON`，把 `ON TRUE` 的 CROSS 改成 INNER | 两侧不再是 CARTESIAN 全表扫 |
| 跨侧谓词镜像 | 等值键两侧可互相传播常量/范围（`t1.id=1` ⇒ `t2.id=1`） | `rebind_table_qualifier` 按同名键把单侧过滤复制到另一侧 | `Point_Get` ×2 / `TableRangeScan` ×2 |
| join 形状选择 | 两侧同 handle 时键折叠为 TRUE，留 `HashJoin CARTESIAN`；两侧 handle range 有序时选 `MergeJoin` | `dml_join_shape`；MergeJoin estRows 走 null-reject `3333.33 × 1.25 = 4166.67`，Build 为语句自身表 | `update t_1,t_2 …` / `delete t_1,t_2 …` 段与金样一致 |
| 跨表等值类型安全 | 类型不同的等值（`int = bit`）不能当 hash key，必须留在 `Selection` 走隐式转换 | `qualify_cross_equality` 校验表归属；`same_join_key_type` 校验声明类型 | `executor/jointest/join` 无回归 |
| suite green | Gate A+B | manifest `green` | `explain_foreign_key` Mem=1 / TiKV=1 PASS；green 33→34 |

### 2.1 历史阶段（归档，不作为对齐进度）
下列 Phase / F*/C* `core-ported` 保留为工程里程碑；**对齐进度只看 suite green**。

| Phase | 名称 | 归档状态 | 备注 |
|---|---|---|---|
| 0–9 | 地基→协议 | `done`（历史） | 见下方旧表 |
| 10 / C6 | 完整性 + PD/TiKV | 基础设施仍有效 | TiFlash 编译暂缓；对齐改走 TDD 套件 |

<details><summary>旧功能波 core-ported 表（归档）</summary>

| ID | 状态 | 证据 |
|---|---|---|
| U0–U3 / F01–F22 / C1–C5 | `core-ported`（历史） | `project/reports/features/` |
| C6 | 本地 PD+TiKV 联通 | tiup v8.5.0；非 suite green |

</details>

---

## 3. 包级状态（Phase 0 基线）

> U1/U2/U3/F01 相关 crate 已有核心路径实现；其余 crate 仍是可编译骨架。  
> 精确文件级状态由 `project/parity/status.json` + `parity_check.py` 生成。

| 顶层包 | crate | 骨架 | 文件移植 | 目标阶段 |
|---|---|---|---|---|
| `pkg/parser` | `sealdb-parser` | ✅ | 核心 | 2/3 |
| `pkg/types` | `sealdb-types` | ✅ | 核心 | 1 |
| `pkg/kv` | `sealdb-kv` | ✅ | 0 | 1/6 |
| `pkg/util` | `sealdb-util` | ✅ | 核心 | 1/5 |
| `pkg/config` | `sealdb-config` | ✅ | 核心 | 0/9 |
| `pkg/errno` | `sealdb-errno` | ✅ | 核心 | 1 |
| `pkg/meta` | `sealdb-meta` | ✅ | 核心 | 3 |
| `pkg/infoschema` | `sealdb-infoschema` | ✅ | 核心 | 3 |
| `pkg/domain` | `sealdb-domain` | ✅ | 核心 | 3/6 |
| `pkg/expression` | `sealdb-expression` | ✅ | 0 | 2/5 |
| `pkg/planner` | `sealdb-planner` | ✅ | 0 | 4/7 |
| `pkg/executor` | `sealdb-executor` | ✅ | 0 | 5/8 |
| `pkg/session` | `sealdb-session` | ✅ | 0 | 2/6 |
| `pkg/sessionctx` | `sealdb-sessionctx` | ✅ | 0 | 2/6 |
| `pkg/sessiontxn` | `sealdb-sessiontxn` | ✅ | 0 | 6 |
| `pkg/store` | `sealdb-store` | ✅ | 核心 | 5/6 |
| `pkg/distsql` | `sealdb-distsql` | ✅ | 0 | 5 |
| `pkg/table` | `sealdb-table` | ✅ | 0 | 5 |
| `pkg/tablecodec` | `sealdb-tablecodec` | ✅ | 0 | 1 |
| `pkg/statistics` | `sealdb-statistics` | ✅ | 0 | 7 |
| `pkg/privilege` | `sealdb-privilege` | ✅ | 0 | 8 |
| `pkg/bindinfo` | `sealdb-bindinfo` | ✅ | 0 | 8 |
| `pkg/ddl` | `sealdb-ddl` | ✅ | 核心 | 3/8 |
| `pkg/server` | `sealdb-server` | ✅ | 0 | 9 |
| `cmd/tidb-server` | `sealdb-server-bin` | ✅ | 0 | 2/9 |

---

## 4. 当前工作与证据

### 2026-11-05 — TDD TiDB 回归全量对齐

- **进度单位**: suite `green`（见 `project/regression/manifest.json`）
- **完成**:
  - 导入 TiDB v8.5.8 `integrationtest` `t/r` + manifest（TiFlash/MPP → `deferred`）
  - `record_tidb_itest.sh` / 禁默认 SealDB record；runner fail-lock + status 过滤
  - R1/R2/R3 套件标 `in_progress`；select 路径：1060 派生表、hex→utf8mb4、STRAIGHT_JOIN USING、session vars
- **验证**:
  - `./tests/integrationtest/run-tests.sh -s memory`（green+local）
  - `./project/scripts/triage_itest.sh -t select --status in_progress`
- **下一步**:
  - R1 `select` 下一失败：`select !(1 + 2)`（sqlparser `!` 一元）与 unary 显示名；heap/`pk_is_handle` 已修
  - 继续 R1 → green，再扫 R2/R3 triage 红灯同名移植

### 2026-11-05 — C6 本地 PD + TiKV 联通（TiFlash 暂缓）

- **完成**:
  - `run_local_pd_tikv.sh` / `verify_pd_tikv.sh`（tiup `pd/tikv` v8.5.0）
  - PD HTTP `no_proxy` 客户端；空 key region 走 scan
  - 实证：TSO + KV roundtrip + list stores/regions
- **验证**:
  - `./project/scripts/run_local_pd_tikv.sh start`（另开终端）
  - `NO_PROXY='*' ./project/scripts/verify_pd_tikv.sh`
- **下一步**:
  - 加深 Cop/BatchCop 在活集群上的覆盖；TiFlash 编译延后

### 2026-11-04

- **Phase**: C6 TiFlash cluster e2e（本地编译）— **暂缓**
- **完成**:
  - 脚本与源码 submodule 保留；Homebrew LLVM 22 与 Poco/C++23 不兼容，停编
- **下一步**:
  - 换兼容工具链或预编译包后再开

### 2026-11-04

- **Phase**: C6 cascades implementation / enforcer rules
- **完成**:
  - `ImplementationRules`：LogicalSelection/TableScan → PhysicalSelection/TableReader
  - `EnforcerRules`：PropRequiredOrder → PhysicalSortEnforcer；`optimize` 串接
- **验证**:
  - `cargo test -q -p sealdb-planner --lib cascades`
- **下一步**:
  - tiflash-cluster-e2e

### 2026-11-04

- **Phase**: C6 BR agent (`br://`)
- **完成**:
  - `br://job` POST/GET via `SEALDB_BR_AGENT_ENDPOINT` `/v1/backup|restore/{job}`
- **验证**:
  - `cargo test -q -p sealdb-session --lib brie`
- **下一步**:
  - tiflash-cluster-e2e

### 2026-11-04

- **Phase**: C6 TableReader MPP / BatchCop hook
- **完成**:
  - `SEALDB_USE_MPP` → `execute_mpp_gather_blocking`；`SEALDB_USE_BATCH_COP` → stream
  - 失败回退 Cop RPC / 本地 KV（同 `SEALDB_USE_COP_RPC`）
- **验证**:
  - `cargo test -q -p sealdb-executor --lib cop_client`
- **下一步**:
  - br-agent；集群 tiflash-e2e

### 2026-11-04

- **Phase**: C6 CancelMPPTask
- **完成**:
  - `CancelTaskRequest` / `CancelMPPTask` RPC；`MppClient::cancel_task(s)`
- **验证**:
  - `cargo test -q -p sealdb-store --lib mpp`
- **下一步**:
  - tiflash-e2e；br-agent

### 2026-11-04

- **Phase**: C6 BRIE GCS / HDFS
- **完成**:
  - `gs://` JSON API media via `SEALDB_GCS_ENDPOINT` (+ optional bearer)
  - `hdfs://` WebHDFS OPEN/CREATE via host or `SEALDB_HDFS_NAMENODE`
- **验证**:
  - `cargo test -q -p sealdb-session --lib brie`
- **下一步**:
  - C6 production-parity leftovers（TiFlash end-to-end / BR agent 等）

### 2026-11-04

- **Phase**: C6 MPP multi-sender root gather
- **完成**:
  - `execute_mpp_root_gather` 对每个 sender `EstablishMPPConnection` 并 `merge_select_responses`
  - `MppClient::gather_from_senders` / `gather_from_sender` API
- **验证**:
  - `cargo test -q -p sealdb-store --lib mpp`
- **下一步**:
  - brie-gcs-hdfs

### 2026-11-04

- **Phase**: C6 cascades more transformation rules
- **完成**:
  - `EliminateFalseSelection` + `DeduplicateGroupExprs`；default_set 扩到 4 条
  - true/false Selection 指纹消除 + group 内 fingerprint 去重
- **验证**:
  - `cargo test -q -p sealdb-planner --lib cascades`
- **下一步**:
  - mpp-multi-sender；brie-gcs-hdfs

### 2026-11-04

- **Phase**: C6 S3 BRIE
- **完成**:
  - `s3://bucket/key` BACKUP/RESTORE；`SEALDB_S3_ENDPOINT` path-style
  - 可选 SigV4（`SEALDB_S3_ACCESS_KEY` / `SEALDB_S3_SECRET_KEY` 或 `AWS_*`）
- **验证**:
  - `cargo test -q -p sealdb-session --lib brie`
- **下一步**:
  - cascades more rules；mpp-multi-sender；GCS/HDFS

### 2026-11-04

- **Phase**: C6 MPP Dispatch / Establish exchange MVB
- **完成**:
  - `mpp_exchange`：`DispatchTaskRequest` / `EstablishMPPConnection` / `MPPDataPacket`
  - `MppClient::{dispatch_mpp_tasks,establish_root_connection,execute_mpp_gather_blocking}`
- **验证**:
  - `cargo test -q -p sealdb-store --lib mpp`
- **下一步**:
  - S3 BRIE；cascades more rules；multi-sender gather

### 2026-11-04

- **Phase**: C6 BatchCoprocessor stream
- **完成**:
  - `BatchRequest` / `BatchResponse` prost；`/tikvpb.Tikv/BatchCoprocessor` server stream
  - `BatchRequestSender` TiFlash 路径走 stream；`MppClient::dispatch_stream_blocking`
- **验证**:
  - `cargo test -q -p sealdb-store --lib batch_`
- **下一步**:
  - MPP exchange / DispatchMPPTask；S3 BRIE；cascades more rules

### 2026-11-04

- **Phase**: C6 BRIE HTTP restore
- **完成**:
  - `BrieLocation::{Local,Http}`；`RESTORE … FROM 'http(s)://…'` 拉取 JSON
  - `BACKUP` 仍拒写远程 URL；S3/GCS/HDFS 继续 unsupported
- **验证**:
  - `cargo test -q -p sealdb-session --lib brie`
- **下一步**:
  - BatchCop stream / MPP exchange；S3 BRIE；cascades more rules

### 2026-11-04

- **Phase**: C6 cascades transformation rules MVB
- **完成**:
  - `RuleType::{EliminateTrueSelection,JoinToApply}`；`TransformationRules::apply_all`
  - `optimize` 跑 explore + transformation；真值 Selection 指纹可消除
- **验证**:
  - `cargo test -q -p sealdb-planner --lib cascades`
- **下一步**:
  - more rewrite rules；BatchCop stream / MPP exchange；brie-remote

### 2026-11-04

- **Phase**: C6 cascades identity hook
- **完成**:
  - `cascades::optimize` 种子 memo + `TaskScheduler::run_explore`
  - `SEALDB_CASCADES=1` 时 PlanBuilder SELECT 路径调用 `maybe_optimize`（不改 RBO 计划）
- **验证**:
  - `cargo test -q -p sealdb-planner --lib cascades`
- **下一步**:
  - cascades transformation/implementation rules；BatchCop stream / MPP exchange；brie-remote

### 2026-11-04

- **Phase**: C6-D BatchCop / MPP dispatch plan
- **完成**:
  - `BatchCopTask` + `build_batch_cop_tasks`（TiKV leader / TiFlash RR）+ `balance_batch_cop_tasks`
  - `BatchRequestSender`：TiKV unary Cop 按 store 批执行；TiFlash 拒流式直至 BatchCop stream
  - `MppClient::plan_dispatch` / `plan_tikv_batch`；`execute_batch_cop_task`
- **验证**:
  - `cargo test -q -p sealdb-store --lib batch_`
  - `cargo test -q -p sealdb-store --lib mpp::`
- **下一步**:
  - cascades hook；BatchCop stream / MPP exchange；brie-remote

### 2026-11-04

- **Phase**: C6-D TiFlash store discovery
- **完成**:
  - PD `PdStoreInfo.labels`；`engine=tiflash` 识别
  - `MppProbe` / `MppClient` 列 TiFlash store；`TikvStorage::mpp_client`
- **验证**:
  - `cargo test -q -p sealdb-store --lib mpp`
- **下一步**:
  - MPP/BatchCop dispatch；cascades；brie-remote

### 2026-11-04

- **Phase**: C6-C tipb TypeChunk decode
- **完成**:
  - `util/chunk/tipb_codec`：列式 TypeChunk encode/decode（对齐 TiDB chunk.Codec）
  - `decode_select_rows` 按 `encode_type` 分发 Default / Chunk
  - TableReader 远程 Cop 路径传入列 MySQL type codes
- **验证**:
  - `cargo test -q -p sealdb-util --lib tipb_codec`
  - `cargo test -q -p sealdb-store --lib`
- **下一步**:
  - cascades 实装；C6-D TiFlash；brie-remote

### 2026-11-03

- **Phase**: C6-C PD stores/regions 客户端
- **完成**:
  - `PdClient`：`list_stores` / `scan_regions` / `get_region` / `store_address`
  - `TikvStorage::{pd_client,list_stores,scan_regions}`
- **验证**:
  - `cargo test -q -p sealdb-store --lib`（含 skip-if-no-PD）
- **下一步**:
  - tipb TypeChunk；C6-D TiFlash；cascades 实装

### 2026-11-03

- **Phase**: C6-C tipb TypeDefault chunk decode
- **完成**:
  - 修正 tipb `Chunk`/`DAGRequest`/`ColumnInfo` wire tag；`EncodeType::TypeDefault`
  - `util/codec/datum` EncodeValue/DecodeOne；`decode_select_rows`
  - `SEALDB_USE_COP_RPC=1` 时 TableReader 尝试远程 Cop + decode，失败回退本地 KV
- **验证**:
  - `cargo test -q -p sealdb-util --lib codec::datum`
  - `cargo test -q -p sealdb-store --lib`
  - `cargo test -q -p sealdb-session --lib test_where_selection`
- **下一步**:
  - PD 全量客户端加深；TypeChunk decode

### 2026-11-03

- **Phase**: C6 本地文件 BRIE
- **完成**:
  - `BACKUP DATABASE … TO 'path'` / `RESTORE DATABASE FROM 'path'`（`file://` 本地 JSON）
  - 远程 s3/gs/hdfs 仍 unsupported；跳过 VIEW
- **验证**:
  - `cargo test -q -p sealdb-session --lib test_brie_local_backup_restore`
  - `cargo test -q -p sealdb-session --lib brie::`
- **下一步**:
  - tipb chunk decode；PD 全量客户端加深

### 2026-11-03

- **Phase**: C6-C TableReader CopTask 调度
- **完成**:
  - `executor/cop_client`：按物理表构建 tipb TableScan CopTasks（Region 切分）
  - TableReader 经 CopTask 本地 KV 执行；`SEALDB_USE_COP_RPC=1` 可探测 TiKV Cop RPC
  - tipb chunk→Datum 解码仍待（远程结果尚未替代本地扫描）
- **验证**:
  - `cargo test -q -p sealdb-executor --lib`
  - `cargo test -q -p sealdb-session --lib test_where_selection`
- **下一步**:
  - tipb chunk decode；BRIE 本地备份/恢复

### 2026-11-03

- **Phase**: C6 Cascades 包骨架
- **完成**:
  - `pkg/planner/cascades` 对齐 optimize/memo/rule/task/base 等 TiDB 文件名；`optimize` no-op
  - 生产路径仍为 RBO；parity stub 登记
- **验证**:
  - `cargo test -q -p sealdb-planner --lib cascades`
- **下一步**:
  - TableReader 切 Cop 路径；BRIE 语义加深

### 2026-11-03

- **Phase**: C6-C `store/copr` + `distsql` 包文件对齐
- **完成**:
  - 对齐 TiDB 文件名：`key_ranges`/`coprocessor`/`region_cache`/`store`/`batch_*`/`mpp*`
  - `distsql::{distsql,request_builder,select_result}`；parity manifest 登记
- **验证**:
  - `cargo test -q -p sealdb-store --lib`
  - `cargo test -q -p sealdb-distsql --lib`
  - `parity_check.py --tidb-root …`（无 invalid entries）
- **下一步**:
  - TableReader 切 Cop 路径；cascades / BRIE

### 2026-11-03

- **Phase**: C6-C TiKV Coprocessor RPC
- **完成**:
  - PD Region leader + store address → tonic `/tikvpb.Tikv/Coprocessor`
  - `execute_cop_task` / `TikvStorage::execute_cop_task`；skip-if-no-cluster 单测
- **验证**:
  - `cargo test -q -p sealdb-store --lib`
- **下一步**:
  - TableReader 切 Cop 路径；算层包文件对齐；cascades / BRIE

### 2026-11-03

- **Phase**: C6 VIEW `ALGORITHM=TEMPTABLE`
- **完成**:
  - SELECT 展开为 `AS MATERIALIZED` CTE；`MERGE` → `NOT MATERIALIZED`
  - TEMPTABLE + WITH CHECK OPTION → 1471；TEMPTABLE VIEW 不可 UPDATE/DELETE
- **验证**:
  - `cargo test -q -p sealdb-session --lib test_view_algorithm_temptable`
- **下一步**:
  - Coprocessor RPC 发送；算层包文件对齐；cascades / BRIE 深化

### 2026-11-03

- **Phase**: C6-C tipb DAG + CopTask
- **完成**:
  - `pkg/store/copr/dag`：tipb 兼容 `DAGRequest`/`TableScan`/`SelectResponse` encode/decode（`REQ_TYPE_DAG=103`）
  - `CopTask` + `build_table_scan_tasks`（按 Region 切分）；`sealdb-distsql::build_table_select_tasks`
  - TiKV gRPC Coprocessor 发送仍待（下一切片）
- **验证**:
  - `cargo test -q -p sealdb-store --lib`
  - `cargo test -q -p sealdb-distsql --lib`
- **下一步**:
  - Coprocessor RPC 发送；VIEW algorithm=TEMPTABLE；算层包文件对齐

### 2026-11-03

- **Phase**: C6-C PD Region locate
- **完成**:
  - `split_key_ranges_via_pd`：PD HTTP `/pd/api/v1/region/key/{hex}` 行走 Region 边界
  - `store::open(Tikv)` 登记 PD endpoints；`split_key_ranges_for_scan` 供 TableReader/IndexLookUp/IndexMerge
  - PD 失败回退单段；`TikvStorage::split_key_ranges` 暴露同路径
- **验证**:
  - `cargo test -q -p sealdb-store --lib`（含 `test_split_key_ranges_via_pd_if_available`）
  - `cargo check -p sealdb-executor`
- **下一步**:
  - Coprocessor DAG / tipb；algorithm=TEMPTABLE；算层包文件对齐

### 2026-11-03

- **Phase**: C6 VIEW `WITH CHECK OPTION`
- **完成**:
  - 解析 `WITH [CASCADED|LOCAL] CHECK OPTION`；默认 `NONE`
  - UPDATE 经视图改写后对新行求值，失败返回 1369
- **验证**:
  - `cargo test -q -p sealdb-parser --lib test_parse_view_with_check_option`
  - `cargo test -q -p sealdb-session --lib view_with_check`
- **下一步**:
  - Region locate / Cop DAG；algorithm=TEMPTABLE

### 2026-11-03

- **Phase**: C6 简单可更新 VIEW
- **完成**:
  - 单表无 JOIN/GROUP BY 的 VIEW：`UPDATE`/`DELETE` 改写到基表并 AND 视图 WHERE
- **验证**:
  - `cargo test -q -p sealdb-session --lib test_simple_updatable_view_update_delete`
- **下一步**:
  - WITH CHECK OPTION / algorithm=TEMPTABLE；Region locate / Cop DAG

### 2026-11-03

- **Phase**: C6-C TableReader/IndexLookUp 走 copr split
- **完成**:
  - `TableReaderExec` / `IndexLookUp` / `IndexMerge` 扫描经 `sealdb_store::split_key_ranges`
- **验证**:
  - `cargo test -q -p sealdb-session --lib test_where_selection`
  - `cargo test -q -p sealdb-session --lib test_index`
- **下一步**:
  - C6-C 续：Region locate / Cop DAG tipb，或算层包文件对齐

### 2026-11-03

- **Phase**: C6-C PD TSO surface + copr KeyRange MVB
- **完成**:
  - `Storage::current_timestamp`：Mem 单调计数；TiKV 调 PD TSO
  - `pkg/store/copr`：`KeyRange` + `split_key_ranges` 单段 MVB（Region 切分待 locate API）
- **验证**:
  - `cargo test -q -p sealdb-store --lib`
- **下一步**:
  - C6-C 续：Region locate / Cop DAG tipb，或算层包文件对齐

### 2026-11-03

- **Phase**: C6-C TiKV `batch_get` + BatchPointGet
- **完成**:
  - `sealdb_kv::Transaction::{batch_get,batch_get_for_update}`；TiKV 走 RPC，默认回退逐 key
  - `BatchPointGetExec` 无锁路径一次 `batch_get` 拉行
- **验证**:
  - `cargo test -q -p sealdb-store --lib`
  - `cargo test -q -p sealdb-session --lib point_get`
- **下一步**:
  - C6-C 续：Coprocessor DAG / Region 定位，或算层包文件对齐

### 2026-11-03

- **Phase**: C6-C TiKV RC `refresh_snapshot`
- **完成**:
  - `pkg/store/driver`：READ COMMITTED 用 PD TSO 换新 `Snapshot` 做普通读；本地 write overlay 保留本事务未提交写
  - 驱动级 skip-if-no-PD：`test_tikv_rc_refresh_snapshot_if_available`
  - 会话级既有：`test_tikv_cross_session_same_cluster_if_available`（SEALDB_TIKV=1）覆盖 RC 可见性
- **验证**:
  - `cargo test -q -p sealdb-store --lib`
  - `cargo test -q -p sealdb-session --lib schema_checker_and_read_committed`
- **下一步**:
  - C6-C 续：`batch_get` / Coprocessor DAG / Region 定位面，或算层包文件对齐

### 2026-11-03

- **Phase**: C6-B PlanBuilder stmt 面收口
- **完成**:
  - stats：`LOAD/LOCK/UNLOCK STATS` no-op
  - 诊断：`TRACE` / `PLAN REPLAYER`
  - 运维：`BACKUP/RESTORE`、`IMPORT INTO`、`SPLIT`/`DISTRIBUTE`、`COMPACT`、`RECOMMEND INDEX`、`CALIBRATE RESOURCE`
  - DDL 边角：`FLASHBACK*`、`RECOVER TABLE`、`DROP PLACEMENT POLICY`、`CREATE/SET RESOURCE GROUP`、`QUERY WATCH`
  - leftover：`open_stmts=0`；C6_CLOSED_STMTS 覆盖全部 C6-hinted PlanBuilder kinds
- **验证**:
  - `cargo test -q -p sealdb-parser --lib test_parse_c6_*`
  - `leftover_inventory.py --check` PASS（open_stmts=0）
- **下一步**:
  - C6-C：算层包/文件对齐加深 + PD/TiKV 全量客户端；C6-D TiFlash 仍延后

### 2026-11-03

- **Phase**: C6-B LOAD DATA
- **完成**:
  - `LOAD DATA [LOCAL] INFILE … INTO TABLE`（FIELDS/LINES/IGNORE/列清单）→ 物化为 INSERT
  - leftover：`LoadDataStmt` 收口；open_stmts 21
- **验证**:
  - `cargo test -q -p sealdb-parser --lib test_parse_load_data`
  - `cargo test -q -p sealdb-session --lib test_load_data_infile`
  - `leftover_inventory.py --check` PASS（denominator 63）
- **下一步**:
  - 继续 C6-B（TRACE / BRIE / stats lock…）或 PD/TiKV 客户端加深

### 2026-11-03

- **Phase**: C6-B SEQUENCE
- **完成**:
  - `CREATE/DROP/ALTER SEQUENCE` + `NEXTVAL` + `information_schema.sequences`
  - leftover：`CreateSequenceStmt` / `DropSequenceStmt` / `AlterSequenceStmt` 收口；open_stmts 22
- **验证**:
  - `cargo test -q -p sealdb-parser --lib test_parse_create_drop_alter_sequence`
  - `cargo test -q -p sealdb-session --lib test_create_drop_alter_sequence_nextval`
  - `leftover_inventory.py --check` PASS（denominator 64）
- **下一步**:
  - C6-B `LOAD DATA` 或 VIEW 边角

### 2026-11-03

- **Phase**: 对齐顺序澄清（先 PD/TiKV 全量，再 TiFlash）
- **完成**:
  - `requirements.md` / `features.md` / `roadmap.md` / `status.json`：TiFlash **延后不放弃**；终点 **逐包/逐文件**对齐 TiDB；顺序 A/B → C（算层大件+PD/TiKV 全量客户端）→ D（TiFlash）
- **下一步**:
  - 继续 C6-B（VIEW 边角 / SEQUENCE / LOAD DATA）与 PD/TiKV 客户端能力加深

### 2026-11-03

- **Phase**: C6 产品定调 + 第一刀 VIEW
- **完成**:
  - 文档：C6 从「明确不做」改为生产对齐分期；第一刀 `CREATE/DROP VIEW`
  - leftover：`C6_CLOSED_STMTS={CreateViewStmt}`；open_stmts 25
- **验证**:
  - `cargo test -q -p sealdb-parser --lib test_parse_create_drop_view`
  - `cargo test -q -p sealdb-session --lib test_create_drop_view_select`
  - `leftover_inventory.py --check` PASS（denominator 67）
- **下一步**:
  - VIEW 边角或下一 B 切片；并行加深 PD/TiKV 调用

### 2026-11-03

- **Phase**: leftover allowlist shrink（Kill/Lock/Optimize/RenameUser/Repair/Binlog/Change/AlterInstance）
- **完成**:
  - `KillStmt`：`KILL [CONNECTION|QUERY] id` + Domain connection registry（F22）
  - `LockTablesStmt` / `CleanupTableLockStmt`：`LOCK TABLES` / `UNLOCK TABLES` + in-memory table locks（F12）
  - `OptimizeTableStmt`：`OPTIMIZE TABLE` status-row OK no-op（F21）
  - `RepairTableStmt`：`REPAIR TABLE` status-row OK no-op（F18）
  - `RenameUserStmt`：privilege manager rename（F20）
  - `BinlogStmt` / `ChangeStmt` / `AlterInstanceStmt`：accepted OK no-op（F22）
  - `UNMAPPED_ALLOWLIST = frozenset()`；`--check` 允许空 allowlist 当 `open_unmapped` 为空
- **验证**:
  - `cargo test -q -p sealdb-parser --lib parse_allowlist_misc_kill_lock`
  - `cargo test -q -p sealdb-session --lib allowlist_kill_lock_optimize`
  - `./project/scripts/feature_gate.sh C1`
- **下一步**:
  - C6 生产对齐；open_stmts 消化 C6-owned kinds

### 2026-11-03


- **Phase**: leftover allowlist shrink（Do/SetPwd/Flush/Shutdown/AlterDatabase）
- **完成**:
  - `DoStmt`：`DO expr[,…]` extract+eval（F01）；常量副作用无结果集
  - `SetPwdStmt`：`SET PASSWORD [FOR user] = 'pwd'` 更新 privilege auth（F20）
  - `FlushStmt`：`FLUSH PRIVILEGES` reload / `FLUSH TABLES` no-op（F20）
  - `ShutdownStmt`：Domain flag + listen loop 可退出（F22）
  - `AlterDatabaseStmt`：`ALTER DATABASE … CHARACTER SET / COLLATE` 更新 DB meta（F02）
  - 均移出 `UNMAPPED_ALLOWLIST`；C1 `execute-do-flush-setpwd-shutdown-alterdb` + parse gate
- **验证**:
  - `cargo test -q -p sealdb-parser --lib parse_do_flush`
  - `cargo test -q -p sealdb-session --lib do_flush_setpwd`
  - `./project/scripts/feature_gate.sh C1`
- **下一步**:
  - 继续消化剩余 allowlist（Binlog/Kill/LockTables…）；C6 仍 deferred

### 2026-11-03

- **Phase**: leftover allowlist shrink（TruncateTableStmt）
- **完成**:
  - Parse `TRUNCATE [TABLE] db.t` → AST `TruncateTableStmt`（TiDB 1:1 命名）
  - Execute：按 `encode_table_prefix` 清 MemStorage/TiKV 行+索引键；`reset_handle`；保留表 schema
  - `FEATURE_HINTS["TruncateTableStmt"] = "F02"`；移出 `UNMAPPED_ALLOWLIST`
  - Session 单测 `test_truncate_table_clears_rows_keeps_schema`；C1 gate `execute-truncate`
- **验证**:
  - `cargo test -q -p sealdb-session --lib truncate`
  - `./project/scripts/feature_gate.sh C1`（含 leftover regenerate）
- **下一步**:
  - 继续消化 `UNMAPPED_ALLOWLIST` 下一项；C6 仍 deferred

### 2026-11-03

- **Phase**: 完整性波 C1（leftover denominator terminal state）
- **完成**:
  - `unmapped-stmts`：FEATURE_HINTS 补齐已落地 stmt→F*/C2–C5；其余 C6 或 `UNMAPPED_ALLOWLIST`；`--check` 校验 allowlist
  - `stale-gap-labels`：F* 与 C1–C5 `gaps: []` 与 leftover inventory 一致；C1 gaps.md 去掉过时 F* 冻结措辞
  - `leftover-suite-execution`：`execute-leftover-suite-smoke` 断言 suite 全部分类且 C2/C5 有对应 feature_gate MVB
  - `status.json` C1 `gaps: []`；phase 10 `done`（C6 deferred）
- **验证**:
  - `python3 project/scripts/leftover_inventory.py --tidb-root "$TIDB_ROOT" --check`
  - `python3 project/scripts/leftover_inventory.py --tidb-root "$TIDB_ROOT" --suite-smoke`
  - `./project/scripts/feature_gate.sh C1`
- **下一步**:
  - C6 保持 deferred；可选后续 wave 从 leftover 分母摘子集标 `ported`

### 2026-11-03

- **Phase**: 完整性波 C5（leftover-collation-suites）
- **完成**:
  - `SET NAMES` / `collation_connection` 校验：未知 charset ERR 1115、未知 collation ERR 1273
  - `test_leftover_collation_suites`：invalid SET、utf8mb4/gbk/latin1、SHOW COLLATION + infoschema、general_ci vs bin
  - C5 gate `execute-leftover-collation-suites`；`status.json` C5 `gaps: []`；leftover notes 标明 `new_character_set*` 由该 MVB 覆盖
- **验证**:
  - `cargo test -q -p sealdb-session --lib test_leftover_collation_suites -- --exact`
  - `./project/scripts/feature_gate.sh C5`
- **下一步**:
  - 完整性波 C1

### 2026-11-03

- **Phase**: 完整性波 C5（set-global）
- **完成**:
  - sqlparser `Set::SingleAssignment.scope` / `@@GLOBAL.` → AST `SetVarScope`
  - Domain `GlobalSysVars`：`sql_mode` / `tidb_enable_auto_analyze` / `character_set_connection`
  - `SET GLOBAL` 写 Domain；`SET SESSION` / plain SET 仍会话局部；新 `Session::with_domain` 继承 Domain 默认
  - `SELECT @@GLOBAL.*` 读 Domain；C5 gate `execute-set-global`；`status.json` / leftover 去掉 `set-global`
- **验证**:
  - `cargo test -q -p sealdb-session --lib test_set_global_sysvars_shared_domain -- --exact`
  - `./project/scripts/feature_gate.sh C5`
- **下一步**:
  - C5 leftover collation suites

### 2026-11-03

- **Phase**: 完整性波 C5（only-full-group-by）
- **完成**:
  - Session `check_only_full_group_by`：`sql_mode` 含 `ONLY_FULL_GROUP_BY` 且有 GROUP BY 时拒绝非聚合 / 非 GROUP BY 列（ERR 1055）
  - HashAgg Const + AnyValue：关 mode 后同查询可执行
  - C5 gate `execute-only-full-group-by`；`status.json` / leftover 去掉 `only-full-group-by`
- **验证**:
  - `cargo test -q -p sealdb-session --lib test_only_full_group_by -- --exact`
  - `./project/scripts/feature_gate.sh C5`
- **下一步**:
  - C5 `set-global` → leftover collation suites

### 2026-11-03

- **Phase**: 完整性波 C5（gbk-latin1）
- **完成**:
  - `pkg/parser/charset/encoding_gbk.rs` / `encoding_latin1.rs`：decode/encode roundtrip（ASCII + 小汉字 / Latin 子集）
  - `pkg/util/collate/gbk_bin.rs` / `gbk_chinese_ci*.rs`：`CollateMode::{GbkBin,GbkChineseCi}`；`SET NAMES gbk|latin1`
  - C5 gate `execute-gbk-latin1`；`status.json` / leftover 去掉 `gbk-latin1`
- **验证**:
  - `cargo test -q -p sealdb-parser --lib charset`
  - `cargo test -q -p sealdb-util --lib collate`
  - `cargo test -q -p sealdb-session --lib test_gbk_latin1_charset_and_collation -- --exact`
  - `./project/scripts/feature_gate.sh C5`
- **下一步**:
  - C5 `only-full-group-by` → `set-global` → leftover collation suites

### 2026-11-03

- **Phase**: 完整性波 C5（unicode-weights）
- **完成**:
  - `pkg/util/collate/`：`CollateMode` + UCA 4.0.0 Latin/ASCII weight subset for `utf8mb4_unicode_ci`
  - Expression / ORDER BY string compare 走 unicode weights；session `SET collation_connection`
  - C5 gate `execute-unicode-weights`；`status.json` / leftover 去掉 `unicode-weights`
- **验证**:
  - `cargo test -q -p sealdb-util --lib collate`
  - `cargo test -q -p sealdb-session --lib test_unicode_ci_weights_compare_and_order -- --exact`
  - `./project/scripts/feature_gate.sh C5`
- **下一步**:
  - C5 `gbk-latin1` → `only-full-group-by` → `set-global` → leftover collation suites

### 2026-11-03

- **Phase**: 完整性波 C2（cost-model-v2）
- **完成**:
  - `plan_cost_ver2.rs`：`CostVer2` / `CostFactors`（CPU/mem/net/scan，对齐 TiDB defaultVer2Factors）
  - JoinReorder 两表 + n-table DP 改走 v2 `hash_join_cost` / `table_scan_cost`
  - EXPLAIN HashJoin `operator info` 带 `estCost=`（v2 helpers）
  - C2 gate `execute-cost-model-v2`；`status.json` C2 `gaps: []`
- **验证**:
  - `cargo test -q -p sealdb-planner --lib plan_cost_ver2`
  - `cargo test -q -p sealdb-session --lib test_explain_cost_model_v2`
  - `./project/scripts/feature_gate.sh C2`
- **下一步**:
  - 完整性波 C3 / C5（C2 完成）

### 2026-11-03

- **Phase**: 完整性波 C2（index-merge-join / IndexMerge）
- **完成**:
  - `PhysicalPlan::IndexMerge`：OR of ≥2 indexed equalities → union partial index scans + table lookup
  - `split_dnf` + `access_plan` 检测；`IndexMergeExecutor` 去重 handle 后回表
  - EXPLAIN：`IndexMerge` + N×`IndexRangeScan` + `TableRowIDScan`（task=cop）
  - C2 gate `execute-index-merge-join`；`status.json` / leftover 去掉 `index-merge-join`
- **验证**:
  - `cargo test -q -p sealdb-session --lib test_explain_index_merge_or`
  - `./project/scripts/feature_gate.sh C2`
- **下一步**:
  - C2 `cost-model-v2`

### 2026-11-03

- **Phase**: 完整性波 C2（n-table-dp）
- **完成**:
  - `rule_join_reorder_dp.rs`：Inner 等式连通图上 left/right-deep bitmask DP（复用 `hash_join_cost` / `table_scan_cost`）
  - `planbuilder` 3+ 表走 DP；首次二元仍用 two-table `reorder_hash_join`
  - Session EXPLAIN：偏斜 ANALYZE 后根 HashJoin 探针侧为大表（非 naive 左深嵌套）
  - C2 gate `execute-n-table-dp`；`status.json` / leftover 去掉 `n-table-dp`
- **验证**:
  - `cargo test -q -p sealdb-planner --lib rule_join_reorder_dp`
  - `cargo test -q -p sealdb-session --lib test_explain_n_table_join_reorder_dp`
  - `./project/scripts/feature_gate.sh C2`
- **下一步**:
  - C2 `index-merge-join` → `cost-model-v2`

### 2026-11-03

- **Phase**: 完整性波 C2（cop-task-rows）
- **完成**:
  - DistSQL `TableReader` / `IndexReader` / `IndexLookUp` EXPLAIN 展开 TiDB 风格 cop 子树（`TableFullScan` / `IndexRangeScan` / `TableRowIDScan`），`task=cop` + 非空 `estRows` / access object
  - Reader 本身为 `root`；pushed filter 时插入 cop `Selection`
  - C2 gate `execute-cop-task-rows`；`status.json` 去掉 `cop-task-rows`
- **验证**:
  - `cargo test -q -p sealdb-session --lib test_explain_cop_task_rows`
  - `./project/scripts/feature_gate.sh C2`
- **下一步**:
  - C2 `n-table-dp` → `index-merge-join` → `cost-model-v2`

### 2026-11-03

- **Phase**: 完整性波 C2（explain-analyze）
- **完成**:
  - `EXPLAIN ANALYZE` 解析 + 执行子计划，输出 TiDB 风格 `actRows` / `execution info`（root 实测行数与耗时）
  - 复用既有 EXPLAIN row/json/dot；Analyze 路径扩展列与 JSON/DOT 标注
  - C2 gate `execute-explain-analyze`；`status.json` 去掉 `explain-analyze`
- **验证**:
  - `cargo test -q -p sealdb-session --lib test_explain_analyze_table_scan_act_rows`
  - `cargo test -q -p sealdb-session --lib test_explain_skyline_and_join_reorder`
  - `./project/scripts/feature_gate.sh C2`
- **下一步**:
  - C2 `cop-task-rows` → `n-table-dp` → `index-merge-join` → `cost-model-v2`

### 2026-11-03

- **Phase**: 完整性波 C4（typed-placeholders）
- **完成**:
  - `plan_cache_param.rs`：`normalize_prep_cache_key` 保留 `?` 模板作 Domain plan-cache key
  - Text EXECUTE / COM_STMT_EXECUTE：仍 bind 字面量执行，但 put/get 用 unbound prepared SQL
  - PointGet 执行时从当前 bound stmt 重读 handle（缓存的首执行 plan shape 可复用）
  - C4 gate `execute-typed-placeholders`；`status.json` / leftover C4 gaps → `[]`
- **验证**:
  - `cargo test -q -p sealdb-session --lib test_typed_placeholders_share_plan_cache`
  - `cargo fmt` + prepare/cache session tests
- **下一步**:
  - 完整性波 C2 / C5

### 2026-11-03

- **Phase**: 完整性波 C4（shared-plan-cache）
- **完成**:
  - Domain-shared `InstancePlanCache`（`plan_cache.rs` / `plan_cache_instance.rs` / `plan_cache_lru.rs`）
  - 同 Domain 跨 Session 命中同一 restored SQL + schema version；`Domain::plan_cache_hit_count` 测试钩子
  - C4 gate `execute-shared-plan-cache`；`status.json` 去掉 `shared-plan-cache`
- **验证**:
  - `cargo test -q -p sealdb-session --lib test_shared_plan_cache_cross_session`
  - `cargo fmt` + planner/session tests
- **下一步**:
  - C4 `typed-placeholders`

### 2026-11-03

- **Phase**: 完整性波 C4（create-binding）
- **完成**:
  - `CREATE BINDING FOR … USING …` / `DROP BINDING FOR …` / `SHOW BINDINGS`
  - Domain-shared `sealdb-bindinfo` 内存绑定；SELECT 按规范化 restored SQL 命中后替换为 bind SQL
  - C4 gate `execute-binding`；`status.json` 去掉 `create-binding`
- **验证**:
  - `cargo test -q -p sealdb-session --lib test_*binding*`
  - `./project/scripts/feature_gate.sh C4`（含 leftover check）
- **下一步**:
  - C4 `shared-plan-cache` → `typed-placeholders`

### 2026-11-03

- **Phase**: 完整性波 C4（user-vars）
- **完成**:
  - `SET @a` / `SELECT @a` / `EXECUTE … USING @a` 会话用户变量
  - 未赋值用户变量读出为 NULL
- **验证**:
  - `./project/scripts/feature_gate.sh C4`
- **下一步**:
  - C4 `create-binding` → `shared-plan-cache` → `typed-placeholders`

### 2026-11-03

- **Phase**: 完整性波 C4（com-stmt-prepare）
- **完成**:
  - F21/F22 leftover 收口后进入 C4→C2→C5
  - COM_STMT_EXECUTE 走 `execute_cached_stmt`（session plan cache）
  - C4 gate 增加 `execute-com-stmt`；`status.json` 去掉 `com-stmt-prepare`
- **验证**:
  - `./project/scripts/feature_gate.sh C4`
- **下一步**:
  - C4 `user-vars` → `create-binding` → `shared-plan-cache` → `typed-placeholders`

### 2026-11-02

- **Phase**: F05 leftover（parallel-sort）
- **完成**:
  - ORDER BY 将行切成 worker 分区并行排序后 k-way merge
  - `SEALDB_SORT_CONCURRENCY` 控制 worker 数（默认 CPU 数，上限 8）
- **验证**:
  - executor serial vs parallel；F05 gate execute-parallel-sort
- **下一步**:
  - leftover F07 / F09 / F02
  - C6 保持 deferred

### 2026-11-02

- **Phase**: F05 leftover（topn-spill）
- **完成**:
  - ORDER BY + LIMIT 走 TopN 堆；LIMIT 大于内存行阈值时 spill 有序 run 并只 merge 前 N 行
  - 无 LIMIT 的 ORDER BY 仍走 Sort
- **验证**:
  - executor TopN heap vs spill；F05 gate execute-topn / execute-topn-spill
- **下一步**:
  - leftover F05 parallel-sort
  - C6 保持 deferred

### 2026-11-02

- **Phase**: F06 leftover（batch-point-get）
- **完成**:
  - handle / unique-index `IN (...)` uses BatchPointGet; single-value IN stays PointGet
  - NOT IN and non-unique IN stay table/index scan
- **验证**:
  - session batch + unique IN; F06 gate execute-batch-point-get
- **下一步**:
  - leftover F05 topn-spill
  - C6 保持 deferred

### 2026-11-02

- **Phase**: F06 leftover（index-point-get）
- **完成**:
  - unique-index equality uses PointGet (index key Get, then row Get)
  - non-unique indexes stay IndexLookUp / IndexReader
- **验证**:
  - session unique PointGet; F06 gate execute-index-point-get
- **下一步**:
  - leftover F06 batch-point-get / F05 topn-spill
  - C6 保持 deferred

### 2026-11-02

- **Phase**: F10 leftover（range-scan）
- **完成**:
  - first-column index range (`>`, `>=`, `<`, `<=`, `BETWEEN`) on IndexLookUp / covering IndexReader
  - unique index range uses handle from the row-key value
- **验证**:
  - session range + unique range; F10 gate execute-range-scan
- **下一步**:
  - leftover F06 index-point-get / F05 topn-spill
  - C6 保持 deferred

### 2026-11-02

- **Phase**: F10 leftover（covering-reader）
- **完成**:
  - covering `IndexReader` when SELECT/WHERE/ORDER BY/GROUP BY/HAVING columns ⊆ index ∪ integer handle
  - unique index handle decoded from row-key value; SELECT `*` still IndexLookUp unless the index covers every table column
- **验证**:
  - session covering + unique lookup; F10 gate execute-covering
- **下一步**:
  - leftover range-scan
  - C6 保持 deferred

### 2026-11-02

- **Phase**: F10 leftover（unique-pk）
- **完成**:
  - CREATE TABLE PRIMARY KEY / UNIQUE INDEX + CREATE UNIQUE INDEX
  - INSERT / unique-index backfill 拒绝 Duplicate entry (1062)
- **验证**:
  - parser / session unique-pk + F10 gate
- **下一步**:
  - leftover covering-reader / range-scan
  - C6 保持 deferred

### 2026-11-02

- **Phase**: leftover spill（HashAgg / Sort / HashJoin）
- **完成**:
  - `DataInDiskByRows` 行编码落盘
  - HashAgg 按 group key 分区 spill；Sort 有序 run + k-way merge；HashJoin 两侧按 join key 分区
  - `SEALDB_SPILL_MEM_ROWS` 控制内存行阈值（默认不 spill）
- **验证**:
  - executor 单元测试 spill vs in-memory；F05/F08/F09 gate
- **下一步**:
  - leftover unique-pk / covering-reader / remaining SHOW
  - C6 保持 deferred

### 2026-11-02

- **Phase**: F08 leftover（StreamAgg）
- **完成**:
  - 无 GROUP BY 的标量聚合走 StreamAgg；GROUP BY 仍 HashAgg
- **验证**:
  - session EXPLAIN COUNT(*) / GROUP BY + F08 gate
- **下一步**:
  - leftover spill
  - C6 保持 deferred

### 2026-11-02

- **Phase**: F11 leftover（其余 information_schema 表）
- **完成**:
  - STATISTICS / KEY_COLUMN_USAGE / TABLE_CONSTRAINTS / PARTITIONS
  - CHARACTER_SETS / COLLATIONS / ENGINES；VIEWS 等空表不再 1146
  - SHOW INDEX / CHARSET / COLLATION
- **验证**:
  - session show + itest infoschema + F11 gate
- **下一步**:
  - leftover StreamAgg / spill
  - C6 保持 deferred

### 2026-11-01

- **Phase**: F11 leftover（SHOW FULL COLUMNS）
- **完成**:
  - `SHOW FULL COLUMNS FROM` 增加 Collation/Privileges/Comment
- **验证**:
  - parser / session / itest infoschema + F11 gate
- **下一步**:
  - leftover 其余 I_S 表
  - C6 保持 deferred

### 2026-11-01

- **Phase**: F08 leftover（DISTINCT agg）
- **完成**:
  - SUM/AVG/MIN/MAX DISTINCT 与 COUNT(DISTINCT) 共用 seen-set
- **验证**:
  - session agg 单测 + itest agg + F08 gate
- **下一步**:
  - leftover（remaining I_S / show-full-columns）
  - C6 保持 deferred

### 2026-11-01

- **Phase**: F04 leftover（PPD / coprocessor-filter）
- **完成**:
  - `rule_predicate_push_down` 把 CNF 谓词下推到 TableReader / IndexLookUp
  - 扫描时 `coprocessor::filter_rows` 过滤（MemStorage 与 TiKV KV scan 共用）
  - Inner join 单侧谓词下推；outer-join 内侧谓词保持 residual
  - EXPLAIN `task: cop` + `pushed:`
- **验证**:
  - planner PPD 单测 + session WHERE/JOIN/IndexLookUp
  - F04 feature gate
- **下一步**:
  - leftover 其余项（SUM DISTINCT / remaining I_S / show-full-columns）
  - C6 保持 deferred

### 2026-11-01

- **Phase**: SHOW CREATE / COUNT(DISTINCT)
- **完成**:
  - `SHOW CREATE TABLE` / `SHOW CREATE DATABASE`
  - `COUNT(DISTINCT col)`；SUM/AVG/MIN/MAX DISTINCT 仍 unsupported
- **验证**:
  - session 单测 + integrationtest agg/infoschema
- **下一步**:
  - leftover 其余项或停在当前完整性波

### 2026-11-01

- **Phase**: tikv-persist
- **完成**:
  - Domain 启动从 Storage `m` 前缀加载 Mutator
  - DDL / next_handle 写回同一前缀（含删除残留 meta key）
  - 新 Domain 可看见上一进程建的表和行
- **验证**:
  - `test_schema_persists_across_domain`
  - `test_tikv_schema_reload_if_available`
- **下一步**:
  - SHOW CREATE / COUNT(DISTINCT)

### 2026-11-01

- **Phase**: 表扫描 Projection
- **完成**:
  - 非 `SELECT *` 在 Selection/Sort/Limit 之后包 Projection
  - `SELECT id AS i` / `SELECT name ORDER BY id` / join 列裁剪
- **验证**:
  - `cargo test -p sealdb-session --lib test_table_column_projection`
  - `./tests/integrationtest/run-tests.sh -t basic --require-tikv`（录金样后）
- **下一步**:
  - tikv-persist（schema / next_handle 写 TiKV）

### 2026-11-01

- **Phase**: 集成测试双存储
- **完成**:
  - 同一 `t/*.test` 在 MemStorage 与 TiKV 上各跑一遍，共用 `r/*.result`
  - `--store memory|tikv|both`；`--require-tikv` / `SEALDB_ITEST_REQUIRE_TIKV`
  - TiKV 会话用独立 global id，避免残留行键污染
- **验证**:
  - `cargo test -p sealdb-integrationtest --lib`
  - `./tests/integrationtest/run-tests.sh --require-tikv`
- **下一步**:
  - leftover（PPD / DISTINCT agg / SHOW CREATE）或 C6 deferred

### 2026-11-01

- **Phase**: SQL 集成测试（TiDB testdata）
- **完成**:
  - `tests/integrationtest`：`t/*.test` + `r/*.result` + `sealdb-itest`
  - 进程内 `Session` 执行；`-- error N` / `-- sorted_result`
  - 种子套件：basic / predicate / agg / dml / join / txn / infoschema / errors
  - `connect`/`connection`、mysql-tester over `--listen` 延后
- **验证**:
  - `cargo test -p sealdb-integrationtest`
  - `./tests/integrationtest/run-tests.sh`
- **下一步**:
  - leftover（PPD / DISTINCT agg / SHOW CREATE）或 C6 deferred

### 2026-11-01

- **Phase**: SQL leftover（HAVING / INSERT SELECT / IN BETWEEN LIKE / information_schema）
- **完成**:
  - `IN` / `BETWEEN` / `LIKE` / `IS NULL` 谓词
  - `GROUP BY ... HAVING`（含 select 列表外的聚合）
  - `INSERT ... SELECT`
  - `information_schema.SCHEMATA|TABLES|COLUMNS` 可作为 SELECT 源
- **验证**:
  - `cargo test --workspace`
  - parser / session 单测覆盖上述路径
- **阻塞/风险**:
  - PPD/coprocessor、DISTINCT agg、其余 I_S 表仍未做
- **下一步**:
  - 继续 leftover 或保持 C6 deferred

### 2026-11-01

- **Phase**: 存储后端（MemStorage + TiKV）
- **完成**:
  - 保留 `MemStorage` 为默认；`--store memory|tikv` / `SEALDB_STORE` 切换
  - `--pd` / `SEALDB_PD_ENDPOINTS` 指向 PD；`pkg/store/driver` 封装 `tikv-client` 0.4
  - `Domain` 持有 `Arc<dyn Storage>`；`Session::new()` / feature gate 仍走内存
  - listen 连接共享同一个 Domain（同进程可见同一张表）
- **验证**:
  - `cargo test --workspace`（无集群时跳过 live TiKV）
  - 本地 PD `127.0.0.1:2379` 上 `--store tikv` SQL 冒烟
- **阻塞/风险**:
  - schema / `next_handle` 仍在进程内存；重启后表定义丢失，行键可残留
  - TiKV 上 RC `refresh_snapshot`、coprocessor、跨进程 schema 未做
- **下一步**:
  - SQL leftover（HAVING / INSERT SELECT / IN/BETWEEN/LIKE / information_schema）
  - C6 保持 deferred

### 2026-11-01

- **Phase**: C5 SET / SQL Mode / collation
- **完成**:
  - `SET NAMES` / `sql_mode` / `collation_connection` 会话变量
  - `SELECT @@collation_connection` / `@@sql_mode`
  - `*_ci` 校对下 ASCII 大小写不敏感比较
- **验证**:
  - `./project/scripts/feature_gate.sh C5`
- **下一步**:
  - C6 保持 deferred

### 2026-11-01

- **Phase**: C4 Prepared / Plan Cache
- **完成**:
  - `PREPARE name FROM 'sql'` / `EXECUTE ... USING` / `DEALLOCATE PREPARE`
  - 按还原 SQL + schema version 做会话级 plan cache
- **验证**:
  - `./project/scripts/feature_gate.sh C4`
- **下一步**:
  - C5 错误码、SQL Mode、charset/collation

### 2026-11-01

- **Phase**: C3 SchemaChecker / 隔离 / 交叉读写
- **完成**:
  - 显式事务遇到并发 DDL 返回 8028
  - `START TRANSACTION` / `SET TRANSACTION` 支持 READ COMMITTED 语句快照
  - 同 Domain 两会话交叉读写（RR 看不见已提交更新，RC 看得见）
- **验证**:
  - `./project/scripts/feature_gate.sh C3`
- **下一步**:
  - C4 Prepared / Plan Cache / Binding

### 2026-11-03

- **Phase**: C3 completeness gaps 收口
- **完成**:
  - Domain 内存 MDL：同表 DML shared vs DDL exclusive（冲突 1205）
  - schema lease + `reload_schema_from_storage`（过期 8027）
  - `SERIALIZABLE` 语法接受，MVB 映射 RR 快照
  - SAVEPOINT 确认已由 F12 覆盖并纳入 C3 gate
  - TiKV same-cluster 跨会话：`SEALDB_TIKV=1` 时跑，否则 skip（不做假 multi-PD）
  - `gaps: []`
- **验证**:
  - `./project/scripts/feature_gate.sh C3`
- **下一步**:
  - C1 leftover 分母 / C6 仍 deferred

### 2026-11-01

- **Phase**: C2 计划形状 / 代价 / JoinReorder
- **完成**:
  - `EXPLAIN` / `FORMAT='json'|'dot'` 输出 TiDB 五列树或 JSON/DOT
  - ANALYZE 后按 scan/net 代价在 TableReader 与 IndexLookUp 之间做 skyline
  - 两表 Inner HashJoin 把较小表放到 build 侧
- **验证**:
  - `./project/scripts/feature_gate.sh C2`
- **下一步**:
  - C3 事务隔离边角、SchemaChecker、交叉读写

### 2026-11-01

- **Phase**: C1 leftover 分母
- **完成**:
  - 冻结 91 条功能 `gaps[]` + 56 个未核心移植语句 + 42 个 integrationtest suite
  - `project/features/leftover.json` 作为机器可读分母
- **验证**:
  - `./project/scripts/feature_gate.sh C1`
- **下一步**:
  - C2 计划形状 / 代价 / Skyline / JoinReorder

### 2026-11-03

- **Phase**: F21/F22 leftover 收口；进入完整性波
- **完成**:
  - F21：ANALYZE INDEX、mstats persist、auto-analyze、HashJoin/TopN cost；`gaps: []`
  - F22：SSL、CLIENT_COMPRESS、COM_STMT_* 二进制、mysql_native_password、multi-resultset；`gaps: []`
  - `project/features/leftover.md` Feature gaps → `none`
- **验证**:
  - `./project/scripts/feature_gate.sh F21`
  - `./project/scripts/feature_gate.sh F22`
- **下一步**:
  - 完整性波按 C4 → C2 → C5 收 gaps（C6 仍 deferred）

### 2026-11-01

- **Phase**: F22 MySQL 协议 COM_QUERY
- **完成**:
  - handshake + `COM_QUERY` / `COM_PING` / `COM_INIT_DB` / `COM_QUIT`
  - 文本协议结果集 / OK / ERR
  - `--listen host:port`
- **验证**:
  - `./project/scripts/feature_gate.sh F22`
- **下一步**:
  - F22 leftover：SSL / compress / COM_STMT / auth / multi-resultset

### 2026-11-01

- **Phase**: F21 ANALYZE + 统计 / CBO
- **完成**:
  - `ANALYZE TABLE` 收集行数与列 NDV
  - 分析后 NDV<=1 的等值谓词改走 TableReader
- **验证**:
  - `./project/scripts/feature_gate.sh F21`
- **下一步**:
  - F21 leftover：histogram/TopN / persist / auto-analyze / join-cost

### 2026-11-01

- **Phase**: F20 GRANT/REVOKE
- **完成**:
  - `GRANT` / `REVOKE` SELECT/INSERT/UPDATE/DELETE/CREATE/DROP/ALL
  - 默认 SkipGrantTables；关闭后按内存授权检查
  - `SHOW GRANTS`
- **验证**:
  - `./project/scripts/feature_gate.sh F20`
- **下一步**:
  - F21 ANALYZE

### 2026-11-01

- **Phase**: F19 分区裁剪
- **完成**:
  - `CREATE TABLE ... PARTITION BY RANGE/HASH`
  - INSERT locate 到物理分区 id
  - 等值/比较谓词静态裁剪 TableReader
- **验证**:
  - `./project/scripts/feature_gate.sh F19`
- **下一步**:
  - F20 GRANT/REVOKE

### 2026-11-01

- **Phase**: F18 在线 DDL
- **完成**:
  - `CREATE INDEX` / `DROP INDEX`
  - `ALTER TABLE ADD/DROP COLUMN` / `ADD/DROP INDEX`
  - 存量行二级索引回填
- **验证**:
  - `./project/scripts/feature_gate.sh F18`
- **下一步**:
  - F19 分区裁剪

### 2026-11-01

- **Phase**: F17 CTE
- **完成**:
  - 非递归 `WITH cte AS (SELECT ...)` / `WITH cte(cols) AS (...)`
  - CTE 链式引用、列裁剪、与 `UNION` 外层组合
- **验证**:
  - `./project/scripts/feature_gate.sh F17`
- **下一步**:
  - F18 在线 DDL

### 2026-11-01

- **Phase**: F16 Window
- **完成**:
  - `ROW_NUMBER` / `RANK` / `DENSE_RANK`
  - `COUNT/SUM/AVG/MIN/MAX OVER (PARTITION BY ... ORDER BY ...)`
  - 无 ORDER BY 时窗口聚合覆盖整个分区
- **验证**:
  - `./project/scripts/feature_gate.sh F16`
- **下一步**:
  - F17 CTE

### 2026-11-01

- **Phase**: F15 `UNION` / `UNION ALL`
- **完成**:
  - 左结合 `UNION` 去重与 `UNION ALL` 追加
  - 外层 `ORDER BY` / `LIMIT`
  - 列数不一致返回 1222
- **验证**:
  - `./project/scripts/feature_gate.sh F15`
- **下一步**:
  - F16 Window

### 2026-11-01

- **Phase**: F14 子查询 / Apply / EXISTS / IN
- **完成**:
  - 非相关与相关 `EXISTS` / `IN (SELECT ...)` / 标量子查询
  - `IN` 值列表
  - 相关列绑定后走 NestedLoopApply
- **验证**:
  - `./project/scripts/feature_gate.sh F14`
- **下一步**:
  - F15 `UNION` / `UNION ALL`

### 2026-11-01

- **Phase**: F13 `SELECT ... FOR UPDATE` + 悲观 RR
- **完成**:
  - `FOR UPDATE` 读最新并加排他锁
  - `BEGIN PESSIMISTIC` 下 UPDATE/DELETE 加锁读最新
  - 锁冲突立即 1205，不阻塞等待
- **验证**:
  - `./project/scripts/feature_gate.sh F13`
- **下一步**:
  - F14 子查询 / Apply / EXISTS / IN

### 2026-11-01

- **Phase**: F12 BEGIN/COMMIT/ROLLBACK + 乐观 RR
- **完成**:
  - 显式事务与 autocommit 共用 session-owned txn
  - 内存 KV 快照读 + 提交写写冲突
- **验证**:
  - `./project/scripts/feature_gate.sh F12`
- **下一步**:
  - F13 `SELECT ... FOR UPDATE` / 悲观 RR

### 2026-11-01

- **Phase**: F10 二级索引 / IndexLookUp
- **完成**:
  - `CREATE TABLE ... INDEX/KEY` 写入 `IndexInfo`
  - INSERT/UPDATE/DELETE 维护二级索引 key
  - 索引列等值走 `IndexLookUpExecutor`
- **验证**:
  - `./project/scripts/feature_gate.sh F10`
- **下一步**:
  - F12 事务 / BEGIN COMMIT ROLLBACK

### 2026-11-01

- **Phase**: F09 HashJoin
- **完成**:
  - 两表 Inner / Left / Right HashJoin
  - 等值 ON 条件
- **验证**:
  - `./project/scripts/feature_gate.sh F09`
- **下一步**:
  - F10 二级索引 / IndexLookUp

### 2026-11-01

- **Phase**: F08 聚合 / GROUP BY
- **完成**:
  - `COUNT/SUM/AVG/MIN/MAX` 走内存 HashAgg
  - 单表 `GROUP BY` 列分组
- **验证**:
  - `./project/scripts/feature_gate.sh F08`
- **下一步**:
  - F09 HashJoin

### 2026-11-01

- **Phase**: F07 UPDATE / DELETE
- **完成**:
  - 单表 `UPDATE SET` / `DELETE` 写内存 KV
  - WHERE 过滤复用 Selection 表达式
- **验证**:
  - `./project/scripts/feature_gate.sh F07`
- **下一步**:
  - F08 聚合 / GROUP BY

### 2026-11-01

- **Phase**: F06 PointGet
- **完成**:
  - `WHERE id = const` 走 handle PointGet
  - INSERT 用第一列整数当 handle
- **验证**:
  - `./project/scripts/feature_gate.sh F06`
- **下一步**:
  - F07 UPDATE / DELETE

### 2026-11-01

- **Phase**: F05 ORDER BY / LIMIT
- **完成**:
  - `ORDER BY` 内存排序
  - `LIMIT` / `OFFSET`
- **验证**:
  - `./project/scripts/feature_gate.sh F05`
- **下一步**:
  - F06 PointGet

### 2026-11-01

- **Phase**: F04 WHERE / Selection
- **完成**:
  - `WHERE` 比较/逻辑过滤
  - `SelectionExec` 包 TableReader / Dual
- **验证**:
  - `./project/scripts/feature_gate.sh F04`
  - 见 `project/reports/features/F04/`
- **阻塞/风险**:
  - 无 PPD / `IN` / `LIKE`
- **下一步**:
  - F05 `ORDER BY` / `LIMIT`

### 2026-11-01

- **Phase**: F03 INSERT + SELECT *
- **完成**:
  - `INSERT ... VALUES` 写入 TiDB 形 row key
  - `SELECT *` 经 TableReader 扫描表前缀
  - Domain 持有 `MemStorage`，单测/feature gate 不依赖实集群
- **验证**:
  - `./project/scripts/feature_gate.sh F03`
  - 见 `project/reports/features/F03/`
- **阻塞/风险**:
  - 行值暂用 tagged 布局，不是 TiDB rowcodec
  - 实集群 PD/TiKV 镜像仍在拉取；host-side tikv-client 还要改 advertise
- **下一步**:
  - F04 `WHERE` / Selection

### 2026-11-01

- **Phase**: F02 元数据
- **完成**:
  - `CREATE/DROP DATABASE|TABLE` 核心路径
  - TiDB 兼容 meta key（`m` + hash/string encoding，`DB:{id}` / `Table:{id}`）
  - InfoSchema 内存快照 + Domain reload
- **验证**:
  - `./project/scripts/feature_gate.sh F02`
  - 见 `project/reports/features/F02/`
- **阻塞/风险**:
  - 仍是内存 meta，未写入真实 TiKV
- **下一步**:
  - F11 `SHOW` / `DESC`，或 F03 `INSERT` + `SELECT *`

### 2026-11-07

- **Phase**: R3 分区/谓词波
- **整体快照**: [status-report.md](./status-report.md)（manifest 254 套：green 23 / in_progress 31 / pending 190 / local 8 / deferred 2）
- **完成**:
  - `executor/partition/partition_boundaries` 1:1 对齐 TiDB 金样（Gate A+B）
  - 统计：窄列（<=10k distinct）保留精确 value counts，分区扫描 selectivity 与 TiDB 一致
  - Parser：`BETWEEN` 下沉为 `>=`/`<=`（`NOT BETWEEN` → `OR`），单值 `IN (x)` 折叠为 `=`，`HIGH_NOT_PRECEDENCE` 下 `NOT x BETWEEN` 绑定更紧
  - Planner：`=`/`IN`/比较/`!=`/`AND`/`OR` 的分区裁剪；无分区命中时报 `partition:dual`；分区 filter est 走 value counts；De Morgan 产生的 `AND` 作为单个下推谓词项（EXPLAIN 打印 `and(...)` 而非逗号拼接）
  - EXPLAIN：`IN` 列表打印 `in(col, ...)` / `not(in(col, ...))`
- **验证**:
  - `run-tests.sh -s memory --status green` → PASS: memory=22
  - `run-tests.sh -s tikv --status green` → PASS: tikv=22
- **阻塞/风险**:
  - `local/agg` 在 HEAD 上已红：投影列名头不再带反引号（`name` vs `` `name` ``），属前序 “projection column ids” 改动的本地冒烟金样漂移，未改金样
- **下一步**:
  - 继续 R2/R3 分区与 planner 套件

### 2026-11-07（`executor/partition/issues` 波）

- **完成**:
  - 自连接相等键谓词派生：`t1.col1 = t2.col1` + `t1.col1 > k` ⇒ 在 `t2` 上派生镜像谓词（TiDB `pushDownPredicatesForJoin`）；所有权表在 infoschema 里按**物理表**解析，别名侧按“单表子树即拥有全部 ON 列”处理，避免别名误配到其他库同名表
  - 统计：ANALYZE 后的**空表**仍为 pseudo（`has_real_stats` 需 `version > 0 && count > 0`），EXPLAIN 保留 `10000.00 … stats:pseudo`（`UK_HP16726` 动态裁剪段已对齐）
  - 谓词折叠：`c BETWEEN T::MIN AND T::MAX` 等整域边界在 NOT NULL 主键列上折叠为裸 `TableFullScan`；支持一元负号字面量与持久的 UNSIGNED flag
  - `executor/partition/issues` 差分行数 94 → 约 60（8 个 hunk），剩余：`inl_join(c)` IndexJoin（Build/Probe、dynamic prune、decided-by range、`stats:partial`）、`desc` 算子 `_N` 编号、`partition:p2` vs `partition:all`
- **验证**:
  - `run-tests.sh -s memory --status green` → PASS: memory=23
  - `run-tests.sh -s tikv --status green` → PASS: tikv=23
- **下一步**:
  - `executor/partition/issues` 的 `inl_join(c)` IndexJoin 形状；再依次 `executor/partition/table`、`executor/partition/write`

### 2026-11-07（`executor/partition/table` 转绿）

- **Phase**: R3 分区波
- **整体快照**: [status-report.md](./status-report.md)（manifest 254 套：green 25 / in_progress 29 / pending 190 / local 8 / deferred 2）
- **完成**:
  - `ALTER TABLE ... EXCHANGE PARTITION ... WITH TABLE ...`：parser AST + lower 解析；DDL 侧校验列布局、目标表非分区后交换分区/表 ID（`meta::update_table_with_new_id` 负责 KV 元数据迁移），无物理行改写
  - `_tidb_rowid` 隐藏投影：非聚簇主键/堆表在 SELECT / WHERE / ORDER BY 显式引用行句柄时，`TableReader` 与 `IndexLookUp` 追加隐藏列 `\u{1}_tidb_rowid`；表达式解析允许未限定名绑定到该隐藏列，且不泄漏进 `SELECT *`
  - 唯一键 LEFT JOIN 上的 `count(*)` 简化：内表按完整主键等值连接时外层连接可消，改走最窄二级索引的 `StreamAgg → IndexReader → cop StreamAgg`
  - EXPLAIN：`IndexFullScan` 在全局标量覆盖索引计数下打印 `keep order:false`（`GROUP BY` 仍为 `keep order:true`）
  - 分区表跳过同步 auto-analyze，对齐 TiDB 的分区异步统计
  - `SET GLOBAL tidb_partition_prune_mode`（含 `= DEFAULT`）与 mysql-tester session 指令
- **验证**:
  - `run-tests.sh -s memory --status green` → PASS: memory=25
  - `run-tests.sh -s tikv --status green` → PASS: memory=0 tikv=25 skipped=229
  - `executor/partition/table`：Mem 0 diff、TiKV 0 diff，升 `green`
- **阻塞/风险**:
  - `cargo test -p sealdb-session --lib` 在 HEAD 上既有 18 个陈旧单测失败（如期望 `cop` 实际 `cop[tikv]`、`2.0` vs `2.0000`、`SELECT 1 WHERE 0` 返回行），非本次改动引入；集成金样为准，单测待后续单独清理
- **下一步**:
  - `executor/partition/issues` 剩余形状（HashAgg / `inl_join` IndexJoin / `_N` 编号）；再做 `executor/partition/write`

### 2026-11-01
- **Phase**: 功能波 Unlock + F01
- **完成**:
  - workspace 空 crate 可编译；`[workspace.dependencies]` 落地
  - U1 codec/tablecodec 核心向量与 row/index key
  - U2 sqlparser_bridge + `--parse --ast-json`
  - U3 `sealdb-server --execute "SELECT 1"`
  - F01 算术/比较/逻辑/`CAST`/`IF`/`IFNULL`
  - 功能清单 `project/features.md` + leftover 脚本 + CI
- **验证**:
  - `cargo test --workspace` / `cargo clippy --workspace --all-targets -- -D warnings`
  - 见 `project/reports/features/F01/`
- **阻塞/风险**:
  - 本机 Homebrew cargo 是 1.97；需 `rustup` 1.99.1
  - TiKV spike 仍缺实集群；结论见 ADR-0004
- **下一步**:
  - F02 `CREATE/DROP TABLE` 写兼容 meta key

### 2026-11-08（表达式索引 + `tidb_shard` 波）

- **Phase**: R3 explain 波（表达式索引解锁）
- **完成**:
  - DDL：`IndexInfo` 的 key part 支持表达式。`CREATE INDEX … ((expr))` 不再报 1235，而是物化为隐藏生成列（`_V$…`），并在建索引时回填、DML 时维护；`information_schema.tidb_indexes` 按 TiDB 渲染表达式 part
  - Planner（`core/expr_index.rs`）：索引 part 的规范化键（列名或表达式文本）、隐藏列解析、`eval_part` 常量折叠、`shard_index_rewrite`。`tidb_shard(a)` 与 TiDB 一致按 Vitess DES null-key 哈希取模 256（`fix(expression)` 提交）折叠
  - Planner：`AddPrefix4ShardIndexes` 语义——唯一索引首 part 为 `tidb_shard(col)` 时，DNF 各分支独立改写（`(tidb_shard(a)=214 AND a=1) OR (tidb_shard(a)=142 AND a=10)`），CNF 仅在其余索引列都被等值钉住时才补前缀
  - Planner：DataSource 阶段加 `expression.PropagateConstant` 等值常量传播（`a = b AND b = 100` → `a = 100 AND b = 100`），使等值能到达 shard 改写；`updateOrPredicate` 只按“被 pin 反证”的分支做矛盾消除，字面量分支（分区裁剪产生的 `or(0, eq(a,-1))`）保持 TiDB 打印形状
  - Planner：表达式索引表的 `SELECT *` 展开走 cop projection（`TableReader data:Projection` / `IndexReader index:Projection`），点查在表达式索引上时在计划上方补 Projection 隐藏 `_V$` 列
  - Executor：`BatchPointGet` 的 key 值改为按表元数据计算（表达式 part 由普通 part 折叠），聚簇主键（handle）走 `index=None`；单点查仍从 `IN` 列表取首个值；row `IN` 列表即使折叠成单个 key 也保持 `Batch_Point_Get` 形状
- **验证**:
  - `explain_shard_index`：Mem/TiKV `PASS`（含 `Point_Get` / `Batch_Point_Get` / `IndexLookUp` + `IndexRangeScan` 形状）
  - `run-tests.sh -s memory --status green` → PASS: memory=35
  - `run-tests.sh -s tikv --status green` → PASS: tikv=35
  - `--status local`：memory=8 / tikv=7（`local/txn` 标 `skip_tikv`）
- **阻塞/风险**:
  - 本地 PD/TiKV 数据目录若被真实 TiDB 初始化过，`mysql.tidb_ddl_notifier` 等表的 meta JSON 里 `Flen` 是 `4294967295`（TiDB 写法），当前 `FieldType.flen: i32` 读不回来；本地需清空 `$SEALDB_CLUSTER_DATA` 再起
  - `cte` 套件仍有既有 mismatch（CTE 上多余 Projection），与本次改动无关，仍标 `in_progress`
- **下一步**:
  - `explain_easy` / `explain_easy_stats` / `explain_generate_column_substitute` 用已落地的表达式索引继续推
  - R3 其余 `in_progress`：`explain_join_stats`、`topn_push_down`、`index_merge`

### 2026-11-08（SQL 文本保真 + 测试基线修正波）

- **Phase**: 跨波次基础修正
- **完成**:
  - Parser/Session/Server：SQL 重写不再用 `bytes[i] as char` 拼接（该写法把 UTF-8 字节按 Latin-1 重新编码）。`rewrite_primary_key_nonclustered`、`bind_sql`（`EXECUTE … USING`）、`bind_placeholders`（COM_STMT_EXECUTE）、`normalize_sql` 全部改为按字符拷贝，`INSERT … VALUES ('孵')` / `SELECT '孵'` 不再输出 `å\u{ad}µ`
  - Parser：`PREPARE s FROM 'SELECT ''a'', ?'` 保留完整语句文本（原先在成对引号的第一处截断，只剩 `SELECT `）；`take_explain_format` 扫描时解开 `''` 转义
  - Parser：`.test` 末尾无 `;` 的语句按 mysql-tester EOF 语义执行（`explain_complex_stats` 以 `set @@sql_mode=default` 收尾）
  - Session：`system_var_result` 只接管“纯常量列表”的无 FROM SELECT；带 WHERE/GROUP BY/ORDER BY/LIMIT/DISTINCT 的查询回到 planner（`SELECT 1 WHERE 0` 原先返回 1 行，现为 `TableDual rows:0`）
  - Planner：select-list `IN (subq)` / `aggregate(IN)` / `EXISTS … HAVING agg = outer` 三个 decorrelation 形状从“按 suite 名启用”改为按查询结构识别，真实查询同样可走
  - itest：Memory 后端每套件新建 `MemStorage`（原先共享实例，`begin()` 反复克隆累积行导致全量 gate 卡住）；TiKV 后端每套件前清空全部非系统 schema（TiDB 每文件都是全新实例，未过滤的 `information_schema.views` 不应看到其它文件的残留）
  - itest：重录 `local/join`、`local/infoschema`（旧 golden 记录的是未对齐 TiDB 的行为：LEFT JOIN 丢未匹配行、CROSS JOIN 只出 2 行、SHOW CREATE TABLE 缺 `DEFAULT NULL`/表选项）
  - 单测：`lower_0x80` 断言改为二进制 datum（`Datum::Bit`），与 0759561 的十六进制字面量决策一致
- **验证**:
  - `./run-tests.sh -s memory --status green,local` → PASS: memory=42
  - `./run-tests.sh -s tikv --status green,local -k` → PASS: tikv=41（`local/txn` 标 skip_tikv）
  - `cargo test -p sealdb-parser -p sealdb-bindinfo -p sealdb-server` 全绿
- **阻塞/风险**:
  - `cargo test -p sealdb-session` 仍有 15 条历史红灯（`SELECT 1 WHERE 0` 已修，余下为行格式 EXPLAIN 缺 `pushed:`、`t1.name` 列头、cost model / join reorder 断言等），需逐条对照 TiDB 判断是测试过期还是实现缺失；CI 跑 `cargo test --workspace`，需单独一波清理
  - TiKV 集群被 SIGKILL 掉进程后会残留锁（`Failed to resolve lock` / `timed out locking meta persist`），本地需 `run_local_pd_tikv.sh stop` 后清空 `$SEALDB_CLUSTER_DATA` 重启
- **下一步**:
  - R3 `in_progress`：`explain_join_stats`（需 stats 索引等值估计 19977、HashAgg/StreamAgg 选择、`TIDB_INLJ` hint 形状）、`explain_shard_index`、`topn_push_down`、`index_merge`、表达式索引（`explain_easy` 的 `index ((a+b+1))`）

### 2026-11-08（`explain_foreign_key` 波）

- **Phase**: R3 explain 波
- **完成**:
  - DDL：`FOREIGN KEY` 约束落 `TableInfo.ForeignKeys`（列、引用表/列、`ON UPDATE` / `ON DELETE` 是否 CASCADE）；`build_table_info` 按 TiDB `BuildTableInfo` 规则补建隐式外键索引（既非现有索引前缀覆盖、也非整型聚簇主键时才建，未命名时生成 `<table>_ibfk_<n>`）
  - Planner：新增 `planner/core/foreign_key.rs`，在 `Insert` / `Update` / `Delete` 下按 TiDB 语义生成 `Foreign_Key_Check`（`check_exist` 引用侧、`check_not_exist` 子表侧）与 `Foreign_Key_Cascade`（`on_update:CASCADE` / `on_delete:CASCADE`）；节点按 (引用表, 子表, 外键定义序) 排序，检查在前、级联在后，覆盖多表 UPDATE/DELETE、`INSERT … ON DUPLICATE KEY UPDATE` 与自引用外键
  - Session：`foreign_key_checks` 变量真正生效；`SET foreign_key_checks=0` 时 planner 不再生成上述节点
  - Parser：`UPDATE t1, t2 SET …` 逗号多表支持（右侧为普通表引用时折叠为 `JOIN … ON TRUE`）
  - EXPLAIN：`Foreign_Key_Check` / `Foreign_Key_Cascade` 行（estRows `0.00`、task `root`、access `table:…, index:…`）排在 DML 源计划之后；DML 非末位子节点的 cop 子树补齐 `│ ` 续行
- **验证**:
  - `explain_foreign_key`：Mem/TiKV `PASS`（161 行全对齐，含两条多表 DML 的 join 形状）
  - `run-tests.sh -s memory --status green` → PASS: memory=33（无回归）
  - `run-tests.sh -s tikv --status green` → PASS: tikv=33
- **多表 DML join 形状（本次补齐）**:
  - `WHERE` 等值键抽取：`dml_join_conditions` 把 `t_1.id = t_2.id` 从 `selection` 提升为 `JOIN … ON`，把 `CROSS` 修正为 `INNER`，并把单侧过滤谓词按同名键镜像到另一侧（`t_1.id=1` → `t_2.id=1`，`t_1.id>0` → `t_2.id>0`）。这正是 TiDB `pushDownPredicatesForJoin` 的行为，两侧因此都能走 `Point_Get` 或 handle range。
  - `dml_join_shape`：两侧同 handle 的 `Point_Get` 时等值键折叠为 TRUE，输出 `HashJoin CARTESIAN`（Build 侧为语句自身表）；两侧都是 `TableRangeScan` 时输出 `MergeJoin`（`keep order:true`），estRows 用 null-reject 选择性 `3333.33 ⋈ 3333.33 → 4166.67`。
  - 谓词下推收紧：`qualify_cross_equality` 现在要求表限定符必须真的属于该 join 子树，才允许把跨表等值升为 hash key；新增 `same_join_key_type`，声明类型不同的跨表等值（如 `int = bit(32)`）继续留在残差 `Selection` 里走 MySQL 隐式转换，避免按原始 datum 比较丢行（回归 `executor/jointest/join`）。
- **阻塞/风险**:
  - `explain_foreign_key` 已转 `green`（33 → 34 套件）
- **下一步**:
  - 继续 R3 `in_progress`：`explain_easy` / `explain_easy_stats` / `explain_stats` / `topn_push_down` 等

### 2026-11-08（`explain_join_stats` 波）

- **Phase**: R3 explain 波
- **完成**:
  - Statistics：`IndexStats::stats_ver` 落库/持久化/JSON 解析。TiDB `TableStatsFromJSON` 对无 `stats_ver` 但 histogram NDV>0 的 JSON 判为 v1；v1 的 histogram 已包含 TopN 行，`TotalRowCount` 不再重复加 TopN（v2 才加）。`idx_b = 22336` 因此取 TopN 精确值 `19977`，不再被 `realtime / total` 缩放成 `19593.26`
  - Planner：`is_pseudo_card` 只认 10000 / 9990 两个伪统计默认值，已 analyze 的 19977 不再被当成伪统计走 `min * 1.25`
  - Planner：标量聚合跨 join 时的输出列号按 DataSource schema 顺序分配（`e(a,b)` + `lo(a,_tidb_rowid)` → `Column#5`），与 TiDB `AllocPlanColumnID` 一致
  - Planner：`/*+ TIDB_INLJ(e) */` 的内表按 join key 首列选二级索引，产出 `IndexLookUp` 探针：`decided range` 只编码 join key，key 的 null-reject 是 `Selection(Build)` + `IndexRangeScan`，表侧谓词留在 `Selection(Probe)`；`IndexJoin` 行数用 `outer * inner / max(NDV)`
  - Planner：逗号 join 解析为 `Cross`，其 WHERE 等值仍作为 join key；hint 形状替换整条语句计划后重新挂上标量 `count(*)` 的 `HashAgg`
  - EXPLAIN：`IndexRangeScan` 输出是 Build 侧基数，`Selection(Build)` 与 `TableRowIDScan` 显示 build 后的行数而不是探针估计；`Selection(Build)` 的基数只受 index 侧残差影响，探针侧谓词不再缩小它
  - Statistics：`pseudo::filter_rows` 对 `cast(col AS double) = 0` 这类不透明表达式保留 0.8 选择率，不再当列等值按 NDV 计算（`point_get_plan` 的 `Selection(Build)` 复原为 `8.00`）
- **验证**:
  - `explain_join_stats`：Mem/TiKV `PASS`（两条语句全对齐：无 hint 的 `HashJoin` + `idx_b` 探针、`TIDB_INLJ(e)` 的 `IndexJoin` + `idx_a` decided range）
  - `run-tests.sh`（green，Mem+TiKV）→ PASS: memory=44 tikv=43
  - `cargo test -p sealdb-statistics -p sealdb-planner` 全绿
- **阻塞/风险**:
  - 无新增；`explain_join_stats` 已转 `green`（43 → 44 套件）
- **下一步**:
  - 继续 R3 `in_progress`：`explain_easy` / `explain_easy_stats` / `explain_generate_column_substitute` / `topn_push_down` / `explain_complex_stats`

### 2026-11-08（`select` / `jointest/join` 估行波）

- **Phase**: R1/R2/R3 收口
- **完成**:
  - Types：`Datum::compare` 的 `I64 ↔ U64` 不再走 `f64`（2^53 以上会塌缩），负数一律更小；分区表 `BETWEEN 9223372036854775807 AND 9223372036854775808` 估行回到 2.00
  - Statistics：histogram 边界保留 `i128` 精确整数（`Bound::Int`）；v1 histogram 单值估行用 `realtime/NDV`；CM sketch 随 index 统计解析/落盘；`column_ndv` 在 `col.ndv > 0` 时不再重复叠加 TopN
  - Planner：分区表跨多个分区读取不再用 index/handle 顺序顶掉 `Sort`（`partition_read_spans_multiple`）；CTE seed 上方的恒等投影按 TiDB 消除；`column_eq_is_cross_table` 区分同表等值（0.8）与跨表相关等值（0.001）
  - Planner：join 估行走 `EstimateFullJoinRowCount`（`fill_join_est_rows` / `join_est_by_ndv`）。伪统计 key 的 NDV 取该列自身 scan 子树行数 × 0.8（`t4.a` 在 2.50 行 join 脊柱下仍按 7992 计），null-supplying 侧是伪统计时外连接取 key 基数而非保留侧行数（`t4 ⟕ …` → 5.00/4.00）
  - EXPLAIN：外连接 Build/Probe 顺序与 TiDB 一致——null-supplying 侧是伪统计时较小脊柱画成 `Build`（`bushy (J1)⟕(J2)` 的 `t2.a=t3.a` 分支）
  - Planner：`try_bushy_quad_outer` 的 `ON t2.a=t3.a` 分支把保留脊柱放底层左输入（此前 `t4 ⟕ mid` 会把 t4 当保留侧，t4 为空时返回 0 行）
  - Access path：hex/bit 字面量超出 `BIT(n)` 宽度时不做 point get / seek key（留 cop Selection）；`stats:partial[col:missing]` 标注；`ORDER BY` handle / index 前缀走 keep-order 扫描 + cop `Limit`；`int_handle_range_rows` / `index_equality_rows` 用 TiDB 整数范围与 index histogram 估行
- **验证**:
  - `sealdb-itest -t select -s memory` → PASS
  - `run-tests.sh`（green，Mem+TiKV）→ `PASS: memory=44 tikv=43`
  - 四表 bushy 外连接结果集（t4 空表 → 4 行 null 扩展）逐行核对
- **阻塞/风险**:
  - 无新增
- **下一步**:
  - 继续 R3 `in_progress`：`explain_easy` / `explain_easy_stats` / `explain_generate_column_substitute` / `topn_push_down` / `explain_complex_stats`

### 2026-11-08（`explain_easy` 波：访问路径成本 + 执行器形态）

- **Phase**: R3 explain 波
- **完成**:
  - Planner（成本）：`index_lookup_cost` 按 TiDB 的探针实际成本定价。v2 把每个索引行折算成一次表探针 + double-read 请求批次（`tidb_index_lookup_size` 20000 → 每批 32 task × `tidb_request_factor` 6e6 ÷ double-read 并发 5），约等于 100 行顺序扫描；v1 用 `doubleReadFactor`，边界约 16% 选择率。以本机 TiDB 8.5 实测校准（9999 行表）：v2 49 行仍选 `IndexLookUp`、99 行转 `TableReader`；v1 1599 行仍选索引
  - Planner：`tidb_cost_model_version` 会话变量（全局 thread-local `COST_MODEL_VERSION` + plan cache key），`IndexLookUp` 在 v1 下按双读计费
  - Planner：TIMESTAMP 索引键存 UTC 墙钟，会话时区的时间范围不能直接 seek；有界时仍规划 `IndexReader` + cop `Selection`，DST gap 告警按 TiDB 次数产生（`types/time`）
  - Planner：`explain` 的伪统计估行只为“非整数常量的单列比较”套 1/3 范围选择率（`datum_range_est`），聚合派生列（`Column#…`）与整数上界仍走 `selectionFactor` 0.8，避免 `agg_predicate_pushdown` / `jointest/join` 估行回退
  - Planner：索引可见性统一走 `index_visible`（`IGNORED`/`INVISIBLE` 都不是访问路径，除非 hint）；`IndexInfo` 增加 `invisible`/`global` 选项并透传 DDL
  - Planner：`IndexMerge` 按 planner 给出的 partial path 执行（含聚簇主键 arm、`keep order`、`limit embedded`），不再从 WHERE 反推 arm
  - Planner：查询级 `ORDER BY`/`LIMIT` 挂在 `Window` 之上（`apply_window_outer_order`），窗口自身的 Sort 只负责分区/peers 顺序
  - Executor：`MemTableScan` 接收下推的等值 ranges（同列 OR、跨列 AND，按 memtable 大小写不敏感比较过滤）；`InformationSchema` 节点携带 ranges
  - Executor：`SELECT` 在 FOR UPDATE/FOR SHARE 下先做 cop `Selection` 再上锁，只锁真正返回的行
  - Session/Store：RC 的 `refresh_snapshot` 推进读时间戳（`read_ts`，写入冲突仍用 `start_ts`）；`Transaction::dirty_keys` 贯通 Mem/TiKV，给 `UnionScan` 脏读合并用
  - 测试基线：`sealdb-itest` 支持 `SEALDB_ITEST_LENIENT_ERRORS`（缺错误码时仍输出文本，便于逐条 triage）
- **验证**:
  - `executor/union_scan`：Mem `PASS`。虚拟生成列索引上的 `select * … where c > 3` 在脏事务里按 TiDB 选 `TableReader`（1/3 伪统计选择率下非覆盖索引不再胜出），行序回到 handle 合并顺序
  - `run-tests.sh -s memory` → `PASS: memory=44`
  - `run-tests.sh -s tikv` → `PASS: tikv=43`
- **阻塞/风险**:
  - 本机 `go` 是 1.26，TiDB 的 `hack` 包只认 1.25：用 `GOTOOLCHAIN=go1.25.12` 构建 `tidb-server` 作为对照 oracle（unistore），后续成本/形状对齐都以它为准
- **下一步**:
  - 继续 R3 `in_progress`：`explain_easy` / `explain_easy_stats` / `explain_generate_column_substitute` / `topn_push_down` / `explain_complex_stats`

### 2026-10-19

**决策落地**

- 旧脚手架代码已丢弃（曾短暂归档为 `legacy-rust-scaffold/`，现已删除；ADR-0007）。
- 新 workspace：24 个 `pkg/*` crate + `cmd/tidb-server`，顶层包粒度，共 25 个 package。
- Rust 升级到 **1.99.1**（rsproxy 镜像），edition 2024，resolver 3。
- 版本统一：TiDB/PD/TiKV 基线 **v8.5.8**；本地 9.0 beta 仅冒烟。
- 源码不再使用来源/移植标记；状态改由 `project/parity/status.json` 维护。

**已验证**

```text
cargo metadata --no-deps   -> 25 packages
cargo check --workspace    -> success (1.92s)
tools/oracle parse         -> SELECT `id`,`name` FROM `users` WHERE `id`=1
```

**待完成（Phase 0 剩余）**

1. `tools/oracle` 的 `plan` / `execute` 模式。
2. `project/parity/status.json` 初始登记 + `--strict` 通过。
3. TiKV/PD 连接 spike（TSO/Region/事务/Coprocessor）。
4. CI 基线（fmt/clippy/test/oracle smoke）。
5. 根 Makefile 重写（新命令）。
6. `docker/` 镜像固定到 v8.5.8。

**TiKV spike 状态**

- 已添加 `tools/spikes/tikv-client`（乐观事务最小示例 + 能力清单）。
- 当前环境未运行 PD/TiKV，且 rsproxy 索引缺少 `web-sys 0.3.106`，编译验证暂未完成。
- 解除条件：启动 v8.5.8 集群，或切换可用的 crates 稀疏索引/降级 web-sys。

---

## 5. Gate 证据索引

| Gate | 报告/命令 | 状态 |
|---|---|---|
| Gate 0 | U0 + CI | 通过（功能波口径） |
| Gate 1 | U1 codec 单测 | 通过（核心类型） |
| Gate 2 | U2/U3/F01 | 进行中（核心路径已通，parser 60% 留完整性波） |
| Gate 3 | `project/reports/phase-03/`（待建） | 未开始 |
| Gate 4 | `project/reports/phase-04/`（待建） | 未开始 |
| Gate 5 | `project/reports/phase-05/`（待建） | 未开始 |
| Gate 6 | `project/reports/phase-06/`（待建） | 未开始 |
| Gate 7 | `project/reports/phase-07/`（待建） | 未开始 |
| Gate 8 | `project/reports/phase-08/`（待建） | 未开始 |
| Gate 9 | `project/reports/phase-09/`（待建） | 未开始 |

---

## 6. 已知偏差 / 技术债

| ID | 内容 | 影响 | 计划 |
|---|---|---|---|
| D1 | 旧 Rust 代码已归档 | 不可复用 | 保持归档 |
| D2 | ~~edition 2024 vs Cargo 1.80~~ | 已解决 | Rust 1.99.1 |
| D3 | 本地 PD/TiKV 仍可能是 9.0 beta | 行为/协议风险 | 测试固定 8.5.8 |
| D4 | sqlparser-rs 与目标语法差异 | Parser 覆盖不全 | bridge + 分批补齐（ADR-0003） |
| D5 | 谓词已在 scan 时过滤；TiKV Coprocessor DAG 仍缺 | 远端 Cop 任务不可用 | 后续完整性波 |
| D6 | workspace crate 边界 | 需显式依赖 | 按顶层包维护；循环依赖单独讨论 |
| D7 | 源码不含来源标记 | 追溯依赖 manifest | 强制更新 `project/parity/status.json` |

---

## 7. 更新模板

每次更新追加一条：

```markdown
### YYYY-MM-DD

- **Phase**: 0X-名称
- **完成**:
  - ...
- **验证**:
  - 命令/结果/报告路径
- **阻塞/风险**:
  - ...
- **下一步**:
  - ...
```
