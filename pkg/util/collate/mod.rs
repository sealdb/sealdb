//! Collators — TiDB `pkg/util/collate`.
//!
//! `utf8mb4_unicode_ci` Latin/ASCII UCA subset + GBK (`gbk_bin` /
//! `gbk_chinese_ci`) / latin1 charset MVB.

// TiDB keeps `collate.go` beside the package; allow `collate::collate` name.
#[allow(clippy::module_inception)]
pub mod collate;
pub mod gbk_bin;
pub mod gbk_chinese_ci;
pub mod gbk_chinese_ci_data;
pub mod ucadata;
pub mod unicode_0400_ci_generated;
pub mod unicode_0400_ci_impl;

pub use collate::{CollateMode, bytes_eq, compare_bytes};
