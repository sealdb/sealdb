/// TiDB `statistics.Table` / `HistColl` core fields for F21.
/// Completeness wave: histograms, TopN, CMSketch, correlation, pseudo versioning.
#[derive(Clone, Debug, Default)]
pub struct ColumnStats {
    pub ndv: u64,
    pub null_count: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Table {
    pub count: u64,
    pub columns: Vec<(String, ColumnStats)>,
    pub version: u64,
}

impl Table {
    pub const PSEUDO_ROW_COUNT: u64 = 10_000;

    pub fn column(&self, name: &str) -> Option<&ColumnStats> {
        self.columns
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, s)| s)
    }

    pub fn eq_row_count(&self, column: &str) -> u64 {
        match self.column(column) {
            Some(col) if col.ndv > 0 => self.count.div_ceil(col.ndv),
            _ => self.count,
        }
    }
}
