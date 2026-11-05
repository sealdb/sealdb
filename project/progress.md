# 进度看板（Progress）

> **唯一进度事实来源 = `project/regression/manifest.json` 中 suite `green`。**  
> 功能波 `core-ported` / 包级 parity **不再**计为生产对齐进度。  
> 最后更新：2026-11-06（R1 `select` group-by StreamAgg→IndexReader Gate A+B 已绿）  
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
| R1 | select, show, types/* | `in_progress` | **`select`/`show` → `green`**；下一：`types/*` triage |
| R2 | join / subquery / agg / window / cte | `in_progress` | 全波套件已标 `in_progress`；跟 triage 红灯移植 |
| R3 | point_get / index / partition / explain* | `in_progress` | 首批 29 套件 `in_progress`；与 R1 EXPLAIN 形状对齐联动 |
| R4–R7 | plan_cache / session / ddl / CBO | `pending` | 依赖前波 green |
| D | *tiflash* / *mpp* | `deferred` | 不丢弃，延后 |

实现纪律：[parity-discipline.md](./parity-discipline.md) — 失败用例 → TiDB 同名包/文件 → SealDB 同路径一比一；生产级清单勾满才升 `green`；禁止改权威 `r/*.result`。

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
