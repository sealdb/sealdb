# Regression waves (TDD)

Progress unit = **suite `green`** against TiDB v8.5.8 goldens (`r/*.result`), not package `core-ported`.

| Wave | Focus | Manifest |
|---|---|---|
| R1 | select / show / types — expression + executor | `wave=R1` |
| R2 | join / subquery / agg / window / cte | `wave=R2` |
| R3 | point_get / index / partition / explain | `wave=R3` |
| R4–R7 | plan_cache, session, ddl, CBO leftovers | pending |
| D | TiFlash / MPP / TiSpark | `deferred` |

## Loop

```bash
./project/scripts/triage_itest.sh --wave R1 --status in_progress -s memory
# map fail → TiDB pkg/file → port same-named .rs → re-run suite
```

## Rules

- Goldens only from `project/scripts/record_tidb_itest.sh` (TiDB).
- SealDB `--record` forbidden except `t/local/` + `SEALDB_ITEST_ALLOW_SEALDB_RECORD=1`.
- Gate A: result set + errno. Gate B: EXPLAIN shape (same suite, no long exemption).
- No `ported` in parity without a `green` suite citing the file.
