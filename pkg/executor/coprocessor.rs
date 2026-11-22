// Coprocessor-filter: evaluate pushed predicates at scan time (TiDB cop task).
// Completeness wave / not this feature: TiKV Coprocessor DAG / CopClient.
use sealdb_errno::Result;
use sealdb_expression::{
    collation_mode, eval_filter, validate_string_cmp_literals_on, with_collation,
};
use sealdb_meta::TableInfo;
use sealdb_parser::ast::ExprNode;
use sealdb_types::Datum;
use sealdb_util::collate::CollateMode;

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
    // Column vs literal comparisons take the column/table collation
    // (IMPLICIT beats COERCIBLE), not the connection default — so
    // `a < 'c'` on a `utf8mb4_general_ci` table excludes `'C'`.
    let mode = filter_collation(filter, info).unwrap_or_else(collation_mode);
    with_collation(mode, || {
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            if eval_filter(filter, schema, &row)? {
                out.push(row);
            }
        }
        Ok(out)
    })
}

/// Collation for evaluating a pushed filter: prefer a referenced string
/// column's FieldType, else the table default, else the session mode.
fn filter_collation(filter: &ExprNode, info: Option<&TableInfo>) -> Option<CollateMode> {
    // An explicit `COLLATE` (coercibility EXPLICIT) on either operand wins over
    // the column's implicit collation: `a = 'aa' COLLATE utf8mb4_bin` compares
    // byte-wise even on a `*_ci` column.
    if let Some(mode) = explicit_collation_in_expr(filter) {
        return Some(mode);
    }
    let info = info?;
    if let Some(mode) = column_collation_in_expr(filter, info) {
        return Some(mode);
    }
    if !info.collate.is_empty() {
        return Some(CollateMode::from_name(&info.collate));
    }
    None
}

/// Find a `expr COLLATE <name>` wrapper anywhere in the predicate.
fn explicit_collation_in_expr(expr: &ExprNode) -> Option<CollateMode> {
    match expr {
        ExprNode::Func(f) if f.name.eq_ignore_ascii_case("__collate") => {
            let name = f.args.get(1).and_then(|arg| match arg {
                ExprNode::Value(v) => match &v.datum {
                    sealdb_types::Datum::Bytes(bytes) => {
                        Some(String::from_utf8_lossy(bytes).into_owned())
                    }
                    _ => None,
                },
                _ => None,
            })?;
            Some(CollateMode::from_name(&name))
        }
        ExprNode::Binary(b) => {
            explicit_collation_in_expr(&b.left).or_else(|| explicit_collation_in_expr(&b.right))
        }
        ExprNode::Unary(u) => explicit_collation_in_expr(&u.expr),
        ExprNode::Func(f) => f.args.iter().find_map(explicit_collation_in_expr),
        ExprNode::InList(i) => explicit_collation_in_expr(&i.expr)
            .or_else(|| i.list.iter().find_map(explicit_collation_in_expr)),
        ExprNode::Between(b) => explicit_collation_in_expr(&b.expr)
            .or_else(|| explicit_collation_in_expr(&b.low))
            .or_else(|| explicit_collation_in_expr(&b.high)),
        ExprNode::Like(l) => {
            explicit_collation_in_expr(&l.expr).or_else(|| explicit_collation_in_expr(&l.pattern))
        }
        ExprNode::IsNull(i) => explicit_collation_in_expr(&i.expr),
        ExprNode::Cast(c) => explicit_collation_in_expr(&c.expr),
        _ => None,
    }
}

fn column_collation_in_expr(expr: &ExprNode, info: &TableInfo) -> Option<CollateMode> {
    match expr {
        ExprNode::Column(col) => info
            .columns
            .iter()
            .find(|c| c.name.o().eq_ignore_ascii_case(&col.name))
            .filter(|c| {
                !c.field_type.collate.is_empty()
                    && c.field_type.eval_type() == sealdb_types::field_type::EvalType::String
            })
            .map(|c| CollateMode::from_name(&c.field_type.collate)),
        ExprNode::Binary(b) => column_collation_in_expr(&b.left, info)
            .or_else(|| column_collation_in_expr(&b.right, info)),
        ExprNode::Unary(u) => column_collation_in_expr(&u.expr, info),
        ExprNode::Func(f) => f
            .args
            .iter()
            .find_map(|a| column_collation_in_expr(a, info)),
        ExprNode::InList(i) => column_collation_in_expr(&i.expr, info).or_else(|| {
            i.list
                .iter()
                .find_map(|a| column_collation_in_expr(a, info))
        }),
        ExprNode::Between(b) => column_collation_in_expr(&b.expr, info)
            .or_else(|| column_collation_in_expr(&b.low, info))
            .or_else(|| column_collation_in_expr(&b.high, info)),
        ExprNode::Like(l) => column_collation_in_expr(&l.expr, info)
            .or_else(|| column_collation_in_expr(&l.pattern, info)),
        ExprNode::IsNull(i) => column_collation_in_expr(&i.expr, info),
        ExprNode::Cast(c) => column_collation_in_expr(&c.expr, info),
        _ => None,
    }
}
