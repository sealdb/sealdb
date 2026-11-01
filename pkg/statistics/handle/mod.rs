/// F21 core: in-memory stats Handle (TiDB `statistics/handle`).
/// Completeness wave: persist mysql.stats_*, auto-analyze, syncload.
use crate::table::Table;
use std::collections::HashMap;
use std::sync::Mutex;

pub struct Handle {
    inner: Mutex<HashMap<(String, String), Table>>,
}

impl Handle {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub fn update(&self, db: impl Into<String>, table: impl Into<String>, stats: Table) {
        self.inner.lock().expect("stats lock").insert(
            (
                db.into().to_ascii_lowercase(),
                table.into().to_ascii_lowercase(),
            ),
            stats,
        );
    }

    pub fn table(&self, db: &str, table: &str) -> Option<Table> {
        self.inner
            .lock()
            .expect("stats lock")
            .get(&(db.to_ascii_lowercase(), table.to_ascii_lowercase()))
            .cloned()
    }
}

impl Default for Handle {
    fn default() -> Self {
        Self::new()
    }
}
