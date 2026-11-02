/// F13 core: pessimistic Repeatable Read (TiDB `PessimisticRRTxnContextProvider`).
/// Completeness wave / not this feature: blocking lock wait, deadlock
/// detection, 2PC/TiKV pessimistic.
pub struct PessimisticRRTxnContextProvider;
