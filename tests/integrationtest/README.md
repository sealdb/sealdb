# SealDB SQL integration tests

TiDB-shaped testdata runner: SQL in, real Session execution, expected I/O in `r/*.result`.

```text
t/*.test     input scripts (mysql-tester dialect)
r/*.result   golden output (echoed SQL + TSV / Error line)
```

Each `.test` file gets a fresh in-process `Session` on `MemStorage`. No `--listen` and no live TiKV required.

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
# all suites (also `cargo test -p sealdb-integrationtest`)
./tests/integrationtest/run-tests.sh

# one suite
./tests/integrationtest/run-tests.sh -t basic

# rewrite goldens after an intentional behavior change
./tests/integrationtest/run-tests.sh -r
./tests/integrationtest/run-tests.sh -r -t basic
```

Or:

```bash
cargo run -p sealdb-integrationtest --
cargo run -p sealdb-integrationtest -- --record --case predicate
```

## Adding a case

1. Add `t/<name>.test` (or `t/<dir>/<name>.test`).
2. Record: `./tests/integrationtest/run-tests.sh -r -t <name>`.
3. Review `r/<name>.result` (do not check in unexpected errors).
4. Re-run without `-r`; `cargo test -p sealdb-integrationtest` must pass.

Do not dump the full TiDB `tests/integrationtest/t` tree: most suites use protocol/stats/features SealDB has not ported.

Table-scan column prune is not wrapped yet: `SELECT id FROM t` still emits every column. Prefer `SELECT *` (or Dual `SELECT 1 AS n`) until that leftover lands. Goldens must match current I/O, not hoped-for I/O.
