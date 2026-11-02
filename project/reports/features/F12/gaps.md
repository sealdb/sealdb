# F12 gaps

SAVEPOINT / ROLLBACK TO / RELEASE SAVEPOINT are leftover-closed on
MemStorage. TiKV savepoints stay `Error::unsupported` (tikv-client has no
savepoint API). `BEGIN PESSIMISTIC` / `SELECT FOR UPDATE` stay F13-owned.

Rejected via `Error::unsupported` in `pkg/parser/sqlparser_bridge/lower.rs`
and marked on `pkg/kv/kv.rs` (default trait) / `pkg/store/driver/mod.rs` /
`pkg/sessiontxn/isolation/optimistic.rs`.

- `BEGIN PESSIMISTIC` / `SELECT ... FOR UPDATE` (F13)
- COMMIT/ROLLBACK AND CHAIN
- `START TRANSACTION` isolation / READ ONLY / causal consistency
- Transactional DDL (DDL currently implicit-commits)
- Stale read / AS OF
- Optimistic retry, pipelined DML, 2PC against live TiKV TSO
- Read Committed / Serializable
- TiKV SAVEPOINT
