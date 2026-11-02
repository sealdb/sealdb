#![forbid(unsafe_code)]

pub mod builtin_arithmetic;
pub mod builtin_cast;
pub mod builtin_compare;
pub mod builtin_control;
pub mod builtin_op;
pub mod constant;
pub mod evaluator;

pub use evaluator::{
    collation_ci, collation_mode, eval_expr, eval_expr_with_row, eval_filter,
    locate_partition_for_row, with_collation, with_collation_ci,
};
pub use sealdb_util::collate::CollateMode;
