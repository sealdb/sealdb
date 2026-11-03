/// TiDB `privileges.SkipWithGrant`. keeps the flag on
/// `crate::Manager` (`skip_with_grant` / `with_grant_table`).
/// Completeness wave: process-wide package variable + config.
pub const SKIP_WITH_GRANT_DEFAULT: bool = true;
