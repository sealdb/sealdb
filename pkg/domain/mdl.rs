use sealdb_errno::{ERR_LOCK_WAIT_TIMEOUT, Error, Result};
use std::collections::HashMap;

/// In-memory metadata lock table (TiDB MDL MVB).
/// Shared locks cover DML; exclusive covers DDL on the same table key.
#[derive(Default)]
pub struct MdlTable {
    entries: HashMap<String, Entry>,
}

#[derive(Default)]
struct Entry {
    /// session_id -> reentrancy count
    shared: HashMap<u64, u32>,
    exclusive: Option<u64>,
}

impl MdlTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquire a shared (DML) lock on `table_key` for `session_id`.
    pub fn lock_shared(&mut self, table_key: &str, session_id: u64) -> Result<()> {
        let entry = self.entries.entry(table_key.to_string()).or_default();
        if let Some(holder) = entry.exclusive
            && holder != session_id
        {
            return Err(mdl_conflict(table_key));
        }
        *entry.shared.entry(session_id).or_insert(0) += 1;
        Ok(())
    }

    /// Acquire an exclusive (DDL) lock on `table_key` for `session_id`.
    pub fn lock_exclusive(&mut self, table_key: &str, session_id: u64) -> Result<()> {
        let entry = self.entries.entry(table_key.to_string()).or_default();
        if let Some(holder) = entry.exclusive {
            if holder != session_id {
                return Err(mdl_conflict(table_key));
            }
            return Ok(());
        }
        let other_shared = entry.shared.keys().any(|&id| id != session_id);
        if other_shared {
            return Err(mdl_conflict(table_key));
        }
        entry.exclusive = Some(session_id);
        Ok(())
    }

    /// Drop one shared hold (or the exclusive hold) for `session_id` on `table_key`.
    pub fn unlock(&mut self, table_key: &str, session_id: u64, exclusive: bool) {
        let Some(entry) = self.entries.get_mut(table_key) else {
            return;
        };
        if exclusive {
            if entry.exclusive == Some(session_id) {
                entry.exclusive = None;
            }
        } else if let Some(count) = entry.shared.get_mut(&session_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                entry.shared.remove(&session_id);
            }
        }
        if entry.exclusive.is_none() && entry.shared.is_empty() {
            self.entries.remove(table_key);
        }
    }

    /// Release every lock held by `session_id`.
    pub fn unlock_session(&mut self, session_id: u64) {
        let keys: Vec<String> = self.entries.keys().cloned().collect();
        for key in keys {
            let Some(entry) = self.entries.get_mut(&key) else {
                continue;
            };
            entry.shared.remove(&session_id);
            if entry.exclusive == Some(session_id) {
                entry.exclusive = None;
            }
            if entry.exclusive.is_none() && entry.shared.is_empty() {
                self.entries.remove(&key);
            }
        }
    }
}

fn mdl_conflict(table_key: &str) -> Error {
    Error::new(
        ERR_LOCK_WAIT_TIMEOUT,
        format!("Lock wait timeout exceeded; try restarting transaction (MDL on {table_key})"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shared_blocks_exclusive() {
        let mut mdl = MdlTable::new();
        mdl.lock_shared("db.t", 1).unwrap();
        let err = mdl.lock_exclusive("db.t", 2).unwrap_err();
        assert_eq!(err.code, ERR_LOCK_WAIT_TIMEOUT);
        mdl.unlock("db.t", 1, false);
        mdl.lock_exclusive("db.t", 2).unwrap();
    }

    #[test]
    fn test_exclusive_blocks_shared() {
        let mut mdl = MdlTable::new();
        mdl.lock_exclusive("db.t", 1).unwrap();
        let err = mdl.lock_shared("db.t", 2).unwrap_err();
        assert_eq!(err.code, ERR_LOCK_WAIT_TIMEOUT);
        mdl.unlock_session(1);
        mdl.lock_shared("db.t", 2).unwrap();
    }
}
