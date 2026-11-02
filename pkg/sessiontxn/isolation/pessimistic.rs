/// F13 core: pessimistic Repeatable Read (TiDB `PessimisticRRTxnContextProvider`).
/// Completeness wave / not this feature: blocking lock wait, deadlock
/// detection, NOWAIT/SKIP LOCKED, 2PC/TiKV pessimistic.
pub struct PessimisticRRTxnContextProvider;
