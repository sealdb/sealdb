use super::kv::{Storage, Transaction};
use sealdb_errno::Result;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct MemStorage {
    data: Arc<Mutex<BTreeMap<Vec<u8>, Vec<u8>>>>,
}

impl MemStorage {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Storage for MemStorage {
    fn begin(&self) -> Result<Box<dyn Transaction>> {
        Ok(Box::new(MemTransaction {
            data: Arc::clone(&self.data),
            writes: BTreeMap::new(),
            deletes: Vec::new(),
        }))
    }
}

struct MemTransaction {
    data: Arc<Mutex<BTreeMap<Vec<u8>, Vec<u8>>>>,
    writes: BTreeMap<Vec<u8>, Vec<u8>>,
    deletes: Vec<Vec<u8>>,
}

impl Transaction for MemTransaction {
    fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if self.deletes.iter().any(|k| k == key) {
            return Ok(None);
        }
        if let Some(v) = self.writes.get(key) {
            return Ok(Some(v.clone()));
        }
        Ok(self.data.lock().expect("mem kv").get(key).cloned())
    }

    fn set(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.deletes.retain(|k| k != key);
        self.writes.insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    fn delete(&mut self, key: &[u8]) -> Result<()> {
        self.writes.remove(key);
        self.deletes.push(key.to_vec());
        Ok(())
    }

    fn scan(&mut self, start: &[u8], end: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let guard = self.data.lock().expect("mem kv");
        let mut out = Vec::new();
        for (k, v) in guard.range(start.to_vec()..end.to_vec()) {
            if self.deletes.iter().any(|d| d == k) {
                continue;
            }
            let value = self.writes.get(k).cloned().unwrap_or_else(|| v.clone());
            out.push((k.clone(), value));
        }
        for (k, v) in &self.writes {
            if k.as_slice() >= start && k.as_slice() < end && !out.iter().any(|(ek, _)| ek == k) {
                out.push((k.clone(), v.clone()));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    fn commit(&mut self) -> Result<()> {
        let mut guard = self.data.lock().expect("mem kv");
        for k in &self.deletes {
            guard.remove(k);
        }
        for (k, v) in std::mem::take(&mut self.writes) {
            guard.insert(k, v);
        }
        self.deletes.clear();
        Ok(())
    }

    fn rollback(&mut self) -> Result<()> {
        self.writes.clear();
        self.deletes.clear();
        Ok(())
    }
}
