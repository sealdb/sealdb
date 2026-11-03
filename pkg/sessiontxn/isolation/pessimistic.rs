/// pessimistic Repeatable Read (TiDB `PessimisticRRTxnContextProvider`).
/// Blocking wait uses `SEALDB_LOCK_WAIT_MS` on MemStorage. Deadlock is
/// wait-for cycle detection (errno 1213). TiKV uses `begin_pessimistic`
/// + `get_for_update`; async/pipelined 2PC stays in gaps.
pub struct PessimisticRRTxnContextProvider;
