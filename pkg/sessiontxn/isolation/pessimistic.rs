/// F13 core: pessimistic Repeatable Read (TiDB `PessimisticRRTxnContextProvider`).
/// Completeness wave / not this feature: 2PC/TiKV pessimistic.
/// Blocking wait uses `SEALDB_LOCK_WAIT_MS` on MemStorage. Deadlock
/// is wait-for cycle detection (errno 1213).
pub struct PessimisticRRTxnContextProvider;
