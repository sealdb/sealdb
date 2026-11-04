# 进度看板（Progress）

> **唯一进度事实来源**。每次工作结束必须更新；没有证据 = 未完成。  
> 最后更新：2026-11-04（C6 S3 BRIE；下一步 cascades more rules / mpp-multi-sender）
> 功能级状态：[features/status.json](./features/status.json)

---

## 1. 状态图例

| 状态 | 含义 |
|---|---|
| `not-started` | 尚未开始 |
| `in-progress` | 进行中（必须写负责人/下一步） |
| `blocked` | 被阻塞（必须写阻塞原因/解除条件） |
| `review` | 开发完成，等待 Gate 评审 |
| `done` | Gate 通过，有证据 |
| `deferred` | 本阶段不做（记录到后续阶段） |

---

## 2. 阶段状态

| Phase | 名称 | 状态 | 开始 | 完成 | Gate 证据 | 备注 |
|---|---|---|---|---|---|---|
| 0 | 地基与规范 | `done` | 2026-10-19 | 2026-11-01 | U0 | workspace/CI/parity/spike 书面结论 |
| 1 | 核心类型与编码 | `done` | 2026-11-01 | 2026-11-01 | U1 | int/uint/float/bytes + row/index key；Decimal/Time 核心路径 |
| 2 | 解析器 + Mock 链路 | `done` | 2026-11-01 | 2026-11-01 | U2/U3/F01 | 核心路径 `core-ported`；parser 全量边角留完整性波 |
| 3 | 元数据与 Bootstrap | `done` | 2026-11-01 | 2026-11-01 | F02/F11 | SHOW/DESC 走内存 InfoSchema |
| 4 | Planner v1（RBO） | `done` | 2026-11-01 | 2026-11-02 | F03–F10 | F03–F10 `core-ported` |
| 5 | Executor v1 | `done` | 2026-11-01 | 2026-11-02 | F03–F10 | 默认 MemStorage，`--store tikv` 可写真实集群 |
| 6 | 事务与 Session | `done` | 2026-11-01 | 2026-11-02 | F12/F13 | F12 乐观 RR + F13 悲观 FOR UPDATE 已通 |
| 7 | CBO 与统计 | `done` | 2026-11-01 | 2026-11-03 | F21 | ANALYZE + 直方图/TopN + persist + auto-analyze + join cost；gaps `[]` |
| 8 | DDL 与高级 SQL | `done` | 2026-11-01 | 2026-11-02 | F14–F20 | F14–F20 `core-ported`；gaps `[]` |
| 9 | MySQL 协议与运维 | `done` | 2026-11-01 | 2026-11-03 | F22 | SSL/compress/COM_STMT/auth/multi-resultset；gaps `[]` |
| 10 | 完整性 + C6 生产对齐 | `in-progress` | 2026-11-03 | — | C1–C5；C6 进行中 | C1–C5 gaps `[]`；C6-B PlanBuilder stmts 收口；下一步 C(PD+TiKV 全量)→D(TiFlash) |

### 2.1 功能波进度

| ID | 状态 | 证据 |
|---|---|---|
| U0–U3 | `core-ported` | workspace / codec tests / `--parse` / `SELECT 1` |
| F01 | `core-ported` | `project/reports/features/F01/` |
| F02 | `core-ported` | `project/reports/features/F02/` |
| F11 | `core-ported` | `project/reports/features/F11/` |
| F03 | `core-ported` | `project/reports/features/F03/` |
| F04 | `core-ported` | `project/reports/features/F04/` |
| F05 | `core-ported` | `project/reports/features/F05/` |
| F06 | `core-ported` | `project/reports/features/F06/` |
| F07 | `core-ported` | `project/reports/features/F07/` |
| F08 | `core-ported` | `project/reports/features/F08/` |
| F09 | `core-ported` | `project/reports/features/F09/` |
| F10 | `core-ported` | `project/reports/features/F10/` |
| F12 | `core-ported` | `project/reports/features/F12/` |
| F13 | `core-ported` | `project/reports/features/F13/` |
| F14 | `core-ported` | `project/reports/features/F14/` |
| F15 | `core-ported` | `project/reports/features/F15/` |
| F16 | `core-ported` | `project/reports/features/F16/` |
| F17 | `core-ported` | `project/reports/features/F17/` |
| F18 | `core-ported` | `project/reports/features/F18/` |
| F19 | `core-ported` | `project/reports/features/F19/` |
| F20 | `core-ported` | `project/reports/features/F20/` |
| F21 | `core-ported` | `project/reports/features/F21/` |
| F22 | `core-ported` | `project/reports/features/F22/` |
| C1 | `core-ported` | `project/reports/features/C1/` |
| C2 | `core-ported` | `project/reports/features/C2/` |
| C3 | `core-ported` | `project/reports/features/C3/` |
| C4 | `core-ported` | `project/reports/features/C4/` |
| C5 | `core-ported` | `project/reports/features/C5/` |
| C6 | `in-progress` | C/D：MPP exchange + S3 BRIE + cascades/HTTP；下一步 more rules / multi-sender |

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
