# SealDB SQL integration tests

TiDB-shaped testdata runner: SQL in, real Session execution, expected I/O in `r/*.result`.

```text
t/*.test     input scripts (mysql-tester dialect)
r/*.result   golden output (echoed SQL + TSV / Error line)
```

Each `.test` file runs twice: a fresh `Session` on **MemStorage**, then the same script on **TiKV** (PD `127.0.0.1:2379` by default). SQL I/O goldens are store-independent. TiKV sessions seed unique table IDs so leftover row keys from a previous process do not leak into `SELECT *`. If PD is down, TiKV is skipped unless `--require-tikv` / `SEALDB_ITEST_REQUIRE_TIKV=1`.

## Directives

| Line | Meaning |
|---|---|
| `# ...` | Comment, dropped |
| `-- error N` / `--error N` | Next statement must fail with errno `N` |
| `-- sorted_result` | Sort result rows before writing/comparing |
| `SQL;` | Statement, including multi-line |

Unsupported (deferred): `connect` / `connection` multi-session, mysql-tester over `--listen`, `s/*.json` stats.

Result format:

```text
SELECT 1 AS n;
n
1
SELECT * FROM missing.t;
Error 1146 (42S02): table missing.t does not exist
```

## Run

```bash
# both stores (also `cargo test -p sealdb-integrationtest`)
./tests/integrationtest/run-tests.sh

# one backend
./tests/integrationtest/run-tests.sh -s memory
./tests/integrationtest/run-tests.sh -s tikv --require-tikv

# one suite on both stores
./tests/integrationtest/run-tests.sh -t basic

# rewrite goldens from MemStorage, then verify on TiKV
./tests/integrationtest/run-tests.sh -r
```

Or:

```bash
cargo run -p sealdb-integrationtest -- --store both
cargo test -p sealdb-integrationtest --lib tests::test_all_suites_memory -- --exact
cargo test -p sealdb-integrationtest --lib tests::test_all_suites_tikv -- --exact
```

## Adding a case

1. Add `t/<name>.test` (or `t/<dir>/<name>.test`).
2. Record: `./tests/integrationtest/run-tests.sh -r -t <name>`.
3. Review `r/<name>.result` (do not check in unexpected errors).
4. Re-run without `-r`; `cargo test -p sealdb-integrationtest` must pass.

Do not dump the full TiDB `tests/integrationtest/t` tree: most suites use protocol/stats/features SealDB has not ported.

Table-scan column prune is not wrapped yet: `SELECT id FROM t` still emits every column. Prefer `SELECT *` (or Dual `SELECT 1 AS n`) until that leftover lands. Goldens must match current I/O, not hoped-for I/O.
