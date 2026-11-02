#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TIDB_ROOT="${TIDB_ROOT:-/Users/wslu/work/github/db/pingcap/tidb}"
if [[ ! -d "$TIDB_ROOT" && -d /home/wslu/work/github/db/pingcap/tidb ]]; then
  TIDB_ROOT=/home/wslu/work/github/db/pingcap/tidb
fi
FEATURE="${1:-}"
REPORT_DIR="$ROOT/project/reports/features/${FEATURE}"
CARGO_BIN="${CARGO_BIN:-$HOME/.rustup/toolchains/1.99.1-aarch64-apple-darwin/bin/cargo}"
export PATH="$(dirname "$CARGO_BIN"):$PATH"
export RUSTC="${RUSTC:-$HOME/.rustup/toolchains/1.99.1-aarch64-apple-darwin/bin/rustc}"

usage() {
  echo "usage: TIDB_ROOT=/path/to/tidb $0 <FEATURE-ID>   e.g. F01"
}

if [[ -z "$FEATURE" ]]; then
  usage
  exit 2
fi

mkdir -p "$REPORT_DIR"
STATUS=0

run() {
  local name="$1"; shift
  echo "== $name =="
  if "$@" >"$REPORT_DIR/${name}.log" 2>&1; then
    echo "PASS: $name"
  else
    echo "FAIL: $name (see $REPORT_DIR/${name}.log)"
    STATUS=1
  fi
}

run fmt cargo fmt --all --check
run clippy cargo clippy --workspace --all-targets -- -D warnings
run test cargo test --workspace
run itest cargo test -q -p sealdb-integrationtest --lib
run parity python3 "$ROOT/project/scripts/parity_check.py" \
  --tidb-root "$TIDB_ROOT" \
  --rust-root "$ROOT" \
  --strict \
  --json "$REPORT_DIR/parity.json"

if [[ -d "$ROOT/tools/oracle/testdata/${FEATURE}" ]]; then
  run oracle-suite "$ROOT/project/scripts/oracle_diff.sh" --suite "$FEATURE" --out "$REPORT_DIR/oracle.json"
fi

case "$FEATURE" in
  U3|F01)
    run execute-select1 cargo run -q -p sealdb-server-bin -- --execute "SELECT 1"
    ;;
  F02)
    run execute-createdb cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1"
    run execute-create-like-as-select cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.src (id INT, name VARCHAR(20), INDEX idx_name (name)); INSERT INTO db1.src VALUES (1, 'a'), (2, 'b'); CREATE TABLE db1.like_t LIKE db1.src; CREATE TABLE db1.ctas AS SELECT * FROM db1.src; SELECT * FROM db1.like_t; SELECT * FROM db1.ctas ORDER BY id"
    ;;
  F11)
    run execute-showdb cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT); SELECT TABLE_NAME FROM information_schema.tables WHERE TABLE_SCHEMA = 'db1'"
    run execute-show-create cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); SHOW CREATE TABLE db1.t"
    run execute-show-full-columns cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); SHOW FULL COLUMNS FROM db1.t"
    run execute-is-statistics cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20), INDEX idx_name (name)); SELECT INDEX_NAME FROM information_schema.statistics WHERE TABLE_SCHEMA = 'db1'; SHOW INDEX FROM db1.t; SHOW CHARSET"
    ;;
  F03)
    run execute-insert-select cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); CREATE TABLE db1.src (id INT, name VARCHAR(20)); INSERT INTO db1.src VALUES (1, 'a'), (2, 'b'); INSERT INTO db1.t SELECT * FROM db1.src; SELECT * FROM db1.t"
    run execute-rowcodec cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20), extra DOUBLE); INSERT INTO db1.t VALUES (1, 'a', 1.5), (2, NULL, 2.5); SELECT * FROM db1.t ORDER BY id"
    ;;
  F04)
    run execute-where cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b'); SELECT * FROM db1.t WHERE id BETWEEN 1 AND 2 AND name LIKE 'b%'"
    run execute-project cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b'); SELECT id AS i FROM db1.t WHERE id = 2"
    run execute-ppd cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b'); EXPLAIN SELECT * FROM db1.t WHERE name = 'b'"
    ;;
  F05)
    run execute-order-limit cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (3, 'c'), (1, 'a'); SELECT * FROM db1.t ORDER BY id LIMIT 1"
    run execute-sort-spill env SEALDB_SPILL_MEM_ROWS=1 cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (3, 'c'), (1, 'a'), (2, 'b'); SELECT * FROM db1.t ORDER BY id"
    run execute-topn cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (3, 'c'), (1, 'a'), (2, 'b'); EXPLAIN SELECT * FROM db1.t ORDER BY id LIMIT 1; SELECT * FROM db1.t ORDER BY id LIMIT 1"
    run execute-topn-spill env SEALDB_SPILL_MEM_ROWS=1 cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (4, 'd'), (1, 'a'), (3, 'c'), (2, 'b'); SELECT * FROM db1.t ORDER BY id LIMIT 3"
    run execute-parallel-sort env SEALDB_SORT_CONCURRENCY=2 cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (3, 'c'), (1, 'a'), (2, 'b'), (0, 'z'); SELECT * FROM db1.t ORDER BY id"
    ;;
  F06)
    run execute-point-get cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b'); SELECT * FROM db1.t WHERE id = 2"
    run execute-index-point-get cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20), extra VARCHAR(20), UNIQUE INDEX idx_name (name)); INSERT INTO db1.t VALUES (1, 'a', 'x'), (2, 'b', 'y'); EXPLAIN SELECT * FROM db1.t WHERE name = 'a'; SELECT * FROM db1.t WHERE name = 'a'"
    run execute-batch-point-get cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b'), (3, 'c'); EXPLAIN SELECT * FROM db1.t WHERE id IN (1, 3); SELECT * FROM db1.t WHERE id IN (1, 3) ORDER BY id"
    ;;
  F07)
    run execute-update-delete cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b'); UPDATE db1.t SET name = 'c' WHERE id = 1; DELETE FROM db1.t WHERE id = 2; SELECT * FROM db1.t"
    run execute-update-delete-order-limit cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b'), (3, 'c'); UPDATE db1.t SET name = 'z' ORDER BY id DESC LIMIT 1; DELETE FROM db1.t ORDER BY id LIMIT 1; SELECT * FROM db1.t ORDER BY id"
    run execute-update-join cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t1 (id INT, name VARCHAR(20)); CREATE TABLE db1.t2 (id INT, tag VARCHAR(20)); INSERT INTO db1.t1 VALUES (1, 'a'), (2, 'b'); INSERT INTO db1.t2 VALUES (1, 'x'), (2, 'y'); UPDATE db1.t1 JOIN db1.t2 ON t1.id = t2.id SET t1.name = t2.tag; DELETE db1.t1 FROM db1.t1 JOIN db1.t2 ON t1.id = t2.id WHERE t2.tag = 'x'; SELECT * FROM db1.t1 ORDER BY id"
    ;;
  F08)
    run execute-agg cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'a'), (3, 'b'); SELECT name, COUNT(*) FROM db1.t GROUP BY name HAVING COUNT(*) > 1 ORDER BY name"
    run execute-count-distinct cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'a'), (3, 'b'); SELECT COUNT(DISTINCT name) FROM db1.t"
    run execute-distinct-agg cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.d (name VARCHAR(20), id INT); INSERT INTO db1.d VALUES ('a', 1), ('a', 1), ('a', 2), ('b', 3); SELECT SUM(DISTINCT id), AVG(DISTINCT id), MIN(DISTINCT id), MAX(DISTINCT id) FROM db1.d"
    run execute-stream-agg cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT); INSERT INTO db1.t VALUES (1), (2); EXPLAIN SELECT COUNT(*) FROM db1.t"
    run execute-spill env SEALDB_SPILL_MEM_ROWS=1 cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'a'), (3, 'b'); SELECT name, COUNT(*) FROM db1.t GROUP BY name HAVING COUNT(*) > 1 ORDER BY name"
    ;;
  F09)
    run execute-join cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t1 (id INT, name VARCHAR(20)); CREATE TABLE db1.t2 (id INT, tag VARCHAR(20)); INSERT INTO db1.t1 VALUES (1, 'a'), (2, 'b'); INSERT INTO db1.t2 VALUES (1, 'x'), (3, 'y'); SELECT t1.id, t1.name, t2.tag FROM db1.t1 JOIN db1.t2 ON t1.id = t2.id"
    run execute-join-spill env SEALDB_SPILL_MEM_ROWS=1 cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t1 (id INT, name VARCHAR(20)); CREATE TABLE db1.t2 (id INT, tag VARCHAR(20)); INSERT INTO db1.t1 VALUES (1, 'a'), (2, 'b'); INSERT INTO db1.t2 VALUES (1, 'x'), (3, 'y'); SELECT t1.id, t1.name, t2.tag FROM db1.t1 JOIN db1.t2 ON t1.id = t2.id"
    run execute-cross-join cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t1 (id INT, name VARCHAR(20)); CREATE TABLE db1.t2 (id INT, tag VARCHAR(20)); INSERT INTO db1.t1 VALUES (1, 'a'), (2, 'b'); INSERT INTO db1.t2 VALUES (1, 'x'), (3, 'y'); SELECT t1.id, t2.tag FROM db1.t1 CROSS JOIN db1.t2 ORDER BY t1.id, t2.tag"
    run execute-n-table-join cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t1 (id INT, name VARCHAR(20)); CREATE TABLE db1.t2 (id INT, tag VARCHAR(20)); CREATE TABLE db1.t3 (id INT, note VARCHAR(20)); INSERT INTO db1.t1 VALUES (1, 'a'), (2, 'b'); INSERT INTO db1.t2 VALUES (1, 'x'), (3, 'y'); INSERT INTO db1.t3 VALUES (1, 'n'); SELECT t1.id, t2.tag, t3.note FROM db1.t1 JOIN db1.t2 ON t1.id = t2.id JOIN db1.t3 ON t2.id = t3.id"
    run execute-semi-join cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t1 (id INT, name VARCHAR(20)); CREATE TABLE db1.t2 (id INT, tag VARCHAR(20)); INSERT INTO db1.t1 VALUES (1, 'a'), (2, 'b'); INSERT INTO db1.t2 VALUES (1, 'x'), (3, 'y'); SELECT * FROM db1.t1 WHERE EXISTS (SELECT 1 FROM db1.t2 WHERE t2.id = t1.id) ORDER BY id; SELECT * FROM db1.t1 WHERE NOT EXISTS (SELECT 1 FROM db1.t2 WHERE t2.id = t1.id) ORDER BY id; SELECT * FROM db1.t1 WHERE id IN (SELECT id FROM db1.t2) ORDER BY id"
    ;;
  F10)
    run execute-index-lookup cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20), INDEX idx_name (name)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b'), (3, 'a'); SELECT * FROM db1.t WHERE name = 'a' ORDER BY id"
    run execute-unique-pk cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20), PRIMARY KEY (id), UNIQUE INDEX idx_name (name)); INSERT INTO db1.t VALUES (1, 'a'); INSERT INTO db1.t VALUES (2, 'b')"
    run execute-covering cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20), extra VARCHAR(20), INDEX idx_name (name)); INSERT INTO db1.t VALUES (1, 'a', 'x'), (2, 'b', 'y'), (3, 'a', 'z'); EXPLAIN SELECT name FROM db1.t WHERE name = 'a'; SELECT name FROM db1.t WHERE name = 'a' ORDER BY id; SELECT * FROM db1.t WHERE name = 'a' ORDER BY id"
    run execute-range-scan cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20), extra VARCHAR(20), INDEX idx_name (name)); INSERT INTO db1.t VALUES (1, 'a', 'x'), (2, 'b', 'y'), (3, 'c', 'z'); EXPLAIN SELECT * FROM db1.t WHERE name > 'a'; SELECT * FROM db1.t WHERE name > 'a' ORDER BY id; SELECT * FROM db1.t WHERE name BETWEEN 'a' AND 'b' ORDER BY id"
    ;;
  F12)
    run execute-txn cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); BEGIN; INSERT INTO db1.t VALUES (1, 'a'); COMMIT; SELECT * FROM db1.t"
    run execute-savepoint cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); BEGIN; INSERT INTO db1.t VALUES (1, 'a'); SAVEPOINT sp1; INSERT INTO db1.t VALUES (2, 'b'); ROLLBACK TO sp1; SELECT * FROM db1.t; RELEASE SAVEPOINT sp1; COMMIT"
    run execute-isolation cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'); START TRANSACTION READ ONLY; SELECT * FROM db1.t; ROLLBACK; START TRANSACTION READ WRITE; INSERT INTO db1.t VALUES (2, 'b'); COMMIT AND CHAIN; INSERT INTO db1.t VALUES (3, 'c'); COMMIT; SELECT * FROM db1.t ORDER BY id"
    run execute-txn-ddl cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT); BEGIN; CREATE TABLE db1.t2 (id INT); INSERT INTO db1.t2 VALUES (1); ROLLBACK; BEGIN; CREATE TABLE db1.t2 (id INT); INSERT INTO db1.t2 VALUES (1); COMMIT; SELECT * FROM db1.t2"
    ;;
  F13)
    run execute-for-update cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'); BEGIN PESSIMISTIC; SELECT * FROM db1.t FOR UPDATE; UPDATE db1.t SET name = 'b' WHERE id = 1; COMMIT; SELECT * FROM db1.t"
    run execute-for-share cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'); BEGIN PESSIMISTIC; SELECT * FROM db1.t FOR SHARE; SELECT * FROM db1.t LOCK IN SHARE MODE; COMMIT; SELECT * FROM db1.t"
    run execute-nowait-skip cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b'); BEGIN PESSIMISTIC; SELECT * FROM db1.t WHERE id = 1 FOR UPDATE; SELECT * FROM db1.t ORDER BY id FOR UPDATE SKIP LOCKED; COMMIT"
    run execute-blocking-wait cargo test -q -p sealdb-kv --lib blocking_wait
    run execute-deadlock cargo test -q -p sealdb-kv --lib deadlock
    run execute-tikv-pessimistic cargo test -q -p sealdb-session --lib tikv_pessimistic
    ;;
  F14)
    run execute-subquery cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t1 (id INT, name VARCHAR(20)); CREATE TABLE db1.t2 (id INT, tag VARCHAR(20)); INSERT INTO db1.t1 VALUES (1, 'a'), (2, 'b'); INSERT INTO db1.t2 VALUES (1, 'x'); SELECT * FROM db1.t1 WHERE EXISTS (SELECT 1 FROM db1.t2 WHERE t2.id = t1.id)"
    run execute-any-all cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t1 (id INT, name VARCHAR(20)); CREATE TABLE db1.t2 (id INT, tag VARCHAR(20)); INSERT INTO db1.t1 VALUES (1, 'a'), (2, 'b'); INSERT INTO db1.t2 VALUES (1, 'x'), (3, 'y'); SELECT * FROM db1.t1 WHERE id = ANY (SELECT id FROM db1.t2) ORDER BY id; SELECT * FROM db1.t1 WHERE id = SOME (SELECT id FROM db1.t2) ORDER BY id; SELECT * FROM db1.t1 WHERE id <> ALL (SELECT id FROM db1.t2) ORDER BY id; SELECT * FROM db1.t1 WHERE id > ANY (SELECT id FROM db1.t2) ORDER BY id; SELECT * FROM db1.t1 WHERE id > ALL (SELECT id FROM db1.t2) ORDER BY id"
    run execute-derived-table cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b'); SELECT * FROM (SELECT * FROM db1.t WHERE id = 1) d; SELECT x FROM (SELECT 2) AS d(x); SELECT * FROM (SELECT * FROM (SELECT id FROM db1.t WHERE id = 2) a) b"
    run execute-select-list-subquery cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t1 (id INT, name VARCHAR(20)); CREATE TABLE db1.t2 (id INT, tag VARCHAR(20)); INSERT INTO db1.t1 VALUES (1, 'a'), (2, 'b'); INSERT INTO db1.t2 VALUES (1, 'x'); SELECT (SELECT 1); SELECT id, (SELECT tag FROM db1.t2 WHERE t2.id = t1.id) FROM db1.t1 ORDER BY id"
    run execute-parallel-apply env SEALDB_APPLY_CONCURRENCY=2 cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t1 (id INT, name VARCHAR(20)); CREATE TABLE db1.t2 (id INT, tag VARCHAR(20)); INSERT INTO db1.t1 VALUES (1, 'a'), (2, 'b'); INSERT INTO db1.t2 VALUES (1, 'x'); SELECT * FROM db1.t1 WHERE EXISTS (SELECT 1 FROM db1.t2 WHERE t2.id = t1.id) ORDER BY id; SELECT * FROM db1.t1 WHERE id NOT IN (SELECT id FROM db1.t2) ORDER BY id"
    run execute-semi-join-rewrite cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t1 (id INT, name VARCHAR(20)); CREATE TABLE db1.t2 (id INT, tag VARCHAR(20)); INSERT INTO db1.t1 VALUES (1, 'a'), (2, 'b'); INSERT INTO db1.t2 VALUES (1, 'x'), (1, 'z'); SELECT * FROM db1.t1 WHERE EXISTS (SELECT 1 FROM db1.t2 WHERE t2.id = t1.id) ORDER BY id; EXPLAIN SELECT * FROM db1.t1 WHERE EXISTS (SELECT 1 FROM db1.t2 WHERE t2.id = t1.id)"
    ;;
  F15)
    run execute-union cargo run -q -p sealdb-server-bin -- --execute "SELECT 1 UNION SELECT 1 UNION ALL SELECT 2"
    run execute-except-intersect cargo run -q -p sealdb-server-bin -- --execute "SELECT 1 EXCEPT SELECT 2; SELECT 1 INTERSECT SELECT 1; SELECT 1 EXCEPT SELECT 1; SELECT 1 UNION ALL SELECT 1 EXCEPT ALL SELECT 1"
    run execute-nested-paren-union cargo run -q -p sealdb-server-bin -- --execute "SELECT 1 UNION (SELECT 2 UNION SELECT 3); (SELECT 1) UNION (SELECT 2); (SELECT 1 UNION SELECT 2) UNION SELECT 3"
    run execute-union-in-subquery cargo run -q -p sealdb-server-bin -- --execute "SELECT (SELECT 1 UNION SELECT 1); SELECT 1 WHERE EXISTS (SELECT 2 UNION SELECT 3); SELECT 1 WHERE 1 IN (SELECT 1 UNION SELECT 2); SELECT 1 WHERE 1 IN (SELECT 2 UNION SELECT 3)"
    run execute-parallel-union env SEALDB_UNION_CONCURRENCY=2 cargo run -q -p sealdb-server-bin -- --execute "SELECT 1 UNION SELECT 2 UNION SELECT 3; SELECT 1 UNION ALL SELECT 1; SELECT 1 EXCEPT SELECT 2; SELECT 1 INTERSECT SELECT 1"
    ;;
  F16)
    run execute-window cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'a'); SELECT id, ROW_NUMBER() OVER (PARTITION BY name ORDER BY id) FROM db1.t ORDER BY id"
    run execute-frame-clause cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'a'), (3, 'b'), (4, 'a'); SELECT id, SUM(id) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM db1.t ORDER BY id; SELECT id, SUM(id) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM db1.t ORDER BY id"
    run execute-named-window cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'a'), (3, 'b'); SELECT id, name, ROW_NUMBER() OVER w FROM db1.t WINDOW w AS (PARTITION BY name ORDER BY id) ORDER BY name, id"
    run execute-lag-lead cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'a'), (3, 'b'), (4, 'a'); SELECT id, LAG(id) OVER (ORDER BY id), LEAD(id) OVER (ORDER BY id), FIRST_VALUE(id) OVER (ORDER BY id) FROM db1.t ORDER BY id"
    run execute-ignore-nulls cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, NULL), (2, 'a'), (3, NULL); SELECT id, FIRST_VALUE(name) IGNORE NULLS OVER (ORDER BY id) FROM db1.t ORDER BY id"
    run execute-pipelined-window cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'a'), (3, 'b'), (4, 'a'); SELECT id, name, ROW_NUMBER() OVER (PARTITION BY name ORDER BY id) FROM db1.t ORDER BY name, id"
    ;;
  F17)
    run execute-cte cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b'); WITH c AS (SELECT * FROM db1.t WHERE id = 1) SELECT * FROM c"
    run execute-recursive-cte cargo run -q -p sealdb-server-bin -- --execute "WITH RECURSIVE c AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM c WHERE n < 3) SELECT * FROM c ORDER BY n"
    run execute-materialized-cte cargo run -q -p sealdb-server-bin -- --execute "WITH c AS MATERIALIZED (SELECT 1 AS n) SELECT * FROM c; WITH c AS NOT MATERIALIZED (SELECT 1 AS n) SELECT * FROM c"
    run execute-union-in-cte cargo run -q -p sealdb-server-bin -- --execute "WITH c AS (SELECT 1 AS n UNION SELECT 2) SELECT * FROM c ORDER BY n"
    run execute-cte-in-join cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'b'); WITH c AS (SELECT * FROM db1.t WHERE id = 1) SELECT t.id, t.name FROM db1.t JOIN c ON t.id = c.id"
    run execute-cte-in-dml cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); WITH c AS (SELECT 1 AS id, 'a' AS name) INSERT INTO db1.t SELECT * FROM c; WITH c AS (SELECT * FROM db1.t WHERE id = 1) UPDATE db1.t JOIN c ON t.id = c.id SET t.name = 'z'; SELECT * FROM db1.t"
    run execute-shared-cte cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'); WITH c AS (SELECT * FROM db1.t) SELECT * FROM c UNION ALL SELECT * FROM c"
    ;;
  F18)
    run execute-ddl cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'); CREATE INDEX idx_name ON db1.t (name); ALTER TABLE db1.t ADD COLUMN age INT; SELECT * FROM db1.t WHERE name = 'a'"
    run execute-ddl-jobs cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT); ADMIN SHOW DDL JOBS"
    run execute-reorg cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'); CREATE INDEX idx_name ON db1.t (name); ADMIN SHOW DDL JOBS"
    run execute-unique-index cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'); CREATE UNIQUE INDEX idx_id ON db1.t (id); ALTER TABLE db1.t ADD UNIQUE INDEX idx_name (name)"
    run execute-change-modify-rename cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20), age VARCHAR(8)); INSERT INTO db1.t VALUES (1, 'a', '10'); CREATE INDEX idx_name ON db1.t (name); ALTER TABLE db1.t CHANGE name title VARCHAR(20); ALTER TABLE db1.t MODIFY age INT; ALTER TABLE db1.t RENAME COLUMN title TO name; ALTER TABLE db1.t RENAME INDEX idx_name TO idx_title; ALTER TABLE db1.t RENAME TO t2; RENAME TABLE db1.t2 TO db1.t; SELECT name, age FROM db1.t"
    ;;
  F19)
    run execute-partition cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)) PARTITION BY RANGE (id) (PARTITION p0 VALUES LESS THAN (10), PARTITION p1 VALUES LESS THAN (MAXVALUE)); INSERT INTO db1.t VALUES (1, 'a'), (15, 'b'); SELECT * FROM db1.t WHERE id < 10"
    ;;
  F20)
    run execute-grant cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); GRANT SELECT ON db1.t TO 'u'@'%'; SHOW GRANTS FOR 'u'@'%'"
    ;;
  F21)
    run execute-analyze cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20), INDEX idx_name (name)); INSERT INTO db1.t VALUES (1, 'a'), (2, 'a'); ANALYZE TABLE db1.t; SELECT * FROM db1.t WHERE name = 'a'"
    ;;
  F22)
    run execute-com-query cargo test -q -p sealdb-server --lib test_com_query -- --exact
    ;;
  C1)
    run leftover python3 "$ROOT/project/scripts/leftover_inventory.py" \
      --tidb-root "$TIDB_ROOT" \
      --rust-root "$ROOT" \
      --out "$ROOT/project/features/leftover.md" \
      --json "$ROOT/project/features/leftover.json" \
      --check
    ;;
  C2)
    run leftover python3 "$ROOT/project/scripts/leftover_inventory.py" \
      --tidb-root "$TIDB_ROOT" \
      --rust-root "$ROOT" \
      --out "$ROOT/project/features/leftover.md" \
      --json "$ROOT/project/features/leftover.json" \
      --check
    run execute-explain cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'); EXPLAIN SELECT * FROM db1.t WHERE id = 1"
    ;;
  C3)
    run leftover python3 "$ROOT/project/scripts/leftover_inventory.py" \
      --tidb-root "$TIDB_ROOT" \
      --rust-root "$ROOT" \
      --out "$ROOT/project/features/leftover.md" \
      --json "$ROOT/project/features/leftover.json" \
      --check
    run execute-rc cargo test -q -p sealdb-session --lib test_schema_checker_and_read_committed
    ;;
  C4)
    run leftover python3 "$ROOT/project/scripts/leftover_inventory.py" \
      --tidb-root "$TIDB_ROOT" \
      --rust-root "$ROOT" \
      --out "$ROOT/project/features/leftover.md" \
      --json "$ROOT/project/features/leftover.json" \
      --check
    run execute-prepare cargo run -q -p sealdb-server-bin -- --execute "CREATE DATABASE db1; CREATE TABLE db1.t (id INT, name VARCHAR(20)); INSERT INTO db1.t VALUES (1, 'a'); PREPARE s FROM 'SELECT name FROM db1.t WHERE id = ?'; EXECUTE s USING 1; DEALLOCATE PREPARE s"
    ;;
  C5)
    run leftover python3 "$ROOT/project/scripts/leftover_inventory.py" \
      --tidb-root "$TIDB_ROOT" \
      --rust-root "$ROOT" \
      --out "$ROOT/project/features/leftover.md" \
      --json "$ROOT/project/features/leftover.json" \
      --check
    run execute-collation cargo test -q -p sealdb-session --lib test_set_names_and_ci_collation
    ;;
esac

if [[ ! -f "$REPORT_DIR/corpus.lock.json" ]]; then
  echo "FAIL: missing $REPORT_DIR/corpus.lock.json"
  STATUS=1
fi
if [[ ! -f "$REPORT_DIR/gaps.md" ]]; then
  echo "FAIL: missing $REPORT_DIR/gaps.md"
  STATUS=1
fi

cat >"$REPORT_DIR/summary.md" <<EOF
# Feature $FEATURE Gate Report

- Date: $(date -u +%Y-%m-%dT%H:%M:%SZ)
- Rust: $(rustc --version 2>/dev/null || echo unknown)
- TiDB root: $TIDB_ROOT

## Conclusion

$(if [[ "$STATUS" -eq 0 ]]; then echo "PASS"; else echo "FAIL"; fi)
EOF

echo
echo "report: $REPORT_DIR/summary.md"
exit "$STATUS"
