#![forbid(unsafe_code)]

//! SQL plan bindings (TiDB `pkg/bindinfo`).

pub mod binding;
pub mod binding_cache;
pub mod session_handle;

pub use binding::{
    Binding, ENABLED, MANUAL, normalize_display, normalize_exact, normalize_sql, sql_digest,
};
pub use binding_cache::BindingCache;
pub use session_handle::SessionBindingHandle;
