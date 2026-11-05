# SealDB SQL integration tests (TDD / TiDB parity)

TiDB-shaped testdata runner: SQL in, Session execution, expected I/O in `r/**/*.result`.

```text
t/**/*.test     input scripts (mysql-tester dialect)
r/**/*.result   golden output (echoed SQL + TSV / Error line) — TiDB-authoritative
s/*.json        stats dumps (from TiDB s.zip)
t/local/        SealDB smoke suites (non-TiDB goldens; status=local)
project/regression/manifest.json   suite status machine
```

## Golden policy (hard)

| Action | Allowed? |
|---|---|
| Refresh goldens with **TiDB** `project/scripts/record_tidb_itest.sh` | **Yes** (authoritative) |
| `sealdb-itest --record` / `run-tests.sh -r` on TiDB corpus | **No** |
| SealDB record for `t/local/*` only | Only with `SEALDB_ITEST_ALLOW_SEALDB_RECORD=1` |

Import / refresh corpus:

```bash
./project/scripts/import_tidb_itest.sh
./project/scripts/record_tidb_itest.sh -t select -r   # needs tidb-server
```

## Manifest statuses

| Status | Default CI | Meaning |
|---|---|---|
| `local` | runs | SealDB smoke under `t/local/` |
| `green` | runs | Matches TiDB golden |
| `in_progress` | opt-in | Active TDD wave (`SEALDB_ITEST_RUN_IN_PROGRESS=1` or `--status in_progress`) |
| `pending` | opt-in | Imported, not started (`SEALDB_ITEST_RUN_PENDING=1`) |
| `deferred` | skip | TiFlash / MPP / TiSpark |

Waves: `R1`…`R7`, `D`, `local` (see manifest `wave` field).

## Directives

| Line | Meaning |
|---|---|
| `# ...` | Comment |
| `-- error N` / `--error N` | Next statement must fail with errno `N` |
| `-- sorted_result` | Sort result rows before compare |
| `SQL;` | Statement |

Unsupported (suite gets `load_error` until ported): `connect` / `connection`, sleep, query-log toggles, etc.

## Run

```bash
# CI gate (green + local)
./tests/integrationtest/run-tests.sh -s memory

# one local suite
./tests/integrationtest/run-tests.sh -t local/basic -s memory

# TDD: triage a pending wave (expect failures)
./tests/integrationtest/run-tests.sh --wave R1 --status pending -s memory -t select
```

## Adding a TiDB-aligned case

1. Ensure suite exists in TiDB `tests/integrationtest` and re-run `import_tidb_itest.sh`.
2. Record/verify golden with `record_tidb_itest.sh -t <name> -r`.
3. Set manifest status `in_progress`, fix SealDB until Gate A/B pass.
4. Promote to `green`. Never edit TiDB goldens by hand to match SealDB.
