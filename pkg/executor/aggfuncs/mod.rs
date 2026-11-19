#[allow(clippy::module_inception)]
pub mod aggfuncs;
pub mod builder;
pub mod func_avg;
pub mod func_bit;
pub mod func_count;
pub mod func_group_concat;
pub mod func_max_min;
pub mod func_sum;

pub use aggfuncs::{AggFunc, is_agg_name};
pub use builder::build_agg_func;
pub use func_group_concat::{group_concat_max_len, set_group_concat_max_len};
