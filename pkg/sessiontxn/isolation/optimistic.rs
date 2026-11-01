/// F12 core: optimistic Repeatable Read (TiDB `OptimisticTxnContextProvider`).
/// Completeness wave / not this feature: retry, pipelined DML, causal
/// consistency, stale read. Pessimistic RR is F13.
pub struct OptimisticTxnContextProvider;
