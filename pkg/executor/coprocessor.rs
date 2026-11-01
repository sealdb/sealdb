// Coprocessor-filter: evaluate pushed predicates at scan time (TiDB cop task).
// Completeness wave / not this feature: TiKV Coprocessor DAG / CopClient.
use sealdb_errno::Result;
use sealdb_expression::eval_filter;
use sealdb_parser::ast::ExprNode;
use sealdb_types::Datum;

pub fn filter_rows(
    rows: Vec<Vec<Datum>>,
    schema: &[String],
    pushed: Option<&ExprNode>,
) -> Result<Vec<Vec<Datum>>> {
    let Some(filter) = pushed else {
        return Ok(rows);
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        if eval_filter(filter, schema, &row)? {
            out.push(row);
        }
    }
    Ok(out)
}
