#![forbid(unsafe_code)]
#![allow(clippy::module_inception)]

pub mod domain;
pub mod mdl;
pub mod schema_checker;
pub use domain::{Domain, GlobalSysVars, SchemaSnap};
pub use mdl::MdlTable;
