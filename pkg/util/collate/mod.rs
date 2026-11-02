//! Collators — TiDB `pkg/util/collate`.
//!
//! C5 unicode-weights MVB: `utf8mb4_unicode_ci` via UCA 4.0.0 Latin/ASCII subset.

// TiDB keeps `collate.go` beside the package; allow `collate::collate` name.
#[allow(clippy::module_inception)]
pub mod collate;
pub mod ucadata;
pub mod unicode_0400_ci_generated;
pub mod unicode_0400_ci_impl;

pub use collate::{CollateMode, bytes_eq, compare_bytes};
