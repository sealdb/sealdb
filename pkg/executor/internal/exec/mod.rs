pub mod executor;
pub use executor::{
    DmlSummary, Executor, QueryResult, note_dml_affected, reset_dml_summary, set_dml_info,
    take_dml_summary,
};

#[cfg(test)]
pub(crate) use executor::VecExec;
