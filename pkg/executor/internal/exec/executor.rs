use sealdb_errno::Result;
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;

pub trait Executor: Send {
    fn open(&mut self) -> Result<()>;
    fn next(&mut self) -> Result<Option<Chunk>>;
    fn close(&mut self) -> Result<()>;
    fn schema(&self) -> &[String];
}

#[cfg(test)]
pub(crate) struct VecExec {
    schema: Vec<String>,
    rows: Vec<Vec<Datum>>,
    emitted: bool,
}

#[cfg(test)]
impl VecExec {
    pub(crate) fn new(schema: Vec<String>, rows: Vec<Vec<Datum>>) -> Self {
        Self {
            schema,
            rows,
            emitted: false,
        }
    }
}

#[cfg(test)]
impl Executor for VecExec {
    fn open(&mut self) -> Result<()> {
        self.emitted = false;
        Ok(())
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if self.emitted {
            return Ok(None);
        }
        self.emitted = true;
        let mut chunk = Chunk::new(self.schema.len(), self.rows.len());
        for row in &self.rows {
            chunk.append_row(row.clone());
        }
        Ok(Some(chunk))
    }

    fn close(&mut self) -> Result<()> {
        Ok(())
    }

    fn schema(&self) -> &[String] {
        &self.schema
    }
}

#[derive(Clone, Debug)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Datum>>,
    /// TiDB-style warnings `(code, message)` for `--enable_warnings` / SHOW WARNINGS.
    pub warnings: Vec<(i32, String)>,
    /// Rows changed by a DML statement (MySQL OK-packet `affected_rows`).
    pub affected_rows: u64,
    /// MySQL OK-packet info string (`Records: …` / `Rows matched: …`), empty for
    /// statements that do not set one (e.g. DELETE).
    pub info: String,
}

impl QueryResult {
    pub fn typed_json(&self) -> serde_json::Value {
        serde_json::json!({
            "columns": self.columns,
            "rows": self.rows.iter().map(|row| {
                row.iter().map(Datum::typed_json).collect::<Vec<_>>()
            }).collect::<Vec<_>>(),
        })
    }

    pub fn display_rows(&self) -> Vec<Vec<String>> {
        self.rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|d| match d {
                        Datum::Time(t) => sealdb_expression::format_timestamp_display(*t),
                        other => other.display(),
                    })
                    .collect()
            })
            .collect()
    }
}

/// MySQL-protocol summary of a DML statement, published by the write executors
/// and folded into `QueryResult` by `ExecutorBuilder::run`.
#[derive(Clone, Debug, Default)]
pub struct DmlSummary {
    pub affected_rows: u64,
    pub info: String,
}

thread_local! {
    // A stack: DML statements may run nested executors (e.g. a scalar subquery
    // in the WHERE clause). The inner `run` must not clobber the outer summary.
    static DML_SUMMARY: std::cell::RefCell<Vec<DmlSummary>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Start collecting a DML summary for one statement.
pub fn reset_dml_summary() {
    DML_SUMMARY.with(|s| s.borrow_mut().push(DmlSummary::default()));
}

/// Accumulate affected rows; `UPDATE`/`DELETE`/`INSERT` all report through this.
pub fn note_dml_affected(rows: u64) {
    DML_SUMMARY.with(|s| {
        if let Some(summary) = s.borrow_mut().last_mut() {
            summary.affected_rows += rows;
        }
    });
}

/// Set the OK-packet info string (`Records: …` / `Rows matched: …`).
pub fn set_dml_info(info: impl Into<String>) {
    DML_SUMMARY.with(|s| {
        if let Some(summary) = s.borrow_mut().last_mut() {
            summary.info = info.into();
        }
    });
}

/// Take the summary for the just-finished statement, if it was a write.
pub fn take_dml_summary() -> Option<DmlSummary> {
    DML_SUMMARY.with(|s| s.borrow_mut().pop())
}
