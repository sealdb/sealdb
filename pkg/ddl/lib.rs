#![forbid(unsafe_code)]
#![allow(clippy::module_inception)]

pub mod alter;
pub mod create_table;
pub mod ddl;
pub mod index;
pub mod schema;
pub mod table;

pub use create_table::build_table_info;
pub use ddl::execute_ddl;
