pub mod executor;
pub use executor::{Executor, QueryResult};

#[cfg(test)]
pub(crate) use executor::VecExec;
