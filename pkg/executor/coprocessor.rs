// Coprocessor-filter: evaluate pushed predicates at scan time (TiDB cop task).
// Completeness wave / not this feature: TiKV Coprocessor DAG / CopClient.
use sealdb_errno::Result;
use sealdb_expression::{eval_filter, validate_string_cmp_literals_on};
use sealdb_meta::TableInfo;
use sealdb_parser::ast::ExprNode;
use sealdb_types::Datum;

pub fn filter_rows(
    rows: Vec<Vec<Datum>>,
    schema: &[String],
    pushed: Option<&ExprNode>,
    info: Option<&TableInfo>,
) -> Result<Vec<Vec<Datum>>> {
    let Some(filter) = pushed else {
        return Ok(rows);
    };
    // TiDB converts binary literals to the column charset before/while
    // comparing; invalid utf8 fails even when the scan returns no rows
    // (`select * from t3 where a > 0x80`).
    validate_string_cmp_literals_on(filter, info)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        if eval_filter(filter, schema, &row)? {
            out.push(row);
        }
    }
    Ok(out)
}
