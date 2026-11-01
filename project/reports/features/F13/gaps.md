# F13 gaps

These stay out of the core-ported door. Pick them up in the completeness
wave or C3. Rejected via `Error::unsupported` in
`pkg/parser/sqlparser_bridge/lower.rs` and marked on
`pkg/kv/mem.rs` / `pkg/executor/table_reader.rs` /
`pkg/sessiontxn/isolation/pessimistic.rs`.

- `FOR SHARE` / `LOCK IN SHARE MODE`
- `FOR UPDATE NOWAIT` / `SKIP LOCKED` / `WAIT N` / `OF table`
- Blocking lock wait (conflict returns 1205 immediately)
- Deadlock detection
- TiKV / 2PC pessimistic lock protocol
- Isolation switching after BEGIN
