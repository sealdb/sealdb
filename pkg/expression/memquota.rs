//! Per-query memory tracker (TiDB `memory.Tracker` + `tidb_mem_quota_query`).
//!
//! Session variables:
//! - `tidb_mem_quota_query`: bytes a single statement may accumulate before the
//!   tracker fires (default 1 GiB, `0` disables the limit).
//! - `tidb_mem_oom_action`: `CANCEL` (error 8175, TiDB default) or `LOG`
//!   (report a warning and keep going).
//!
//! Executors that buffer rows (hash join build / cartesian probe, sort,
//! aggregation) charge their output through [`consume_query_mem`], which raises
//! TiDB's `ErrMemoryExceedForQuery` once the limit is crossed.

use sealdb_errno::{ERR_MEMORY_EXCEEDED, Error, Result};
use sealdb_types::Datum;
use std::cell::Cell;

/// TiDB `variable.DefTiDBMemQuotaQuery` (1 GiB).
pub const DEFAULT_QUERY_MEM_QUOTA: i64 = 1 << 30;

/// TiDB `tidb_mem_oom_action`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OomAction {
    /// Abort the statement with error 8175 (TiDB default).
    Cancel,
    /// Log a warning and continue (statement may exceed the quota).
    Log,
    /// Neither cancel nor log.
    None,
}

impl OomAction {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().trim_matches(|c| c == '\'' || c == '"').to_ascii_uppercase().as_str() {
            "CANCEL" | "DEFAULT" | "" => Some(Self::Cancel),
            "LOG" => Some(Self::Log),
            "NONE" => Some(Self::None),
            _ => None,
        }
    }
}

thread_local! {
    static QUOTA: Cell<i64> = const { Cell::new(DEFAULT_QUERY_MEM_QUOTA) };
    static USED: Cell<u64> = const { Cell::new(0) };
    static ACTION: Cell<OomAction> = const { Cell::new(OomAction::Cancel) };
    /// TiDB appends the connection id to error 8175 (`…[conn=17]`).
    static CONN_ID: Cell<u64> = const { Cell::new(0) };
}

/// `SET tidb_mem_quota_query = <bytes>`; `DEFAULT` restores 1 GiB.
pub fn set_query_mem_quota(quota: i64) {
    QUOTA.with(|q| q.set(quota));
}

pub fn query_mem_quota() -> i64 {
    QUOTA.with(Cell::get)
}

pub fn set_query_mem_oom_action(action: OomAction) {
    ACTION.with(|a| a.set(action));
}

pub fn query_mem_oom_action() -> OomAction {
    ACTION.with(Cell::get)
}

/// Bytes charged to the current statement so far.
pub fn query_mem_usage() -> u64 {
    USED.with(Cell::get)
}

/// Start of a statement: TiDB allocates a fresh tracker per query.
pub fn reset_query_mem_usage() {
    USED.with(|u| u.set(0));
}

/// Server handshake id for the `[conn=N]` suffix of error 8175.
pub fn set_connection_id(id: u64) {
    CONN_ID.with(|c| c.set(id));
}

pub fn connection_id() -> u64 {
    CONN_ID.with(Cell::get)
}

/// Charge `bytes` against the statement quota, mirroring TiDB
/// `Tracker.Consume` + `ActionOnExceed` (`CANCEL`/`LOG`).
pub fn consume_query_mem(bytes: u64) -> Result<()> {
    if bytes == 0 {
        return Ok(());
    }
    let used = USED.with(|u| {
        let next = u.get().saturating_add(bytes);
        u.set(next);
        next
    });
    let quota = QUOTA.with(Cell::get);
    if quota <= 0 || used <= quota as u64 {
        return Ok(());
    }
    if std::env::var_os("SEALDB_MEM_DEBUG").is_some() && bytes > 1 << 20 {
        eprintln!("[mem] used={used} quota={quota} charge={bytes}");
        let bt = std::backtrace::Backtrace::force_capture();
        eprintln!("{bt}");
    }
    match ACTION.with(Cell::get) {
        OomAction::Cancel => Err(memory_exceeded_error()),
        OomAction::Log | OomAction::None => Ok(()),
    }
}

/// TiDB `ErrMemoryExceedForQuery` (8175) text, including the connection id.
pub fn memory_exceeded_error() -> Error {
    Error::new(
        ERR_MEMORY_EXCEEDED,
        format!(
            "Your query has been cancelled due to exceeding the allowed memory limit for a single SQL query. \
             Please try narrowing your query scope or increase the tidb_mem_quota_query limit and try again.[conn={}]",
            connection_id()
        ),
    )
}

/// Heap footprint of one datum (fixed width + owned buffer).
pub fn datum_mem_size(datum: &Datum) -> u64 {
    let base = std::mem::size_of::<Datum>() as u64;
    base + match datum {
        Datum::Bytes(b) => b.len() as u64,
        _ => 0,
    }
}

pub fn row_mem_size(row: &[Datum]) -> u64 {
    row.iter().map(datum_mem_size).sum()
}

pub fn rows_mem_size(rows: &[Vec<Datum>]) -> u64 {
    rows.iter().map(|row| row_mem_size(row)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_cancel_and_log_actions() {
        reset_query_mem_usage();
        set_query_mem_quota(64);
        set_query_mem_oom_action(OomAction::Cancel);
        assert!(consume_query_mem(32).is_ok());
        let err = consume_query_mem(64).unwrap_err();
        assert_eq!(err.code, ERR_MEMORY_EXCEEDED);
        assert!(err.message.ends_with("[conn=0]"), "{}", err.message);

        reset_query_mem_usage();
        set_query_mem_oom_action(OomAction::Log);
        assert!(consume_query_mem(1024).is_ok());
        assert!(query_mem_usage() >= 1024);

        reset_query_mem_usage();
        set_query_mem_quota(0); // 0 = unlimited
        assert!(consume_query_mem(1 << 40).is_ok());

        set_query_mem_quota(DEFAULT_QUERY_MEM_QUOTA);
        set_query_mem_oom_action(OomAction::Cancel);
        reset_query_mem_usage();
    }
}
