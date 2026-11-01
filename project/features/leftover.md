# Leftover inventory

Baseline features core-ported: C1, C2, F01, F02, F03, F04, F05, F06, F07, F08, F09, F10, F11, F12, F13, F14, F15, F16, F17, F18, F19, F20, F21, F22, U0, U1, U2, U3

C1 freezes this file plus `project/features/leftover.json` as the completeness denominator.
Do not claim leftover pass rates until a later wave marks a subset `ported`.

## Feature gaps still open

### F02

- `online-ddl-jobs`
- `create-like-as-select`
- `indexes`
- `tikv-persist`

### F03

- `tikv-persist`
- `rowcodec`
- `insert-select`

### F04

- `ppd`
- `in-between-like`
- `coprocessor-filter`

### F05

- `topn-spill`
- `parallel-sort`

### F06

- `batch-point-get`
- `index-point-get`

### F07

- `multi-table`
- `update-join`
- `order-limit`
- `returning`

### F08

- `having`
- `distinct-agg`
- `stream-agg`
- `spill`

### F09

- `semi-anti`
- `cross-join`
- `n-table`
- `spill`

### F10

- `unique-pk`
- `covering-reader`
- `range-scan`

### F11

- `information_schema-tables`
- `show-create`
- `show-full-columns`

### F12

- `pessimistic`
- `savepoint`
- `isolation-modes`
- `transactional-ddl`

### F13

- `for-share`
- `nowait-skip-locked`
- `blocking-wait`
- `deadlock`
- `tikv-pessimistic`

### F14

- `any-all`
- `from-derived-table`
- `select-list-subquery`
- `parallel-apply`
- `semi-anti-rewrite`

### F15

- `except-intersect`
- `nested-paren-union`
- `union-in-subquery`
- `parallel-union`

### F16

- `frame-clause`
- `named-window`
- `lag-lead`
- `ignore-nulls`
- `pipelined-window`

### F17

- `recursive-cte`
- `materialized-hint`
- `union-in-cte`
- `cte-in-join`
- `cte-in-dml`
- `shared-cte-storage`

### F18

- `ddl-job-states`
- `reorg`
- `unique-index`
- `change-modify-rename`
- `after-first`
- `algorithm-lock`

### F19

- `list-key`
- `range-columns`
- `expr-keys`
- `add-drop-partition`
- `subpartition`
- `dynamic-prune`

### F20

- `roles`
- `with-grant-option`
- `mysql-user-persist`
- `column-grants`
- `dynamic-privs`
- `create-user`

### F21

- `histogram`
- `topn`
- `auto-analyze`
- `analyze-index`
- `persist-stats`
- `join-cost`

### F22

- `ssl`
- `compression`
- `prepared-stmt`
- `auth-plugin`
- `binary-protocol`
- `multi-resultset`

## PlanBuilder statement kinds still open

- `ast.AdminStmt` → unmapped
- `ast.DeallocateStmt` → C4
- `ast.ExecuteStmt` → C4
- `ast.ExplainStmt` → C2
- `ast.ExplainForStmt` → C2
- `ast.TraceStmt` → unmapped
- `ast.ImportIntoStmt` → C6
- `ast.LoadDataStmt` → C6
- `ast.LoadStatsStmt` → unmapped
- `ast.LockStatsStmt` → unmapped
- `ast.UnlockStatsStmt` → unmapped
- `ast.PlanReplayerStmt` → unmapped
- `ast.PrepareStmt` → C4
- `ast.DoStmt` → unmapped
- `ast.SetStmt` → C5
- `ast.SetConfigStmt` → C5
- `ast.BinlogStmt` → unmapped
- `ast.CreateBindingStmt` → C4
- `ast.DropBindingStmt` → C4
- `ast.SetBindingStmt` → C4
- `ast.ChangeStmt` → unmapped
- `ast.SplitRegionStmt` → unmapped
- `ast.DistributeTableStmt` → unmapped
- `ast.CompactTableStmt` → unmapped
- `ast.RecommendIndexStmt` → unmapped
- `ast.FlushStmt` → unmapped
- `ast.AlterInstanceStmt` → unmapped
- `ast.RenameUserStmt` → unmapped
- `ast.BRIEStmt` → C6
- `ast.CalibrateResourceStmt` → C6
- `ast.AddQueryWatchStmt` → unmapped
- `ast.DropQueryWatchStmt` → unmapped
- `ast.GrantRoleStmt` → unmapped
- `ast.RevokeRoleStmt` → unmapped
- `ast.KillStmt` → unmapped
- `ast.DropUserStmt` → unmapped
- `ast.SetPwdStmt` → unmapped
- `ast.ShutdownStmt` → unmapped
- `ast.SetResourceGroupStmt` → unmapped
- `ast.AlterDatabaseStmt` → unmapped
- `ast.AlterSequenceStmt` → unmapped
- `ast.CreateViewStmt` → unmapped
- `ast.CreateSequenceStmt` → unmapped
- `ast.DropSequenceStmt` → unmapped
- `ast.TruncateTableStmt` → unmapped
- `ast.RenameTableStmt` → unmapped
- `ast.RecoverTableStmt` → unmapped
- `ast.FlashBackTableStmt` → unmapped
- `ast.FlashBackDatabaseStmt` → unmapped
- `ast.FlashBackToTimestampStmt` → unmapped
- `ast.LockTablesStmt` → unmapped
- `ast.CleanupTableLockStmt` → unmapped
- `ast.RepairTableStmt` → unmapped
- `ast.DropPlacementPolicyStmt` → unmapped
- `ast.CreateResourceGroupStmt` → unmapped
- `ast.OptimizeTableStmt` → unmapped

## integrationtest leftover suites

- `access_path_selection` → C2
- `agg_predicate_pushdown` → C2
- `black_list` → C6
- `clustered_index` → C2
- `collation_agg_func` → C5
- `collation_check_use_collation` → C5
- `collation_misc` → C5
- `collation_pointget` → C5
- `common_collation` → C5
- `cte` → F17
- `db_integration` → C3
- `explain` → C2
- `explain-non-select-stmt` → C2
- `explain_complex` → C2
- `explain_complex_stats` → C2
- `explain_cte` → C2
- `explain_easy` → C2
- `explain_easy_stats` → C2
- `explain_foreign_key` → C2
- `explain_generate_column_substitute` → C2
- `explain_indexmerge_stats` → C2
- `explain_join_stats` → C2
- `explain_shard_index` → C2
- `explain_stats` → C2
- `explain_union_scan` → C2
- `generated_columns` → C2
- `imdbload` → C2
- `index_join` → C2
- `index_merge` → C2
- `naaj` → C2
- `new_character_set` → C5
- `new_character_set_builtin` → C5
- `new_character_set_invalid` → C5
- `null_rejected` → C2
- `partition` → F19
- `select` → F04
- `show` → F11
- `subquery` → F14
- `topn_push_down` → F05
- `topn_pushdown` → F05
- `tpch` → C2
- `window_function` → F16

## Completeness wave notes

- C1 is the frozen leftover denominator, not a pass-rate claim.
- C2 core-ports EXPLAIN row/json/dot, costed skyline, and two-table JoinReorder; ANALYZE / n-table DP / IndexMerge stay open.
- C3 owns isolation / SchemaChecker / `db_integration`.
- C4 owns Prepare / Execute / Binding.
- C5 owns SET / collation / charset leftover.
- C6 stays deferred: Cascades, TiFlash/MPP, BR/Lightning, plugins.
