use sealdb_errno::{ERR_INFO_SCHEMA_CHANGED, ERR_INFO_SCHEMA_EXPIRED, Error, Result};

/// TiDB `domain.SchemaChecker`: reject DML/SELECT in an explicit txn
/// after a concurrent DDL bumps the schema version (8028).
/// Schema lease expiry (8027) is checked via [`Domain::check_schema_lease`].
/// Concurrent DDL vs DML on the same table is gated by Domain MDL.
pub fn check(related_ver: i64, current_ver: i64) -> Result<()> {
    if related_ver != current_ver {
        return Err(Error::new(
            ERR_INFO_SCHEMA_CHANGED,
            "Information schema is changed during the execution of the statement(for example, table definition may be updated by other DDL ran in parallel). If you see this error often, try increasing `tidb_max_delta_schema_count`",
        ));
    }
    Ok(())
}

/// TiDB `ErrInfoSchemaExpired` (8027): local schema lease elapsed
/// without a reload.
pub fn expired() -> Error {
    Error::new(
        ERR_INFO_SCHEMA_EXPIRED,
        "Information schema is out of date: schema failed to update in 1 lease, please make sure TiDB can connect to TiKV",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schema_changed() {
        assert!(check(1, 1).is_ok());
        let err = check(1, 2).unwrap_err();
        assert_eq!(err.code, ERR_INFO_SCHEMA_CHANGED);
    }
}
