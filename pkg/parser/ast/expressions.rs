use super::dml::{ByItem, ResultSetNode, SelectStmt};
use crate::format::RestoreCtx;
use crate::opcode::Op;
use sealdb_errno::Result;
use sealdb_types::Datum;

#[derive(Clone, Debug)]
pub enum ExprNode {
    Value(ValueExpr),
    Column(ColumnNameExpr),
    Binary(BinaryOpExpr),
    Unary(UnaryOpExpr),
    Func(FuncCallExpr),
    Cast(FuncCastExpr),
    Exists(ExistsExpr),
    InSubquery(InSubqueryExpr),
    InList(InListExpr),
    Between(BetweenExpr),
    Like(PatternLikeExpr),
    IsNull(IsNullExpr),
    /// `CASE [operand] WHEN … THEN … [ELSE …] END`.
    Case(CaseExpr),
    ScalarSubquery(ScalarSubqueryExpr),
    /// Quantified comparison: `expr op ANY|ALL (subquery)`.
    /// `= ANY` / `= SOME` lower to `InSubquery`; `<> ALL` lowers to `NOT IN`.
    QuantifiedCmp(QuantifiedCmpExpr),
    Window(WindowFuncExpr),
    /// Row constructor `(a, b)` (TiDB `*ast.RowExpr`). Only meaningful as the
    /// left operand of `IN` / quantified comparison subqueries, which lower to
    /// a null-aware anti join (`rule_semi_join` / `planbuilder::naaj_plan`).
    Row(RowExpr),
}

impl ExprNode {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        match self {
            ExprNode::Value(v) => v.restore(ctx),
            ExprNode::Column(v) => v.restore(ctx),
            ExprNode::Binary(v) => v.restore(ctx),
            ExprNode::Unary(v) => v.restore(ctx),
            ExprNode::Func(v) => v.restore(ctx),
            ExprNode::Cast(v) => v.restore(ctx),
            ExprNode::Exists(v) => v.restore(ctx),
            ExprNode::InSubquery(v) => v.restore(ctx),
            ExprNode::InList(v) => v.restore(ctx),
            ExprNode::Between(v) => v.restore(ctx),
            ExprNode::Like(v) => v.restore(ctx),
            ExprNode::IsNull(v) => v.restore(ctx),
            ExprNode::Case(v) => v.restore(ctx),
            ExprNode::ScalarSubquery(v) => v.restore(ctx),
            ExprNode::QuantifiedCmp(v) => v.restore(ctx),
            ExprNode::Window(v) => v.restore(ctx),
            ExprNode::Row(v) => v.restore(ctx),
        }
    }

    pub fn has_subquery(&self) -> bool {
        match self {
            ExprNode::Exists(_)
            | ExprNode::InSubquery(_)
            | ExprNode::ScalarSubquery(_)
            | ExprNode::QuantifiedCmp(_) => true,
            ExprNode::Binary(b) => b.left.has_subquery() || b.right.has_subquery(),
            ExprNode::Unary(u) => u.expr.has_subquery(),
            ExprNode::Func(f) => f.args.iter().any(Self::has_subquery),
            ExprNode::Cast(c) => c.expr.has_subquery(),
            ExprNode::InList(i) => i.expr.has_subquery() || i.list.iter().any(Self::has_subquery),
            ExprNode::Between(b) => {
                b.expr.has_subquery() || b.low.has_subquery() || b.high.has_subquery()
            }
            ExprNode::Like(l) => l.expr.has_subquery() || l.pattern.has_subquery(),
            ExprNode::IsNull(i) => i.expr.has_subquery(),
            ExprNode::Case(c) => {
                c.operand.as_ref().is_some_and(|o| o.has_subquery())
                    || c.when_then
                        .iter()
                        .any(|(w, t)| w.has_subquery() || t.has_subquery())
                    || c.else_expr.as_ref().is_some_and(|e| e.has_subquery())
            }
            ExprNode::Window(w) => {
                w.args.iter().any(Self::has_subquery)
                    || w.spec.partition_by.iter().any(Self::has_subquery)
                    || w.spec.order_by.iter().any(|item| item.expr.has_subquery())
            }
            ExprNode::Row(r) => r.exprs.iter().any(Self::has_subquery),
            ExprNode::Value(_) | ExprNode::Column(_) => false,
        }
    }

    pub fn restore_string(&self) -> String {
        let mut ctx = RestoreCtx::new();
        let _ = self.restore(&mut ctx);
        ctx.into_string()
    }

    /// TiDB `ColumnInfo.GeneratedExprString`: lowercase keywords, back-quoted
    /// names and `_utf8mb4` string introducers.
    pub fn restore_string_generated(&self) -> String {
        let mut ctx = RestoreCtx::new_generated();
        let _ = self.restore(&mut ctx);
        ctx.into_string()
    }

    /// `*` select-item marker used by the bridge before star expansion.
    pub fn is_star_marker(&self) -> bool {
        matches!(
            self,
            ExprNode::Value(ValueExpr {
                datum: Datum::Bytes(b)
            }) if b == b"*"
        )
    }

    /// TiDB `ast.ExprNode.Text()`-style rendering used to derive a select
    /// field's implicit name: identifiers unquoted, functions lowercased,
    /// binary operators without surrounding spaces.
    pub fn text(&self) -> String {
        match self {
            ExprNode::Column(c) => {
                let mut out = String::new();
                if let Some(schema) = &c.schema {
                    out.push_str(schema);
                    out.push('.');
                }
                if let Some(table) = &c.table {
                    out.push_str(table);
                    out.push('.');
                }
                out.push_str(&c.name);
                out
            }
            ExprNode::Value(v) => match &v.datum {
                Datum::Bytes(b) => {
                    format!("'{}'", String::from_utf8_lossy(b).replace('\'', "''"))
                }
                Datum::Bit(b) => format!("'{}'", String::from_utf8_lossy(b)),
                Datum::Enum(e) => format!("'{}'", String::from_utf8_lossy(&e.label)),
                Datum::I64(n) => n.to_string(),
                Datum::Year(n) => n.to_string(),
                Datum::U64(n) => n.to_string(),
                Datum::F64(f) => {
                    if f.fract() == 0.0 && f.is_finite() {
                        format!("{f}")
                    } else {
                        f.to_string()
                    }
                }
                Datum::Decimal(d) => d.to_string(),
                Datum::Time(t) => format!("{t}"),
                Datum::Null => "NULL".to_string(),
            },
            ExprNode::Unary(u) => format!("{}{}", u.op.restore(), u.expr.text()),
            ExprNode::Binary(b) => {
                if matches!(b.op, Op::LogicAnd | Op::LogicOr | Op::LogicXor | Op::IntDiv) {
                    format!("{} {} {}", b.left.text(), b.op.restore(), b.right.text())
                } else {
                    format!("{}{}{}", b.left.text(), b.op.restore(), b.right.text())
                }
            }
            ExprNode::Func(f) => {
                let mut out = f.name.to_ascii_lowercase();
                out.push('(');
                if f.distinct {
                    out.push_str("distinct ");
                }
                if f.name.eq_ignore_ascii_case("COUNT") && f.args.is_empty() {
                    out.push('*');
                } else {
                    for (i, arg) in f.args.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        out.push_str(&arg.text());
                    }
                }
                out.push(')');
                out
            }
            ExprNode::Cast(c) => format!("cast({} as {})", c.expr.text(), c.target),
            other => other.restore_string(),
        }
    }

    /// TiDB's implicit select-field name (`SelectField` without `AS`). Returns
    /// an empty string when the expression has no usable name (subqueries), so
    /// callers can fall back to `name_exp_N`.
    pub fn name(&self) -> String {
        match self {
            ExprNode::Column(c) => c.name.clone(),
            ExprNode::Value(v) => match &v.datum {
                Datum::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
                Datum::Bit(b) => String::from_utf8_lossy(b).into_owned(),
                Datum::Enum(e) => String::from_utf8_lossy(&e.label).into_owned(),
                Datum::I64(n) => n.to_string(),
                Datum::Year(n) => n.to_string(),
                Datum::U64(n) => n.to_string(),
                Datum::F64(f) => {
                    if f.fract() == 0.0 && f.is_finite() {
                        format!("{f}")
                    } else {
                        f.to_string()
                    }
                }
                Datum::Decimal(d) => d.to_string(),
                Datum::Time(t) => format!("{t}"),
                Datum::Null => "NULL".to_string(),
            },
            ExprNode::Func(_) | ExprNode::Cast(_) | ExprNode::Binary(_) | ExprNode::Unary(_) => {
                self.text()
            }
            _ => String::new(),
        }
    }

    pub fn split_cnf(&self) -> Vec<ExprNode> {
        match self {
            ExprNode::Binary(bin) if bin.op == Op::LogicAnd && !bin.single_cnf_item => {
                let mut out = bin.left.split_cnf();
                out.extend(bin.right.split_cnf());
                out
            }
            other => vec![other.clone()],
        }
    }

    /// Split the expression into CNF conjuncts, ignoring
    /// `single_cnf_item` marks. Estimation and pruning need the plain
    /// conjunct list even when `EXPLAIN` keeps an `AND` as one item.
    pub fn split_cnf_all(&self) -> Vec<ExprNode> {
        match self {
            ExprNode::Binary(bin) if bin.op == Op::LogicAnd => {
                let mut out = bin.left.split_cnf_all();
                out.extend(bin.right.split_cnf_all());
                out
            }
            other => vec![other.clone()],
        }
    }

    /// Split a DNF (OR of AND-arms). Pure equality arms stay as single nodes.
    pub fn split_dnf(&self) -> Vec<ExprNode> {
        match self {
            ExprNode::Binary(bin) if bin.op == Op::LogicOr => {
                let mut out = bin.left.split_dnf();
                out.extend(bin.right.split_dnf());
                out
            }
            other => vec![other.clone()],
        }
    }

    pub fn compose_cnf(exprs: Vec<ExprNode>) -> Option<ExprNode> {
        exprs.into_iter().reduce(|left, right| {
            ExprNode::Binary(BinaryOpExpr {
                op: Op::LogicAnd,
                left: Box::new(left),
                right: Box::new(right),
                single_cnf_item: false,
            })
        })
    }

    pub fn referenced_columns(&self) -> Vec<(Option<String>, String)> {
        let mut out = Vec::new();
        self.collect_columns(&mut out);
        out
    }

    /// Fold a literal constant (including unary `-`/`+`) to a datum.
    pub fn const_datum(&self) -> Option<Datum> {
        match self {
            ExprNode::Value(v) => Some(v.datum.clone()),
            ExprNode::Unary(u) if matches!(u.op, Op::UnaryMinus | Op::Minus | Op::Plus) => {
                let inner = Self::const_datum(&u.expr)?;
                if matches!(u.op, Op::Plus) {
                    return Some(inner);
                }
                Some(match inner {
                    Datum::I64(v) => Datum::I64(v.checked_neg()?),
                    Datum::U64(v) if v == 1u64 << 63 => Datum::I64(i64::MIN),
                    Datum::U64(v) if v <= i64::MAX as u64 => Datum::I64(-(v as i64)),
                    Datum::F64(v) => Datum::F64(-v),
                    Datum::Decimal(d) => Datum::Decimal(d.negated()),
                    _ => return None,
                })
            }
            _ => None,
        }
    }

    pub fn eq_column_const(&self) -> Option<(String, Datum)> {
        let ExprNode::Binary(bin) = self else {
            return None;
        };
        // `=` and null-safe `<=>` both drive index equality access (TiDB).
        if !matches!(bin.op, Op::Eq | Op::NullSafeEq) {
            return None;
        }
        match (bin.left.as_ref(), bin.right.as_ref()) {
            (ExprNode::Column(col), right) | (right, ExprNode::Column(col)) => {
                right.const_datum().map(|value| (col.name.clone(), value))
            }
            _ => None,
        }
    }

    /// `expr = const` on any left-hand side, keyed by [`ExprNode::expr_key`]:
    /// a plain column reports its name, an expression reports its canonical
    /// text, which is how a predicate matches an expression index part
    /// (`tidb_shard(a) = 8` → key `tidb_shard(a)`).
    pub fn eq_key_const(&self) -> Option<(String, Datum)> {
        if let Some(hit) = self.eq_column_const() {
            return Some(hit);
        }
        let ExprNode::Binary(bin) = self else {
            return None;
        };
        if !matches!(bin.op, Op::Eq | Op::NullSafeEq) {
            return None;
        }
        match (bin.left.as_ref(), bin.right.as_ref()) {
            (left, right) if !matches!(left, ExprNode::Column(_)) => {
                right.const_datum().map(|value| (left.expr_key(), value))
            }
            (left, right) if !matches!(right, ExprNode::Column(_)) => {
                left.const_datum().map(|value| (right.expr_key(), value))
            }
            _ => None,
        }
    }

    /// Canonical text of an expression, used to match an index key part with
    /// the predicate that constrains it. Table qualifiers and quoting are
    /// dropped, so `` tidb_shard(`a`) `` and `test3.a`'s `tidb_shard(test3.a)`
    /// share the key `tidb_shard(a)`.
    pub fn expr_key(&self) -> String {
        let mut text = String::new();
        self.write_expr_key(&mut text);
        text
    }

    fn write_expr_key(&self, out: &mut String) {
        match self {
            ExprNode::Column(col) => out.push_str(&col.name.to_ascii_lowercase()),
            ExprNode::Value(value) => {
                let mut ctx = RestoreCtx::new();
                let _ = value.restore(&mut ctx);
                out.push_str(&ctx.into_string());
            }
            ExprNode::Binary(bin) => {
                out.push('(');
                bin.left.write_expr_key(out);
                out.push_str(bin.op.restore());
                bin.right.write_expr_key(out);
                out.push(')');
            }
            ExprNode::Unary(unary) => {
                out.push_str(unary.op.restore());
                unary.expr.write_expr_key(out);
            }
            ExprNode::Func(func) => {
                out.push_str(&func.name.to_ascii_lowercase());
                out.push('(');
                for (i, arg) in func.args.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    arg.write_expr_key(out);
                }
                out.push(')');
            }
            ExprNode::Cast(cast) => {
                out.push_str("cast(");
                cast.expr.write_expr_key(out);
                out.push_str(" as ");
                out.push_str(&cast.target.to_ascii_lowercase());
                out.push(')');
            }
            // Anything else falls back to the restored SQL text with the
            // quoting removed, which is still stable across qualifiers.
            other => {
                out.push_str(
                    &other
                        .restore_string()
                        .replace(['`', '"'], "")
                        .to_ascii_lowercase(),
                );
            }
        }
    }

    pub fn eq_const_on(&self, column: &str) -> Option<Datum> {
        self.split_cnf().into_iter().find_map(|expr| {
            expr.eq_column_const()
                .and_then(|(name, value)| name.eq_ignore_ascii_case(column).then_some(value))
        })
    }

    pub fn column_in_consts(&self) -> Option<(String, Vec<Datum>)> {
        self.column_in_consts_with_len()
            .map(|(name, values, _)| (name, values))
    }

    /// Like `column_in_consts`, but also reports the source list length so
    /// BatchPointGet estRows keeps duplicates (`IN (1,2,3,1,2)` → 5).
    pub fn column_in_consts_with_len(&self) -> Option<(String, Vec<Datum>, usize)> {
        let ExprNode::InList(list) = self else {
            return None;
        };
        if list.not {
            return None;
        }
        let ExprNode::Column(col) = list.expr.as_ref() else {
            return None;
        };
        let mut values = Vec::new();
        for item in &list.list {
            let Some(value) = item.const_datum() else {
                return None;
            };
            if value.is_null() {
                continue;
            }
            if !values
                .iter()
                .any(|have: &Datum| have.compare(&value).is_eq())
            {
                values.push(value);
            }
        }
        Some((col.name.clone(), values, list.list.len()))
    }

    pub fn in_const_on(&self, column: &str) -> Option<Vec<Datum>> {
        self.split_cnf().into_iter().find_map(|expr| {
            expr.column_in_consts()
                .and_then(|(name, values)| name.eq_ignore_ascii_case(column).then_some(values))
        })
    }

    /// `(a, b) IN ((1, 2), (3, 4))`: row column names plus deduped constant
    /// tuples, and the source list length for BatchPointGet estRows.
    pub fn row_in_consts_with_len(&self) -> Option<(Vec<String>, Vec<Vec<Datum>>, usize)> {
        let ExprNode::InList(list) = self else {
            return None;
        };
        if list.not {
            return None;
        }
        let ExprNode::Row(row) = list.expr.as_ref() else {
            return None;
        };
        let mut names = Vec::with_capacity(row.exprs.len());
        for expr in &row.exprs {
            let ExprNode::Column(col) = expr else {
                return None;
            };
            names.push(col.name.clone());
        }
        if names.is_empty() {
            return None;
        }
        let mut tuples: Vec<Vec<Datum>> = Vec::new();
        for item in &list.list {
            let ExprNode::Row(tuple) = item else {
                return None;
            };
            if tuple.exprs.len() != names.len() {
                return None;
            }
            let mut values = Vec::with_capacity(tuple.exprs.len());
            for expr in &tuple.exprs {
                values.push(expr.const_datum()?);
            }
            if !tuples.iter().any(|have| {
                have.len() == values.len()
                    && have
                        .iter()
                        .zip(values.iter())
                        .all(|(a, b)| a.compare(b).is_eq())
            }) {
                tuples.push(values);
            }
        }
        Some((names, tuples, list.list.len()))
    }

    /// `(a = 1 AND b = 2) OR (a = 3 AND b = 4)` → constant tuples ordered by
    /// `columns`. Used by batch point gets planned from a DNF of equalities.
    pub fn dnf_const_tuples(&self, columns: &[String]) -> Option<Vec<Vec<Datum>>> {
        let arms = self.split_dnf();
        if arms.len() < 2 {
            return None;
        }
        let mut out = Vec::with_capacity(arms.len());
        for arm in &arms {
            let eqs: Vec<(String, Datum)> = arm
                .split_cnf_all()
                .into_iter()
                .filter_map(|expr| expr.eq_column_const())
                .collect();
            let mut tuple = Vec::with_capacity(columns.len());
            for column in columns {
                tuple.push(
                    eqs.iter()
                        .find(|(col, _)| col.eq_ignore_ascii_case(column))
                        .map(|(_, value)| value.clone())?,
                );
            }
            out.push(tuple);
        }
        Some(out)
    }

    /// TiDB renders a row-value `IN` as the equivalent DNF of equalities once
    /// it survives into a scan filter: `(a,b) IN ((1,2))` → `a = 1 AND b = 2`.
    pub fn rewrite_row_in_dnf(&self) -> ExprNode {
        match self {
            ExprNode::InList(list) if !list.not => {
                let ExprNode::Row(row) = list.expr.as_ref() else {
                    return self.clone();
                };
                if !row
                    .exprs
                    .iter()
                    .all(|expr| matches!(expr, ExprNode::Column(_)))
                {
                    return self.clone();
                }
                let mut arms = Vec::with_capacity(list.list.len());
                for item in &list.list {
                    let ExprNode::Row(tuple) = item else {
                        return self.clone();
                    };
                    if tuple.exprs.len() != row.exprs.len() {
                        return self.clone();
                    }
                    let mut conjuncts = Vec::with_capacity(tuple.exprs.len());
                    for (column, value) in row.exprs.iter().zip(tuple.exprs.iter()) {
                        let ExprNode::Value(value) = value else {
                            return self.clone();
                        };
                        conjuncts.push(ExprNode::Binary(BinaryOpExpr {
                            op: Op::Eq,
                            left: Box::new(column.clone()),
                            right: Box::new(ExprNode::Value(value.clone())),
                            single_cnf_item: false,
                        }));
                    }
                    match ExprNode::compose_cnf(conjuncts) {
                        Some(arm) => arms.push(arm),
                        None => return self.clone(),
                    }
                }
                arms.into_iter()
                    .reduce(|left, right| {
                        ExprNode::Binary(BinaryOpExpr {
                            op: Op::LogicOr,
                            left: Box::new(left),
                            right: Box::new(right),
                            single_cnf_item: false,
                        })
                    })
                    .unwrap_or_else(|| self.clone())
            }
            ExprNode::Binary(bin) => ExprNode::Binary(BinaryOpExpr {
                op: bin.op,
                left: Box::new(bin.left.rewrite_row_in_dnf()),
                right: Box::new(bin.right.rewrite_row_in_dnf()),
                single_cnf_item: bin.single_cnf_item,
            }),
            other => other.clone(),
        }
    }

    pub fn column_cmp_const(&self) -> Option<(String, Op, Datum)> {
        let ExprNode::Binary(bin) = self else {
            return None;
        };
        // A signed literal parses as `-(1)`, so fold the constant side rather
        // than only accepting a bare `Value` (`c1 < -1` drives a range in TiDB).
        match (bin.left.as_ref(), bin.right.as_ref()) {
            (ExprNode::Column(col), right)
                if matches!(bin.op, Op::Lt | Op::Le | Op::Gt | Op::Ge) =>
            {
                right
                    .const_datum()
                    .map(|value| (col.name.clone(), bin.op, value))
            }
            (left, ExprNode::Column(col)) => flip_cmp(bin.op).and_then(|op| {
                left.const_datum()
                    .map(|value| (col.name.clone(), op, value))
            }),
            _ => None,
        }
    }

    /// `col <> const` / `const <> col` — `ranger` splits it in two ranges.
    pub fn ne_column_const(&self) -> Option<(String, Datum)> {
        let ExprNode::Binary(bin) = self else {
            return None;
        };
        if bin.op != Op::Ne {
            return None;
        }
        match (bin.left.as_ref(), bin.right.as_ref()) {
            (ExprNode::Column(col), right) => {
                right.const_datum().map(|value| (col.name.clone(), value))
            }
            (left, ExprNode::Column(col)) => {
                left.const_datum().map(|value| (col.name.clone(), value))
            }
            _ => None,
        }
    }

    pub fn between_column_const(&self) -> Option<(String, Datum, Datum)> {
        let ExprNode::Between(b) = self else {
            return None;
        };
        if b.not {
            return None;
        }
        let ExprNode::Column(col) = b.expr.as_ref() else {
            return None;
        };
        let low = b.low.const_datum()?;
        let high = b.high.const_datum()?;
        Some((col.name.clone(), low, high))
    }

    pub fn column_ranges(&self) -> Vec<ColumnRange> {
        let mut ranges = Vec::new();
        for item in self.split_cnf() {
            if let Some((name, op, value)) = item.column_cmp_const() {
                apply_cmp_range(&mut ranges, name, op, value);
            } else if let Some((name, low, high)) = item.between_column_const() {
                apply_cmp_range(&mut ranges, name.clone(), Op::Ge, low);
                apply_cmp_range(&mut ranges, name, Op::Le, high);
            }
        }
        ranges
    }

    pub fn collect_columns(&self, out: &mut Vec<(Option<String>, String)>) {
        match self {
            ExprNode::Column(col) => out.push((col.table.clone(), col.name.clone())),
            ExprNode::Binary(b) => {
                b.left.collect_columns(out);
                b.right.collect_columns(out);
            }
            ExprNode::Unary(u) => u.expr.collect_columns(out),
            ExprNode::Func(f) => {
                for arg in &f.args {
                    arg.collect_columns(out);
                }
            }
            ExprNode::Cast(c) => c.expr.collect_columns(out),
            ExprNode::InList(i) => {
                i.expr.collect_columns(out);
                for item in &i.list {
                    item.collect_columns(out);
                }
            }
            ExprNode::Between(b) => {
                b.expr.collect_columns(out);
                b.low.collect_columns(out);
                b.high.collect_columns(out);
            }
            ExprNode::Like(l) => {
                l.expr.collect_columns(out);
                l.pattern.collect_columns(out);
            }
            ExprNode::IsNull(i) => i.expr.collect_columns(out),
            ExprNode::Case(c) => {
                if let Some(operand) = &c.operand {
                    operand.collect_columns(out);
                }
                for (when, then) in &c.when_then {
                    when.collect_columns(out);
                    then.collect_columns(out);
                }
                if let Some(else_expr) = &c.else_expr {
                    else_expr.collect_columns(out);
                }
            }
            ExprNode::Window(w) => {
                for arg in &w.args {
                    arg.collect_columns(out);
                }
                for part in &w.spec.partition_by {
                    part.collect_columns(out);
                }
                for item in &w.spec.order_by {
                    item.expr.collect_columns(out);
                }
            }
            ExprNode::Exists(e) => collect_result_set_columns(&e.query, out),
            ExprNode::InSubquery(i) => {
                i.expr.collect_columns(out);
                collect_result_set_columns(&i.query, out);
            }
            ExprNode::ScalarSubquery(s) => collect_result_set_columns(&s.query, out),
            ExprNode::QuantifiedCmp(q) => {
                q.expr.collect_columns(out);
                collect_result_set_columns(&q.query, out);
            }
            ExprNode::Row(r) => {
                for expr in &r.exprs {
                    expr.collect_columns(out);
                }
            }
            ExprNode::Value(_) => {}
        }
    }
}

fn collect_result_set_columns(query: &ResultSetNode, out: &mut Vec<(Option<String>, String)>) {
    for select in query.selects() {
        collect_select_columns(select, out);
    }
}

fn collect_select_columns(query: &SelectStmt, out: &mut Vec<(Option<String>, String)>) {
    if let Some(selection) = &query.selection {
        selection.collect_columns(out);
    }
}

#[derive(Clone, Debug)]
pub struct ColumnNameExpr {
    pub schema: Option<String>,
    pub table: Option<String>,
    pub name: String,
}

impl ColumnNameExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        if let Some(schema) = &self.schema {
            ctx.write_name(schema);
            ctx.write_str(".");
        }
        if let Some(table) = &self.table {
            ctx.write_name(table);
            ctx.write_str(".");
        }
        // Session variables are not identifiers: TiDB prints `@a`, never `` `@a` ``.
        if self.name.starts_with('@') {
            ctx.write_str(&self.name);
        } else {
            ctx.write_name(&self.name);
        }
        Ok(())
    }
}

/// Escape a string for a MySQL single-quoted literal. Restored text is fed back
/// into the parser (generated columns, views), so a literal backslash has to be
/// doubled for the unescaped value to round-trip.
fn escape_mysql_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "''")
}

#[derive(Clone, Debug)]
pub struct ValueExpr {
    pub datum: Datum,
}

impl ValueExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        if (ctx.in_view() || ctx.in_generated())
            && matches!(
                &self.datum,
                Datum::Bytes(_) | Datum::Bit(_) | Datum::Enum(_)
            )
        {
            // TiDB restores string literals with their charset introducer.
            ctx.write_str(if ctx.in_view() {
                "_UTF8MB4"
            } else {
                "_utf8mb4"
            });
        }
        ctx.write_str(&match &self.datum {
            Datum::Null => "NULL".to_string(),
            Datum::I64(v) => v.to_string(),
            Datum::Year(v) => v.to_string(),
            Datum::U64(v) => v.to_string(),
            Datum::F64(v) => {
                if v.fract() == 0.0 && v.is_finite() {
                    format!("{v}")
                } else {
                    v.to_string()
                }
            }
            Datum::Bytes(b) => format!("'{}'", escape_mysql_string(&String::from_utf8_lossy(b))),
            Datum::Bit(b) => format!("'{}'", escape_mysql_string(&String::from_utf8_lossy(b))),
            Datum::Decimal(d) => d.to_string(),
            Datum::Time(t) => format!("'{t}'"),
            Datum::Enum(e) => format!(
                "'{}'",
                escape_mysql_string(&String::from_utf8_lossy(&e.label))
            ),
        });
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct BinaryOpExpr {
    pub op: Op,
    pub left: Box<ExprNode>,
    pub right: Box<ExprNode>,
    /// Keep this `AND` as one predicate item instead of splitting it into
    /// separate CNF conjuncts. TiDB splits the pushed-down predicate list into
    /// CNF items while pushing down; an `AND` synthesized later by
    /// `PushDownNot` (e.g. `NOT (a < 2 OR a > -1)`) stays a single item and
    /// `EXPLAIN` prints it as `and(...)` rather than comma-joining the sides.
    pub single_cnf_item: bool,
}

impl BinaryOpExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        self.left.restore(ctx)?;
        // TiDB view restore writes `a=1` / `a+1` (no spaces) but keeps word
        // operators spaced (` AND ` / ` OR ` / ` XOR `).
        let word = matches!(
            self.op,
            Op::LogicAnd | Op::LogicOr | Op::LogicXor | Op::IntDiv
        );
        if word || !ctx.in_view() {
            ctx.write_str(" ");
        }
        ctx.write_str(self.op.restore());
        if word || !ctx.in_view() {
            ctx.write_str(" ");
        }
        self.right.restore(ctx)
    }
}

#[derive(Clone, Debug)]
pub struct UnaryOpExpr {
    pub op: Op,
    pub expr: Box<ExprNode>,
}

impl UnaryOpExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str(self.op.restore());
        if matches!(self.op, Op::Not) {
            ctx.write_str(" ");
        }
        // TiDB keeps ParenthesesExpr under `! (expr)`; sqlparser bang rewrite
        // drops call/paren nodes, so re-wrap compound children for display.
        let wrap = matches!(self.op, Op::Not2)
            && matches!(
                self.expr.as_ref(),
                ExprNode::Binary(_)
                    | ExprNode::Unary(_)
                    | ExprNode::Func(_)
                    | ExprNode::Cast(_)
                    | ExprNode::Between(_)
                    | ExprNode::InList(_)
                    | ExprNode::InSubquery(_)
                    | ExprNode::Like(_)
                    | ExprNode::IsNull(_)
                    | ExprNode::Case(_)
                    | ExprNode::QuantifiedCmp(_)
                    | ExprNode::Window(_)
            );
        if wrap {
            ctx.write_str("(");
        }
        self.expr.restore(ctx)?;
        if wrap {
            ctx.write_str(")");
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct FuncCallExpr {
    pub name: String,
    pub args: Vec<ExprNode>,
    pub distinct: bool,
}

impl FuncCallExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        // `IS [NOT] TRUE|FALSE|UNKNOWN` round-trips as a predicate.
        if let Some(suffix) = is_truth_suffix(&self.name)
            && let Some(arg) = self.args.first()
        {
            arg.restore(ctx)?;
            ctx.write_str(suffix);
            return Ok(());
        }
        if ctx.in_view() {
            ctx.write_str(&self.name.to_ascii_uppercase());
        } else if ctx.in_generated() {
            ctx.write_str(&self.name.to_ascii_lowercase());
        } else {
            ctx.write_str(&self.name);
        }
        ctx.write_str("(");
        if self.distinct {
            ctx.write_str("DISTINCT ");
        }
        if self.name.eq_ignore_ascii_case("COUNT") && self.args.is_empty() {
            ctx.write_str("*");
        } else {
            for (i, arg) in self.args.iter().enumerate() {
                if i > 0 {
                    ctx.write_str(", ");
                }
                // `TIMESTAMPADD`/`TIMESTAMPDIFF` keep their unit as a bare
                // keyword (`timestampdiff(microsecond, …)`), not a string.
                if i == 0
                    && (self.name.eq_ignore_ascii_case("TIMESTAMPDIFF")
                        || self.name.eq_ignore_ascii_case("TIMESTAMPADD"))
                    && let ExprNode::Value(v) = arg
                    && let Datum::Bytes(unit) = &v.datum
                {
                    ctx.write_str(&String::from_utf8_lossy(unit));
                    continue;
                }
                arg.restore(ctx)?;
            }
        }
        ctx.write_str(")");
        Ok(())
    }
}

/// SQL text for the `IS [NOT] …` scalar functions lowered from
/// `Expr::IsTrue` / `IsFalse` / `IsUnknown` (and negations).
pub fn is_truth_suffix(name: &str) -> Option<&'static str> {
    if name.eq_ignore_ascii_case("istrue") {
        Some(" is true")
    } else if name.eq_ignore_ascii_case("isnottrue") {
        Some(" is not true")
    } else if name.eq_ignore_ascii_case("isfalse") {
        Some(" is false")
    } else if name.eq_ignore_ascii_case("isnotfalse") {
        Some(" is not false")
    } else if name.eq_ignore_ascii_case("isunknown") {
        Some(" is unknown")
    } else if name.eq_ignore_ascii_case("isnotunknown") {
        Some(" is not unknown")
    } else {
        None
    }
}

#[derive(Clone, Debug)]
pub struct FuncCastExpr {
    pub expr: Box<ExprNode>,
    pub target: String,
}

impl FuncCastExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        if ctx.in_generated() {
            // TiDB stores the generated expression with lowercased keywords.
            ctx.write_str("cast(");
            self.expr.restore(ctx)?;
            ctx.write_str(" as ");
            ctx.write_str(&self.target.to_ascii_lowercase());
            ctx.write_str(")");
            return Ok(());
        }
        ctx.write_str("CAST(");
        self.expr.restore(ctx)?;
        ctx.write_str(" AS ");
        ctx.write_str(&self.target);
        ctx.write_str(")");
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ExistsExpr {
    pub not: bool,
    pub query: Box<ResultSetNode>,
}

impl ExistsExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        if self.not {
            ctx.write_str("NOT ");
        }
        ctx.write_str("EXISTS (");
        self.query.restore(ctx)?;
        ctx.write_str(")");
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct InSubqueryExpr {
    pub expr: Box<ExprNode>,
    pub not: bool,
    pub query: Box<ResultSetNode>,
    /// `expr != ALL (subquery)`: TiDB keeps the quantified form
    /// (`*ast.CompareSubqueryExpr`) instead of folding to `NOT IN`, so the
    /// planner can pick the left-outer-semi shape.
    pub all_ne: bool,
}

impl InSubqueryExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        self.expr.restore(ctx)?;
        if self.all_ne {
            ctx.write_str(" != ALL (");
        } else if self.not {
            ctx.write_str(" NOT IN (");
        } else {
            ctx.write_str(" IN (");
        }
        self.query.restore(ctx)?;
        ctx.write_str(")");
        Ok(())
    }
}

/// Row constructor `(a, b)` — TiDB `*ast.RowExpr`.
#[derive(Clone, Debug)]
pub struct RowExpr {
    pub exprs: Vec<ExprNode>,
}

impl RowExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("(");
        for (i, expr) in self.exprs.iter().enumerate() {
            if i > 0 {
                ctx.write_str(", ");
            }
            expr.restore(ctx)?;
        }
        ctx.write_str(")");
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct InListExpr {
    pub expr: Box<ExprNode>,
    pub not: bool,
    pub list: Vec<ExprNode>,
}

impl InListExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        self.expr.restore(ctx)?;
        if self.not {
            ctx.write_str(" NOT IN (");
        } else {
            ctx.write_str(" IN (");
        }
        for (i, item) in self.list.iter().enumerate() {
            if i > 0 {
                ctx.write_str(",");
            }
            item.restore(ctx)?;
        }
        ctx.write_str(")");
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ColumnRange {
    pub column: String,
    pub low: Option<(Datum, bool)>,
    pub high: Option<(Datum, bool)>,
}

fn flip_cmp(op: Op) -> Option<Op> {
    match op {
        Op::Lt => Some(Op::Gt),
        Op::Le => Some(Op::Ge),
        Op::Gt => Some(Op::Lt),
        Op::Ge => Some(Op::Le),
        _ => None,
    }
}

fn apply_cmp_range(ranges: &mut Vec<ColumnRange>, name: String, op: Op, value: Datum) {
    let slot = match ranges
        .iter()
        .position(|range| range.column.eq_ignore_ascii_case(&name))
    {
        Some(idx) => idx,
        None => {
            ranges.push(ColumnRange {
                column: name,
                low: None,
                high: None,
            });
            ranges.len() - 1
        }
    };
    match op {
        Op::Gt => tighten_low(&mut ranges[slot].low, value, false),
        Op::Ge => tighten_low(&mut ranges[slot].low, value, true),
        Op::Lt => tighten_high(&mut ranges[slot].high, value, false),
        Op::Le => tighten_high(&mut ranges[slot].high, value, true),
        _ => {}
    }
}

fn tighten_low(slot: &mut Option<(Datum, bool)>, value: Datum, inclusive: bool) {
    *slot = Some(match slot.take() {
        None => (value, inclusive),
        Some((existing, existing_incl)) => match value.compare(&existing) {
            std::cmp::Ordering::Greater => (value, inclusive),
            std::cmp::Ordering::Less => (existing, existing_incl),
            std::cmp::Ordering::Equal => (existing, existing_incl && inclusive),
        },
    });
}

fn tighten_high(slot: &mut Option<(Datum, bool)>, value: Datum, inclusive: bool) {
    *slot = Some(match slot.take() {
        None => (value, inclusive),
        Some((existing, existing_incl)) => match value.compare(&existing) {
            std::cmp::Ordering::Less => (value, inclusive),
            std::cmp::Ordering::Greater => (existing, existing_incl),
            std::cmp::Ordering::Equal => (existing, existing_incl && inclusive),
        },
    });
}

#[derive(Clone, Debug)]
pub struct BetweenExpr {
    pub expr: Box<ExprNode>,
    pub not: bool,
    pub low: Box<ExprNode>,
    pub high: Box<ExprNode>,
}

impl BetweenExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        self.expr.restore(ctx)?;
        if self.not {
            ctx.write_str(" NOT BETWEEN ");
        } else {
            ctx.write_str(" BETWEEN ");
        }
        self.low.restore(ctx)?;
        ctx.write_str(" AND ");
        self.high.restore(ctx)
    }
}

/// `CASE [operand] WHEN cond THEN result … [ELSE result] END`.
#[derive(Clone, Debug)]
pub struct CaseExpr {
    /// Simple-CASE operand (`CASE x WHEN 1 THEN …`); None for searched CASE.
    pub operand: Option<Box<ExprNode>>,
    pub when_then: Vec<(ExprNode, ExprNode)>,
    pub else_expr: Option<Box<ExprNode>>,
}

impl CaseExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("CASE");
        if let Some(operand) = &self.operand {
            ctx.write_str(" ");
            operand.restore(ctx)?;
        }
        for (when, then) in &self.when_then {
            ctx.write_str(" WHEN ");
            when.restore(ctx)?;
            ctx.write_str(" THEN ");
            then.restore(ctx)?;
        }
        if let Some(else_expr) = &self.else_expr {
            ctx.write_str(" ELSE ");
            else_expr.restore(ctx)?;
        }
        ctx.write_str(" END");
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct PatternLikeExpr {
    pub expr: Box<ExprNode>,
    pub not: bool,
    pub pattern: Box<ExprNode>,
    pub ci: bool,
}

impl PatternLikeExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        self.expr.restore(ctx)?;
        if self.not {
            ctx.write_str(" NOT LIKE ");
        } else {
            ctx.write_str(" LIKE ");
        }
        self.pattern.restore(ctx)
    }
}

#[derive(Clone, Debug)]
pub struct IsNullExpr {
    pub expr: Box<ExprNode>,
    pub not: bool,
}

impl IsNullExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        self.expr.restore(ctx)?;
        if self.not {
            ctx.write_str(" IS NOT NULL");
        } else {
            ctx.write_str(" IS NULL");
        }
        Ok(())
    }
}

/// `left <op> ANY|ALL (query)`. `all` is true for ALL, false for ANY/SOME.
#[derive(Clone, Debug)]
pub struct QuantifiedCmpExpr {
    pub expr: Box<ExprNode>,
    pub op: Op,
    pub all: bool,
    pub query: Box<ResultSetNode>,
}

impl QuantifiedCmpExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        self.expr.restore(ctx)?;
        ctx.write_str(" ");
        ctx.write_str(self.op.restore());
        if self.all {
            ctx.write_str(" ALL (");
        } else {
            ctx.write_str(" ANY (");
        }
        self.query.restore(ctx)?;
        ctx.write_str(")");
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ScalarSubqueryExpr {
    pub query: Box<ResultSetNode>,
}

impl ScalarSubqueryExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("(");
        self.query.restore(ctx)?;
        ctx.write_str(")");
        Ok(())
    }
}

/// `ROWS` / `RANGE` / `GROUPS` frame unit (TiDB `FrameType`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowFrameUnits {
    Rows,
    Range,
    Groups,
}

impl WindowFrameUnits {
    pub fn restore(self) -> &'static str {
        match self {
            WindowFrameUnits::Rows => "ROWS",
            WindowFrameUnits::Range => "RANGE",
            WindowFrameUnits::Groups => "GROUPS",
        }
    }
}

/// One side of `BETWEEN start AND end`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WindowFrameBound {
    UnboundedPreceding,
    Preceding(u64),
    CurrentRow,
    Following(u64),
    UnboundedFollowing,
    /// `INTERVAL '2:30' MINUTE_SECOND PRECEDING` (TiDB window RANGE).
    IntervalPreceding {
        value: String,
        unit: String,
    },
    IntervalFollowing {
        value: String,
        unit: String,
    },
}

impl WindowFrameBound {
    pub fn restore(&self, ctx: &mut RestoreCtx) {
        match self {
            WindowFrameBound::UnboundedPreceding => ctx.write_str("UNBOUNDED PRECEDING"),
            WindowFrameBound::Preceding(n) => {
                ctx.write_str(&n.to_string());
                ctx.write_str(" PRECEDING");
            }
            WindowFrameBound::CurrentRow => ctx.write_str("CURRENT ROW"),
            WindowFrameBound::Following(n) => {
                ctx.write_str(&n.to_string());
                ctx.write_str(" FOLLOWING");
            }
            WindowFrameBound::UnboundedFollowing => ctx.write_str("UNBOUNDED FOLLOWING"),
            WindowFrameBound::IntervalPreceding { value, unit } => {
                ctx.write_str("INTERVAL ");
                ctx.write_str(value);
                ctx.write_str(" ");
                ctx.write_str(unit);
                ctx.write_str(" PRECEDING");
            }
            WindowFrameBound::IntervalFollowing { value, unit } => {
                ctx.write_str("INTERVAL ");
                ctx.write_str(value);
                ctx.write_str(" ");
                ctx.write_str(unit);
                ctx.write_str(" FOLLOWING");
            }
        }
    }
}

/// Explicit `OVER (... ROWS|RANGE|GROUPS BETWEEN ... AND ...)`.
#[derive(Clone, Debug)]
pub struct WindowFrameSpec {
    pub units: WindowFrameUnits,
    pub start: WindowFrameBound,
    pub end: WindowFrameBound,
}

impl WindowFrameSpec {
    pub fn restore(&self, ctx: &mut RestoreCtx) {
        ctx.write_str(self.units.restore());
        ctx.write_str(" BETWEEN ");
        self.start.restore(ctx);
        ctx.write_str(" AND ");
        self.end.restore(ctx);
    }
}

#[derive(Clone, Debug, Default)]
pub struct WindowSpec {
    pub partition_by: Vec<ExprNode>,
    pub order_by: Vec<ByItem>,
    pub frame: Option<WindowFrameSpec>,
}

impl WindowSpec {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        let mut first = true;
        if !self.partition_by.is_empty() {
            ctx.write_str("PARTITION BY ");
            for (i, expr) in self.partition_by.iter().enumerate() {
                if i > 0 {
                    ctx.write_str(",");
                }
                expr.restore(ctx)?;
            }
            first = false;
        }
        if !self.order_by.is_empty() {
            if !first {
                ctx.write_str(" ");
            }
            ctx.write_str("ORDER BY ");
            for (i, item) in self.order_by.iter().enumerate() {
                if i > 0 {
                    ctx.write_str(",");
                }
                item.expr.restore(ctx)?;
                if item.desc {
                    ctx.write_str(" DESC");
                }
            }
            first = false;
        }
        if let Some(frame) = &self.frame {
            if !first {
                ctx.write_str(" ");
            }
            frame.restore(ctx);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct WindowFuncExpr {
    pub name: String,
    pub args: Vec<ExprNode>,
    pub spec: WindowSpec,
    /// `OVER w` / `OVER (w ...)` until the SELECT `WINDOW` clause is applied.
    pub window_ref: Option<String>,
    /// `IGNORE NULLS` (RESPECT NULLS is the default `false`).
    pub ignore_nulls: bool,
}

impl WindowFuncExpr {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str(&self.name);
        ctx.write_str("(");
        if self.name.eq_ignore_ascii_case("COUNT") && self.args.is_empty() {
            ctx.write_str("*");
        } else {
            for (i, arg) in self.args.iter().enumerate() {
                if i > 0 {
                    ctx.write_str(", ");
                }
                arg.restore(ctx)?;
            }
        }
        ctx.write_str(")");
        if self.ignore_nulls {
            ctx.write_str(" IGNORE NULLS");
        }
        ctx.write_str(" OVER (");
        self.spec.restore(ctx)?;
        ctx.write_str(")");
        Ok(())
    }
}
