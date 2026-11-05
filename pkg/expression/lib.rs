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
    clear_warnings, collation_ci, collation_mode, current_db_name, eval_expr, eval_expr_with_row,
    eval_filter, format_timestamp_display, locate_partition_for_row, session_local_to_utc,
    sql_mode_flag, take_warnings, utc_to_session_local, validate_string_cmp_literals,
    with_collation, with_collation_ci, with_current_db, with_index_dst_double, with_sql_mode,
    with_time_zone,
};
pub use funcs::{FuncCategory, FuncMeta, builtin_list, implemented_names, lookup};
pub use sealdb_util::collate::CollateMode;
pub use vectorized::{VecEval, VecExprStub};