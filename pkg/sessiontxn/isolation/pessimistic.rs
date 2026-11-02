/// F13 core: pessimistic Repeatable Read (TiDB `PessimisticRRTxnContextProvider`).
/// Completeness wave / not this feature: deadlock detection, 2PC/TiKV
/// pessimistic. Blocking wait uses `SEALDB_LOCK_WAIT_MS` on MemStorage.
pub struct PessimisticRRTxnContextProvider;
