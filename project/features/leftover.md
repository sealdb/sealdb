# Leftover inventory

Baseline features core-ported: C1, C2, C3, C4, C5, F01, F02, F03, F04, F05, F06, F07, F08, F09, F10, F11, F12, F13, F14, F15, F16, F17, F18, F19, F20, F21, F22, U0, U1, U2, U3

C1 freezes this file plus `project/features/leftover.json` as the completeness denominator.
Do not claim leftover pass rates until a later wave marks a subset `ported`.

## Feature gaps still open

- none

## Completeness gaps still open

- none

## Frozen unmapped PlanBuilder allowlist

These kinds stay `unmapped` on purpose (not claimed by F*/C2–C5, not C6).
C1 `--check` requires open `unmapped` stmts to equal this set.

- none

## PlanBuilder statement kinds still open

- `ast.TraceStmt` → C6
- `ast.ImportIntoStmt` → C6
- `ast.LoadStatsStmt` → C6
- `ast.LockStatsStmt` → C6
- `ast.UnlockStatsStmt` → C6
- `ast.PlanReplayerStmt` → C6
- `ast.SplitRegionStmt` → C6
- `ast.DistributeTableStmt` → C6
- `ast.CompactTableStmt` → C6
- `ast.RecommendIndexStmt` → C6
- `ast.BRIEStmt` → C6
- `ast.CalibrateResourceStmt` → C6
- `ast.AddQueryWatchStmt` → C6
- `ast.DropQueryWatchStmt` → C6
- `ast.SetResourceGroupStmt` → C6
- `ast.RecoverTableStmt` → C6
- `ast.FlashBackTableStmt` → C6
- `ast.FlashBackDatabaseStmt` → C6
- `ast.FlashBackToTimestampStmt` → C6
- `ast.DropPlacementPolicyStmt` → C6
- `ast.CreateResourceGroupStmt` → C6

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
- Feature (F*) gaps are empty; C1–C5 completeness gaps closed.
- C2: completeness gaps closed (Cost Model v2 MVB + IndexMerge OR-union).
- C3: completeness gaps closed (MDL + schema-lease 8027 + SERIALIZABLE→RR + SAVEPOINT F12 + same-cluster TiKV cross-session when SEALDB_TIKV=1).
- C4: completeness gaps closed (template-keyed plan cache / COM_STMT_* / user-vars / BINDING).
- C5: completeness gaps closed (unicode-weights + gbk-latin1 + only-full-group-by + set-global + leftover collation suites MVB; new_character_set* exercised by test_leftover_collation_suites).
- Leftover suite execution: suites are owner-classified; C2 `explain*` / `index_merge` MVB via `execute-explain*`; C5 collation / `new_character_set*` via `execute-leftover-collation-suites`. Full integrationtest port is not claimed.
- C6 is production parity: package/file align TiDB compute; order A/B → C (compute + full PD/TiKV client) → D (TiFlash; deferred not dropped). Closed C6 stmts: C6_CLOSED_STMTS. Open stmts = B backlog.
