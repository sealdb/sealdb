# F13 gaps

`FOR SHARE` / `LOCK IN SHARE MODE` / `NOWAIT` / `SKIP LOCKED` are
leftover-closed on MemStorage. TiKV maps share/wait via trait
defaults (exclusive, ignore wait). `BEGIN PESSIMISTIC` /
`SELECT FOR UPDATE` stay core-ported.

Rejected via `Error::unsupported` in
`pkg/parser/sqlparser_bridge/lower.rs` and marked on
`pkg/kv/mem.rs` / `pkg/executor/table_reader.rs` /
`pkg/sessiontxn/isolation/pessimistic.rs`.

- `WAIT N` / `FOR UPDATE OF table`
- `innodb_lock_wait_timeout` (MemStorage uses `SEALDB_LOCK_WAIT_MS`, default 50ms)
- Deadlock detection
- TiKV / 2PC pessimistic lock protocol
- Isolation switching after BEGIN
