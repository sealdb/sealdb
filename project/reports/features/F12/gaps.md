# F12 gaps

SAVEPOINT, READ ONLY / READ WRITE, COMMIT/ROLLBACK AND CHAIN, and
transactional CREATE/DROP DATABASE|TABLE are leftover-closed. RC
statement snapshots stay C3. `BEGIN PESSIMISTIC` / `SELECT FOR UPDATE`
stay F13-owned. SERIALIZABLE stays rejected. CREATE INDEX / ALTER
backfill KV is not rolled back (F18). SAVEPOINT does not undo DDL.

Rejected via `Error::unsupported` in `pkg/parser/sqlparser_bridge/lower.rs`
and marked on `pkg/kv/kv.rs` / `pkg/store/driver/mod.rs` /
`pkg/sessiontxn/isolation/optimistic.rs`.

- `BEGIN PESSIMISTIC` / `SELECT ... FOR UPDATE` (F13)
- CREATE INDEX / ALTER backfill KV on ROLLBACK (F18)
- Causal consistency / SET TRANSACTION SNAPSHOT / AS OF
- SERIALIZABLE / READ UNCOMMITTED
- Optimistic retry, pipelined DML, 2PC against live TiKV TSO
- TiKV SAVEPOINT
