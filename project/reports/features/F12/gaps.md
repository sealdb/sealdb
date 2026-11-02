# F12 gaps

SAVEPOINT, READ ONLY / READ WRITE, and COMMIT/ROLLBACK AND CHAIN are
leftover-closed. RC statement snapshots stay C3. `BEGIN PESSIMISTIC` /
`SELECT FOR UPDATE` stay F13-owned. SERIALIZABLE stays rejected.

Rejected via `Error::unsupported` in `pkg/parser/sqlparser_bridge/lower.rs`
and marked on `pkg/kv/kv.rs` / `pkg/store/driver/mod.rs` /
`pkg/sessiontxn/isolation/optimistic.rs`.

- `BEGIN PESSIMISTIC` / `SELECT ... FOR UPDATE` (F13)
- Transactional DDL (DDL currently implicit-commits)
- Causal consistency / SET TRANSACTION SNAPSHOT / AS OF
- SERIALIZABLE / READ UNCOMMITTED
- Optimistic retry, pipelined DML, 2PC against live TiKV TSO
- TiKV SAVEPOINT
