#![forbid(unsafe_code)]

pub mod builtin_arithmetic;
pub mod builtin_binary_plan;
pub mod builtin_cast;
pub mod builtin_compare;
pub mod builtin_control;
pub mod builtin_op;
pub mod constant;
pub mod encryption;
pub mod evaluator;
pub mod funcs;
pub mod memquota;
pub mod shard;
pub mod vectorized;

pub use memquota::{
    OomAction, consume_query_mem, datum_mem_size, query_mem_oom_action, query_mem_quota,
    query_mem_usage, reset_query_mem_usage, row_mem_size, rows_mem_size, set_connection_id,
    set_query_mem_oom_action, set_query_mem_quota,
};

pub use evaluator::{
    DEFAULT_WINDOW_CONCURRENCY, DivByZeroMode, NOTE_WARNING_BASE,
    allow_auto_increment_in_generated, allow_auto_random_explicit_insert, allow_write_row_id,
    block_encryption_mode, clear_warnings, collation_ci, collation_mode, current_db_name,
    datum_sql_cmp, datum_sql_eq, eval_expr, eval_expr_with_row, eval_filter, eval_where_text,
    format_timestamp_display, locate_partition_for_row, note_message, note_warning,
    session_local_time_exists, session_local_to_utc, session_local_to_utc_gap_shift,
    set_current_dml_table, set_default_week_format, set_last_insert_id, set_session_timestamp,
    set_window_concurrency, shift_time_minutes, sql_mode_flag, strict_not_null_check,
    take_warnings, take_warnings_from, time_to_unix_millis, truthy, utc_to_session_local,
    validate_string_cmp_literals, validate_string_cmp_literals_on, warnings_len,
    window_concurrency, with_allow_auto_increment_in_generated,
    with_allow_auto_random_explicit_insert, with_allow_write_row_id, with_block_encryption_mode,
    with_collation, with_collation_ci, with_current_db, with_div_by_zero_mode,
    with_index_dst_double, with_insert_value_eval, with_sql_mode, with_strict_dml,
    with_strict_not_null_check, with_time_zone,
};
pub use funcs::{FuncCategory, FuncMeta, builtin_list, check_arg_count, implemented_names, lookup};
pub use sealdb_util::collate::CollateMode;
pub use vectorized::{VecEval, VecExprStub};
