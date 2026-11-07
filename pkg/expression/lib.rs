#![forbid(unsafe_code)]

pub mod builtin_arithmetic;
pub mod builtin_cast;
pub mod builtin_compare;
pub mod builtin_control;
pub mod builtin_op;
pub mod constant;
pub mod evaluator;
pub mod funcs;
pub mod memquota;
pub mod vectorized;

pub use memquota::{
    OomAction, consume_query_mem, datum_mem_size, query_mem_oom_action, query_mem_quota,
    query_mem_usage, reset_query_mem_usage, row_mem_size, rows_mem_size, set_connection_id,
    set_query_mem_oom_action, set_query_mem_quota,
};

pub use evaluator::{
    DivByZeroMode, clear_warnings, collation_ci, collation_mode, current_db_name, datum_sql_eq,
    eval_expr,
    eval_expr_with_row, eval_filter, format_timestamp_display, locate_partition_for_row,
    note_warning, session_local_to_utc, set_window_concurrency, sql_mode_flag, take_warnings,
    truthy,
    utc_to_session_local, validate_string_cmp_literals, window_concurrency, with_collation,
    with_collation_ci, with_current_db, with_div_by_zero_mode, with_index_dst_double,
    with_sql_mode, with_time_zone,
};
pub use funcs::{FuncCategory, FuncMeta, builtin_list, implemented_names, lookup};
pub use sealdb_util::collate::CollateMode;
pub use vectorized::{VecEval, VecExprStub};