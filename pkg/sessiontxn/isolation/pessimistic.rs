/// F13 core: pessimistic Repeatable Read (TiDB `PessimisticRRTxnContextProvider`).
/// Completeness wave / not this feature: blocking lock wait, deadlock
/// detection, NOWAIT/SKIP LOCKED, FOR SHARE, 2PC/TiKV pessimistic.
pub struct PessimisticRRTxnContextProvider;
