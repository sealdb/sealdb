/// optimistic Repeatable Read (TiDB `OptimisticTxnContextProvider`).
/// Completeness wave / not this feature: retry, pipelined DML, causal
/// consistency, stale read.
pub struct OptimisticTxnContextProvider;
