/// in-memory stats Handle (TiDB `statistics/handle`).
/// Persists under Storage key `mstats` (mysql.stats_* equivalent).
use crate::table::{
    ColumnStats, Histogram, HistogramBucket, IndexBucket, IndexHistogram, IndexStats,
    PartitionStats, Table, TopN,
};
use sealdb_errno::{ERR_UNKNOWN_ERROR, Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

pub const STATS_STORAGE_KEY: &[u8] = b"mstats";

/// One registered extended statistics object (TiDB `mysql.stats_extended`).
#[derive(Clone, Debug)]
pub struct ExtendedStats {
    pub name: String,
    /// TiDB `StatsTypeCorrelation`; only that kind is accepted at DDL time.
    pub kind: String,
    pub columns: Vec<String>,
}

pub struct Handle {
    inner: Mutex<HashMap<(String, String), Table>>,
    /// Soft-deleted stats (TiDB `DROP STATS`): `mysql.stats_meta` /
    /// `stats_histograms` keep their rows with zeroed counters while the
    /// buckets and TopN are gone. The planner never sees these entries.
    dropped: Mutex<HashMap<(String, String), Table>>,
    /// Locked table / partition ids (TiDB `mysql.stats_table_locked`): `LOCK
    /// STATS` rows, surfaced through `mysql.stats_table_locked` and
    /// `SHOW STATS_LOCKED`. Explicit/auto `ANALYZE` does not consult the set
    /// yet (see the `statistics/lock_table_stats` entry in progress.md).
    locked: Mutex<HashSet<i64>>,
    /// Extended statistics per table id (TiDB `mysql.stats_extended`), in
    /// creation order so duplicate-column-set checks match TiDB.
    extended: Mutex<HashMap<i64, Vec<ExtendedStats>>>,
    /// `mysql.stats_fm_sketch` rows as `(table_id, partition_id, hist_id)`.
    fm_sketches: Mutex<HashSet<(i64, i64, i64)>>,
}

impl Handle {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            dropped: Mutex::new(HashMap::new()),
            locked: Mutex::new(HashSet::new()),
            extended: Mutex::new(HashMap::new()),
            fm_sketches: Mutex::new(HashSet::new()),
        }
    }

    /// Extended statistics registered on `table_id`, in creation order.
    pub fn extended_stats(&self, table_id: i64) -> Vec<ExtendedStats> {
        self.extended
            .lock()
            .expect("stats lock")
            .get(&table_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Register one extended statistics object (the caller validated names,
    /// duplicate columns and column-set uniqueness).
    pub fn add_extended_stats(&self, table_id: i64, stats: ExtendedStats) {
        self.extended
            .lock()
            .expect("stats lock")
            .entry(table_id)
            .or_default()
            .push(stats);
    }

    /// Drop the extended statistics called `name`; returns whether a row was
    /// removed.
    pub fn drop_extended_stats(&self, table_id: i64, name: &str) -> bool {
        let mut extended = self.extended.lock().expect("stats lock");
        let Some(stats) = extended.get_mut(&table_id) else {
            return false;
        };
        let before = stats.len();
        stats.retain(|s| !s.name.eq_ignore_ascii_case(name));
        before != stats.len()
    }

    /// Record the FM sketch rows an `ANALYZE` produced. TiDB keys them by
    /// table, partition and histogram id, so re-analyzing the same table keeps
    /// the row count stable.
    pub fn record_fm_sketches(&self, rows: impl IntoIterator<Item = (i64, i64, i64)>) {
        let mut fm = self.fm_sketches.lock().expect("stats lock");
        for row in rows {
            fm.insert(row);
        }
    }

    /// `TRUNCATE TABLE mysql.stats_fm_sketch`.
    pub fn clear_fm_sketches(&self) {
        self.fm_sketches.lock().expect("stats lock").clear();
    }

    /// `mysql.stats_fm_sketch` rows in a stable order.
    pub fn fm_sketch_ids(&self) -> Vec<(i64, i64, i64)> {
        let mut rows: Vec<(i64, i64, i64)> = self
            .fm_sketches
            .lock()
            .expect("stats lock")
            .iter()
            .copied()
            .collect();
        rows.sort_unstable();
        rows
    }

    /// Lock statistics collection for `ids`; returns the ids that were already
    /// locked (TiDB `AddLockedTables` skipped list).
    pub fn lock_stats(&self, ids: &[i64]) -> Vec<i64> {
        let mut locked = self.locked.lock().expect("stats lock");
        let mut skipped = Vec::new();
        for id in ids {
            if !locked.insert(*id) {
                skipped.push(*id);
            }
        }
        skipped
    }

    /// Unlock statistics collection for `ids`; returns the ids that were not
    /// locked (TiDB `RemoveLockedTables` skipped list).
    pub fn unlock_stats(&self, ids: &[i64]) -> Vec<i64> {
        let mut locked = self.locked.lock().expect("stats lock");
        let mut skipped = Vec::new();
        for id in ids {
            if !locked.remove(id) {
                skipped.push(*id);
            }
        }
        skipped
    }

    /// Locked table / partition ids in ascending order.
    pub fn locked_ids(&self) -> Vec<i64> {
        let mut ids: Vec<i64> = self
            .locked
            .lock()
            .expect("stats lock")
            .iter()
            .copied()
            .collect();
        ids.sort_unstable();
        ids
    }

    pub fn is_locked(&self, id: i64) -> bool {
        self.locked.lock().expect("stats lock").contains(&id)
    }

    /// Drop the lock rows of `ids` (TiDB removes them with the table).
    pub fn forget_locks(&self, ids: &[i64]) {
        let mut locked = self.locked.lock().expect("stats lock");
        for id in ids {
            locked.remove(id);
        }
    }

    pub fn update(&self, db: impl Into<String>, table: impl Into<String>, stats: Table) {
        let mut stats = stats;
        // ANALYZE refreshes count/histograms and clears the modify delta.
        stats.modify_count = 0;
        let key = (
            db.into().to_ascii_lowercase(),
            table.into().to_ascii_lowercase(),
        );
        // A fresh ANALYZE revives a soft-deleted (`DROP STATS`) table.
        self.dropped.lock().expect("stats lock").remove(&key);
        self.inner.lock().expect("stats lock").insert(key, stats);
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

    /// Drop stats for a table (TiDB clears stats_meta on DROP TABLE).
    pub fn delete(&self, db: &str, table: &str) {
        let key = (db.to_ascii_lowercase(), table.to_ascii_lowercase());
        self.inner.lock().expect("stats lock").remove(&key);
        self.dropped.lock().expect("stats lock").remove(&key);
    }

    /// TiDB `DROP STATS` soft delete: zero the histogram metadata but keep the
    /// `stats_meta` / `stats_histograms` rows, and drop buckets + TopN. The
    /// planner falls back to pseudo statistics because `table()` skips these.
    pub fn soft_delete(&self, db: &str, table: &str) {
        let key = (db.to_ascii_lowercase(), table.to_ascii_lowercase());
        let mut inner = self.inner.lock().expect("stats lock");
        let Some(mut stats) = inner.remove(&key) else {
            return;
        };
        stats.modify_count = 0;
        for (_, column) in stats.columns.iter_mut() {
            column.ndv = 0;
            column.null_count = 0;
            column.tot_col_size = 0;
            column.part_tot_col_size.clear();
            column.histogram = Some(crate::table::Histogram::default());
            column.topn = None;
            column.value_counts = None;
        }
        for (_, index) in stats.indices.iter_mut() {
            index.ndv = 0;
            index.null_count = 0;
            index.histogram = Some(crate::table::IndexHistogram::default());
            index.topn.clear();
            index.cm_sketch = None;
            index.stats_ver = 0;
        }
        self.dropped.lock().expect("stats lock").insert(key, stats);
    }

    /// Stats rows as `mysql.stats_*` reports them: live stats, else the soft
    /// deleted snapshot of a `DROP STATS` table.
    pub fn raw_table(&self, db: &str, table: &str) -> Option<Table> {
        let key = (db.to_ascii_lowercase(), table.to_ascii_lowercase());
        if let Some(stats) = self.inner.lock().expect("stats lock").get(&key) {
            return Some(stats.clone());
        }
        self.dropped.lock().expect("stats lock").get(&key).cloned()
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
                indices: stats
                    .indices
                    .iter()
                    .map(|(name, idx)| persist_index(name, idx))
                    .collect(),
                columns: stats
                    .columns
                    .iter()
                    .map(|(name, col)| persist_column(name, col))
                    .collect(),
                part_counts: stats.part_counts.clone(),
                partitions: stats
                    .partitions
                    .iter()
                    .map(|part| PersistPartition {
                        id: part.id,
                        columns: part
                            .columns
                            .iter()
                            .map(|(name, col)| persist_column(name, col))
                            .collect(),
                        indices: part
                            .indices
                            .iter()
                            .map(|(name, idx)| persist_index(name, idx))
                            .collect(),
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
            let columns = t.columns.into_iter().map(load_column).collect();
            let indices = t.indices.into_iter().map(load_index).collect();
            let partitions = t
                .partitions
                .into_iter()
                .map(|part| PartitionStats {
                    id: part.id,
                    columns: part.columns.into_iter().map(load_column).collect(),
                    indices: part.indices.into_iter().map(load_index).collect(),
                })
                .collect();
            map.insert(
                (t.db, t.table),
                Table {
                    count: t.count,
                    modify_count: t.modify_count,
                    columns,
                    indices,
                    part_counts: t.part_counts,
                    partitions,
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

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.lock().expect("stats lock");
        f.debug_struct("Handle")
            .field("tables", &inner.len())
            .finish()
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
    #[serde(default)]
    indices: Vec<PersistIndex>,
    columns: Vec<PersistColumn>,
    #[serde(default)]
    part_counts: Vec<(i64, u64)>,
    #[serde(default)]
    partitions: Vec<PersistPartition>,
}

#[derive(Serialize, Deserialize)]
struct PersistPartition {
    id: i64,
    columns: Vec<PersistColumn>,
    indices: Vec<PersistIndex>,
}

fn persist_index(name: &str, idx: &IndexStats) -> PersistIndex {
    PersistIndex {
        name: name.to_string(),
        ndv: idx.ndv,
        null_count: idx.null_count,
        histogram: idx.histogram.as_ref().map(|h| PersistIndexHist {
            ndv: h.ndv,
            buckets: h
                .buckets
                .iter()
                .map(|b| PersistIndexBucket {
                    lower: b.lower.clone(),
                    upper: b.upper.clone(),
                    count: b.count,
                    repeats: b.repeats,
                    ndv: b.ndv,
                })
                .collect(),
        }),
        topn: idx.topn.clone(),
        cm_sketch: idx.cm_sketch.clone(),
        stats_ver: idx.stats_ver,
        placeholder: idx.placeholder,
    }
}

fn load_index(i: PersistIndex) -> (String, IndexStats) {
    (
        i.name,
        IndexStats {
            ndv: i.ndv,
            null_count: i.null_count,
            histogram: i.histogram.map(|h| IndexHistogram {
                ndv: h.ndv,
                buckets: h
                    .buckets
                    .into_iter()
                    .map(|b| IndexBucket {
                        lower: b.lower,
                        upper: b.upper,
                        count: b.count,
                        repeats: b.repeats,
                        ndv: b.ndv,
                    })
                    .collect(),
            }),
            topn: i.topn,
            cm_sketch: i.cm_sketch,
            stats_ver: i.stats_ver,
            placeholder: i.placeholder,
        },
    )
}

fn persist_column(name: &str, col: &ColumnStats) -> PersistColumn {
    PersistColumn {
        name: name.to_string(),
        ndv: col.ndv,
        null_count: col.null_count,
        correlation: col.correlation,
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
        value_counts: col.value_counts.clone(),
        tot_col_size: col.tot_col_size,
        part_tot_col_size: col.part_tot_col_size.clone(),
    }
}

fn load_column(c: PersistColumn) -> (String, ColumnStats) {
    (
        c.name,
        ColumnStats {
            ndv: c.ndv,
            null_count: c.null_count,
            histogram: c.histogram.map(|h| Histogram {
                ndv: c.ndv,
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
            value_counts: c.value_counts,
            tot_col_size: c.tot_col_size,
            part_tot_col_size: c.part_tot_col_size,
            ci: false,
            correlation: c.correlation,
        },
    )
}

#[derive(Serialize, Deserialize)]
struct PersistIndex {
    name: String,
    ndv: u64,
    #[serde(default)]
    null_count: u64,
    #[serde(default)]
    histogram: Option<PersistIndexHist>,
    #[serde(default)]
    topn: Vec<(Vec<u8>, u64)>,
    #[serde(default)]
    cm_sketch: Option<crate::cmsketch::CmSketch>,
    #[serde(default)]
    stats_ver: u64,
    /// The index existed at ANALYZE time (see `IndexStats::placeholder`).
    #[serde(default)]
    placeholder: bool,
}

#[derive(Serialize, Deserialize)]
struct PersistIndexHist {
    ndv: u64,
    buckets: Vec<PersistIndexBucket>,
}

#[derive(Serialize, Deserialize)]
struct PersistIndexBucket {
    lower: Vec<u8>,
    upper: Vec<u8>,
    count: u64,
    repeats: u64,
    ndv: u64,
}

#[derive(Serialize, Deserialize)]
struct PersistColumn {
    name: String,
    ndv: u64,
    null_count: u64,
    #[serde(default)]
    correlation: f64,
    #[serde(default)]
    histogram: Option<PersistHist>,
    #[serde(default)]
    topn: Option<Vec<(String, u64)>>,
    #[serde(default)]
    value_counts: Option<Vec<(String, u64)>>,
    #[serde(default)]
    tot_col_size: u64,
    #[serde(default)]
    part_tot_col_size: Vec<(i64, u64)>,
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

#[cfg(test)]
mod locked_stats_tests {
    use super::*;

    #[test]
    fn lock_stats_reports_already_locked_ids() {
        let handle = Handle::new();
        assert!(handle.lock_stats(&[1, 2]).is_empty());
        assert_eq!(handle.lock_stats(&[2, 3]), vec![2]);
        // TiDB keeps locked ids sorted for `mysql.stats_table_locked`.
        assert_eq!(handle.locked_ids(), vec![1, 2, 3]);
        assert!(handle.is_locked(1));
        assert!(!handle.is_locked(4));
    }

    #[test]
    fn unlock_stats_reports_not_locked_ids() {
        let handle = Handle::new();
        handle.lock_stats(&[1, 2]);
        assert!(handle.unlock_stats(&[2]).is_empty());
        assert_eq!(handle.unlock_stats(&[1, 5]), vec![5]);
        assert!(handle.locked_ids().is_empty());
    }

    #[test]
    fn forget_locks_drops_dropped_table_rows() {
        let handle = Handle::new();
        handle.lock_stats(&[1, 2, 3]);
        handle.forget_locks(&[2, 3]);
        assert_eq!(handle.locked_ids(), vec![1]);
    }
}
