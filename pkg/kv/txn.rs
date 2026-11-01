use super::kv::Transaction;
use sealdb_errno::Result;
use std::sync::Mutex;

/// Shared handle for a session-owned KV transaction.
pub struct TxnHandle {
    inner: Mutex<Box<dyn Transaction>>,
}

impl TxnHandle {
    pub fn new(txn: Box<dyn Transaction>) -> Self {
        Self {
            inner: Mutex::new(txn),
        }
    }

    pub fn with_txn<R>(&self, f: impl FnOnce(&mut dyn Transaction) -> Result<R>) -> Result<R> {
        let mut guard = self.inner.lock().expect("txn handle lock");
        f(guard.as_mut())
    }
}
