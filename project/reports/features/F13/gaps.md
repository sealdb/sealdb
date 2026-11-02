# F13 gaps

`FOR SHARE` / `LOCK IN SHARE MODE` are leftover-closed as shared row
locks on MemStorage (compatible with other shares, conflict with
`FOR UPDATE`). TiKV maps share to exclusive via the default trait
methods. `BEGIN PESSIMISTIC` / `SELECT FOR UPDATE` stay core-ported.

Rejected via `Error::unsupported` in
`pkg/parser/sqlparser_bridge/lower.rs` and marked on
`pkg/kv/mem.rs` / `pkg/executor/table_reader.rs` /
`pkg/sessiontxn/isolation/pessimistic.rs`.

- `FOR UPDATE NOWAIT` / `SKIP LOCKED` / `WAIT N` / `OF table`
- Blocking lock wait (conflict returns 1205 immediately)
- Deadlock detection
- TiKV / 2PC pessimistic lock protocol
- Isolation switching after BEGIN
