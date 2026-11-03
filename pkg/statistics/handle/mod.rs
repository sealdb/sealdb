/// in-memory stats Handle (TiDB `statistics/handle`).
/// Persists under Storage key `mstats` (mysql.stats_* equivalent).
use crate::table::{ColumnStats, Histogram, HistogramBucket, Table, TopN};
use sealdb_errno::{ERR_UNKNOWN_ERROR, Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

pub const STATS_STORAGE_KEY: &[u8] = b"mstats";

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
        let mut stats = stats;
        // ANALYZE refreshes count/histograms and clears the modify delta.
        stats.modify_count = 0;
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

    /// Accumulate DML delta (TiDB stats_meta.modify_count).
    pub fn add_modify(&self, db: &str, table: &str, delta: u64) {
        if delta == 0 {
            return;
        }
        let key = (db.to_ascii_lowercase(), table.to_ascii_lowercase());
        let mut inner = self.inner.lock().expect("stats lock");
        let entry = inner.entry(key).or_default();
        entry.modify_count = entry.modify_count.saturating_add(delta);
    }

    pub fn needs_auto_analyze(&self, db: &str, table: &str, ratio: f64) -> bool {
        self.table(db, table)
            .map(|t| t.needs_auto_analyze(ratio))
            .unwrap_or(false)
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let inner = self.inner.lock().expect("stats lock");
        let snap: Vec<PersistTable> = inner
            .iter()
            .map(|((db, table), stats)| PersistTable {
                db: db.clone(),
                table: table.clone(),
                count: stats.count,
                modify_count: stats.modify_count,
                version: stats.version,
                columns: stats
                    .columns
                    .iter()
                    .map(|(name, col)| PersistColumn {
                        name: name.clone(),
                        ndv: col.ndv,
                        null_count: col.null_count,
                        histogram: col.histogram.as_ref().map(|h| PersistHist {
                            buckets: h
                                .buckets
                                .iter()
                                .map(|b| PersistBucket {
                                    lower: b.lower.clone(),
                                    upper: b.upper.clone(),
                                    count: b.count,
                                    repeats: b.repeats,
                                    ndv: b.ndv,
                                })
                                .collect(),
                        }),
                        topn: col.topn.as_ref().map(|t| t.items.clone()),
                    })
                    .collect(),
            })
            .collect();
        serde_json::to_vec(&snap)
            .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("encode stats: {e}")))
    }

    pub fn decode(&self, raw: &[u8]) -> Result<()> {
        if raw.is_empty() {
            return Ok(());
        }
        let snap: Vec<PersistTable> = serde_json::from_slice(raw)
            .map_err(|e| Error::new(ERR_UNKNOWN_ERROR, format!("decode stats: {e}")))?;
        let mut map = HashMap::new();
        for t in snap {
            let columns = t
                .columns
                .into_iter()
                .map(|c| {
                    (
                        c.name,
                        ColumnStats {
                            ndv: c.ndv,
                            null_count: c.null_count,
                            histogram: c.histogram.map(|h| Histogram {
                                buckets: h
                                    .buckets
                                    .into_iter()
                                    .map(|b| HistogramBucket {
                                        lower: b.lower,
                                        upper: b.upper,
                                        count: b.count,
                                        repeats: b.repeats,
                                        ndv: b.ndv,
                                    })
                                    .collect(),
                            }),
                            topn: c.topn.map(|items| TopN { items }),
                        },
                    )
                })
                .collect();
            map.insert(
                (t.db, t.table),
                Table {
                    count: t.count,
                    modify_count: t.modify_count,
                    columns,
                    version: t.version,
                },
            );
        }
        *self.inner.lock().expect("stats lock") = map;
        Ok(())
    }
}

impl Default for Handle {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Serialize, Deserialize)]
struct PersistTable {
    db: String,
    table: String,
    count: u64,
    #[serde(default)]
    modify_count: u64,
    version: u64,
    columns: Vec<PersistColumn>,
}

#[derive(Serialize, Deserialize)]
struct PersistColumn {
    name: String,
    ndv: u64,
    null_count: u64,
    #[serde(default)]
    histogram: Option<PersistHist>,
    #[serde(default)]
    topn: Option<Vec<(String, u64)>>,
}

#[derive(Serialize, Deserialize)]
struct PersistHist {
    buckets: Vec<PersistBucket>,
}

#[derive(Serialize, Deserialize)]
struct PersistBucket {
    lower: String,
    upper: String,
    count: u64,
    repeats: u64,
    ndv: u64,
}
