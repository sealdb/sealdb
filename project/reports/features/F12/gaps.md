# F12 gaps

These stay out of the core-ported door. Pick them up in the completeness wave
or F13. Rejected via `Error::unsupported` in
`pkg/parser/sqlparser_bridge/lower.rs` and marked on
`pkg/kv/mem.rs` / `pkg/sessiontxn/txn_manager.rs` /
`pkg/sessiontxn/isolation/optimistic.rs`.

- `BEGIN PESSIMISTIC` / `SELECT ... FOR UPDATE` (F13)
- SAVEPOINT / ROLLBACK TO / RELEASE SAVEPOINT
- COMMIT/ROLLBACK AND CHAIN
- `START TRANSACTION` isolation / READ ONLY / causal consistency
- Transactional DDL (DDL currently implicit-commits)
- Stale read / AS OF
- Optimistic retry, pipelined DML, 2PC against live TiKV TSO
- Read Committed / Serializable
