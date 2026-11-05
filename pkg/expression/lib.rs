#![forbid(unsafe_code)]

pub mod builtin_arithmetic;
pub mod builtin_cast;
pub mod builtin_compare;
pub mod builtin_control;
pub mod builtin_op;
pub mod constant;
pub mod evaluator;
pub mod funcs;
pub mod vectorized;

pub use evaluator::{
    collation_ci, collation_mode, eval_expr, eval_expr_with_row, eval_filter,
    locate_partition_for_row, validate_string_cmp_literals, with_collation, with_collation_ci,
    with_sql_mode,
};
pub use funcs::{FuncCategory, FuncMeta, builtin_list, implemented_names, lookup};
pub use sealdb_util::collate::CollateMode;
pub use vectorized::{VecEval, VecExprStub};