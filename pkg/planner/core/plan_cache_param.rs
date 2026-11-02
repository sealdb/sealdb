//! Prepared-statement plan-cache key helpers (TiDB `plan_cache_param.go`, MVB).
//!
//! Full ParamMarker AST rewrite / typed-param rebuild is deferred. MVB keeps the
//! prepared SQL template (with `?`) as the Domain plan-cache key so EXECUTE
//! with different USING literals shares one entry.

/// Normalize prepared SQL for use as a plan-cache key.
///
/// Collapses runs of whitespace and trims; leaves `?` markers intact so
/// `PREPARE … FROM '… WHERE id = ?'` maps all EXECUTE bindings to one key.
pub fn normalize_prep_cache_key(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut prev_space = true;
    for ch in sql.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
            continue;
        }
        out.push(ch);
        prev_space = false;
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_prep_cache_key_keeps_placeholders() {
        assert_eq!(
            normalize_prep_cache_key("  SELECT  name FROM t WHERE id = ?  "),
            "SELECT name FROM t WHERE id = ?"
        );
        assert_eq!(
            normalize_prep_cache_key("SELECT name FROM t WHERE id = ?"),
            normalize_prep_cache_key("SELECT  name  FROM  t  WHERE  id  =  ?")
        );
    }
}
