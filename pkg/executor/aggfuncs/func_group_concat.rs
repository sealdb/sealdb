use super::aggfuncs::{AggFunc, remember_distinct};
use sealdb_errno::Result;
use sealdb_types::Datum;
use std::collections::HashSet;

/// Default `group_concat_max_len` (TiDB / MySQL 1024 bytes).
pub const DEFAULT_GROUP_CONCAT_MAX_LEN: usize = 1024;

thread_local! {
    /// `group_concat_max_len` for the statement, including a `SET_VAR` hint.
    static GROUP_CONCAT_MAX_LEN: std::cell::Cell<usize> =
        const { std::cell::Cell::new(DEFAULT_GROUP_CONCAT_MAX_LEN) };
}

pub fn group_concat_max_len() -> usize {
    GROUP_CONCAT_MAX_LEN.with(|c| c.get())
}

/// Set the statement-scoped `group_concat_max_len`; `None` restores the
/// default (used when a `SET_VAR` hint goes out of scope).
pub fn set_group_concat_max_len(value: Option<usize>) {
    GROUP_CONCAT_MAX_LEN.with(|c| c.set(value.unwrap_or(DEFAULT_GROUP_CONCAT_MAX_LEN)));
}

/// `GROUP_CONCAT(expr[, expr…] [SEPARATOR str])`.
///
/// Only the single-argument form reaches execution today; every argument is
/// already narrowed to `var_string(20)` by the planner's projection, so the
/// value is converted with `Datum::as_bytes` and joined with the separator.
pub struct GroupConcat {
    separator: String,
    distinct: bool,
    values: Vec<Vec<u8>>,
    seen: HashSet<String>,
}

impl GroupConcat {
    pub fn new(distinct: bool) -> Self {
        Self {
            separator: ",".to_string(),
            distinct,
            values: Vec::new(),
            seen: HashSet::new(),
        }
    }
}

impl AggFunc for GroupConcat {
    fn update(&mut self, value: Option<&Datum>) -> Result<()> {
        let Some(v) = value else {
            return Ok(());
        };
        if v.is_null() {
            return Ok(());
        }
        if self.distinct && !remember_distinct(&mut self.seen, v) {
            return Ok(());
        }
        self.values.push(v.as_bytes());
        Ok(())
    }

    fn finalize(&self) -> Result<Datum> {
        if self.values.is_empty() {
            return Ok(Datum::Null);
        }
        let mut out = Vec::new();
        for (i, value) in self.values.iter().enumerate() {
            if i > 0 {
                out.extend_from_slice(self.separator.as_bytes());
            }
            out.extend_from_slice(value);
        }
        // TiDB truncates the concatenation to `group_concat_max_len` bytes and
        // keeps the leading part (no warning in the text protocol).
        let max_len = group_concat_max_len();
        if out.len() > max_len {
            out.truncate(max_len);
        }
        Ok(Datum::Bytes(out))
    }
}
