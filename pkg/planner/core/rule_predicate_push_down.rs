/// predicate pushdown (TiDB `rule_predicate_push_down.go`).
/// CNF conjuncts without subqueries attach to TableReader / IndexLookUp /
/// IndexReader (coprocessor-filter analog). Join-local predicates push to each side;
/// outer-join inner-side filters stay residual. Completeness wave: TiKV
/// Coprocessor DAG / CopClient.
use super::physical_plans::{JoinType, PhysicalPlan};
use sealdb_infoschema::InfoSchema;
use sealdb_parser::ast::ExprNode;
use sealdb_types::Datum;

pub fn predicate_push_down(
    plan: PhysicalPlan,
    filter: &ExprNode,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    push_into(plan, filter.split_cnf(), is, current_db)
}

pub fn eq_column_consts(selection: Option<&ExprNode>) -> Vec<(String, Datum)> {
    selection
        .map(|expr| {
            expr.split_cnf()
                .into_iter()
                .filter_map(|item| item.eq_column_const())
                .collect()
        })
        .unwrap_or_default()
}

fn push_into(
    plan: PhysicalPlan,
    conjuncts: Vec<ExprNode>,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    if conjuncts.is_empty() {
        return plan;
    }
    match plan {
        PhysicalPlan::TableReader {
            table,
            partitions,
            partition_names,
            partition_access,
            row_count,
            stats_pseudo,
            filter_est,
            range_scan,
            pushed,
            ..
        } => {
            let (pushable, residual): (Vec<_>, Vec<_>) =
                conjuncts.into_iter().partition(|expr| !expr.has_subquery());
            // `pk = const` → PointGet (TiDB cte HASH_JOIN on PK CTE).
            if residual.is_empty()
                && pushed.is_none()
                && let Some(handle) = pk_eq_handle(is, current_db, &table, &pushable)
            {
                return PhysicalPlan::PointGet {
                    table,
                    handle: Some(handle),
                    index: None,
                    index_columns: Vec::new(),
                    partition_names,
                };
            }
            wrap_residual(
                PhysicalPlan::TableReader {
                    table,
                    partitions,
                    partition_names,
                    partition_access,
                    row_count,
                    stats_pseudo,
                    filter_est,
                    // Newer conjuncts first (TiDB: pushed ON/like before view WHERE).
                    range_scan,
                    pushed: and_opt(ExprNode::compose_cnf(pushable), pushed),
                },
                residual,
            )
        }
        PhysicalPlan::PartitionUnion { children } => PhysicalPlan::PartitionUnion {
            children: children
                .into_iter()
                .map(|c| push_into(c, conjuncts.clone(), is, current_db))
                .collect(),
        },
        PhysicalPlan::IndexLookUpReader {
            table,
            index,
            index_columns,
            partitions,
            partition_access,
            row_count,
            selection_info,
            probe_selection_info,
            pushed,
        } => {
            let (pushable, residual): (Vec<_>, Vec<_>) =
                conjuncts.into_iter().partition(|expr| !expr.has_subquery());
            wrap_residual(
                PhysicalPlan::IndexLookUpReader {
                    table,
                    index,
                    index_columns,
                    partitions,
                    partition_access,
                    row_count,
                    selection_info,
                    probe_selection_info,
                    pushed: and_opt(pushed, ExprNode::compose_cnf(pushable)),
                },
                residual,
            )
        }
        PhysicalPlan::IndexReader {
            table,
            index,
            index_columns,
            partitions,
            partition_access,
            row_count,
            pushed,
        } => {
            let (pushable, residual): (Vec<_>, Vec<_>) =
                conjuncts.into_iter().partition(|expr| !expr.has_subquery());
            wrap_residual(
                PhysicalPlan::IndexReader {
                    table,
                    index,
                    index_columns,
                    partitions,
                    partition_access,
                    row_count,
                    pushed: and_opt(pushed, ExprNode::compose_cnf(pushable)),
                },
                residual,
            )
        }
        PhysicalPlan::IndexMerge {
            table,
            indexes,
            partitions,
            pushed,
        } => {
            let (pushable, residual): (Vec<_>, Vec<_>) =
                conjuncts.into_iter().partition(|expr| !expr.has_subquery());
            wrap_residual(
                PhysicalPlan::IndexMerge {
                    table,
                    indexes,
                    partitions,
                    pushed: and_opt(pushed, ExprNode::compose_cnf(pushable)),
                },
                residual,
            )
        }
        PhysicalPlan::PointGet {
            table,
            handle,
            index,
            index_columns,
            partition_names,
        } => {
            let residual = conjuncts
                .into_iter()
                .filter(|expr| {
                    // The unique-index equality is already the point get lookup.
                    if index.is_some()
                        && let Some((col, _)) = expr.eq_column_const()
                        && index_columns
                            .iter()
                            .any(|c| c.eq_ignore_ascii_case(&col))
                    {
                        return false;
                    }
                    handle.is_none_or(|value| !is_handle_eq(expr, value))
                })
                .collect();
            wrap_residual(
                PhysicalPlan::PointGet {
                    table,
                    handle,
                    index,
                    index_columns,
                    partition_names,
                },
                residual,
            )
        }
        PhysicalPlan::BatchPointGet {
            table,
            column,
            index,
            n_keys,
            index_columns,
        } => {
            let residual = conjuncts
                .into_iter()
                .filter(|expr| !is_in_list_on(expr, &column))
                .collect();
            wrap_residual(
                PhysicalPlan::BatchPointGet {
                    table,
                    column,
                    index,
                    n_keys,
                    index_columns,
                },
                residual,
            )
        }
        PhysicalPlan::Projection { exprs, child } => PhysicalPlan::Projection {
            exprs,
            child: Box::new(push_into(*child, conjuncts, is, current_db)),
        },
        PhysicalPlan::Distinct {
            keys,
            fields,
            names,
            child,
        } => PhysicalPlan::Distinct {
            keys,
            fields,
            names,
            child: Box::new(push_into(*child, conjuncts, is, current_db)),
        },
        PhysicalPlan::HashJoin {
            tp,
            left,
            right,
            left_table,
            right_table,
            on,
        } => {
            let left_cols = table_columns(is, current_db, &left_table);
            let right_cols = table_columns(is, current_db, &right_table);
            let mut left_preds = Vec::new();
            let mut right_preds = Vec::new();
            let mut residual = Vec::new();
            for expr in conjuncts {
                match classify(
                    &expr,
                    &left_table,
                    &right_table,
                    &left_cols,
                    &right_cols,
                    tp,
                ) {
                    Side::Left => left_preds.push(expr),
                    Side::Right => right_preds.push(expr),
                    Side::Residual => residual.push(expr),
                }
            }
            // Comma joins (`FROM a, b WHERE a.x = b.x`) plan as CARTESIAN until
            // the WHERE equality becomes the join key. Split remaining
            // conjuncts by the tables owned by each subtree: cross-side column
            // equalities attach to this join (`inner join, equal:[…]`), and
            // single-side conjuncts descend to where they belong.
            let mut tp = tp;
            let mut on = on;
            if matches!(tp, JoinType::Inner | JoinType::Cross) {
                let mut left_tables = std::collections::HashSet::new();
                left_tables.insert(left_table.to_ascii_lowercase());
                collect_join_tables(&left, &mut left_tables);
                let mut right_tables = std::collections::HashSet::new();
                right_tables.insert(right_table.to_ascii_lowercase());
                collect_join_tables(&right, &mut right_tables);
                let left_map = column_owners(&left_tables, is, current_db);
                let right_map = column_owners(&right_tables, is, current_db);
                let mut keep = Vec::new();
                // HashJoin derives its hash key from the first equality in
                // `ON`; further equalities stay as a filter above the join so
                // they are still enforced (inner joins are filter-equivalent).
                let mut key_taken = !is_true_expr(&on);
                for expr in residual {
                    match resolve_conjunct_side(&expr, &left_map, &right_map) {
                        Some(Side::Left) => left_preds.push(expr),
                        Some(Side::Right) => right_preds.push(expr),
                        Some(Side::Residual) | None => {
                            if let Some(key) = qualify_cross_equality(&expr, &left_map, &right_map)
                            {
                                if key_taken {
                                    keep.push(key);
                                } else {
                                    // CROSS JOIN's placeholder `ON TRUE` gives
                                    // way to the equality key.
                                    on = key;
                                    tp = JoinType::Inner;
                                    key_taken = true;
                                }
                            } else {
                                keep.push(expr);
                            }
                        }
                    }
                }
                // TiDB `pushDownPredicatesForJoin`: an inner-join equality key
                // makes single-side predicates derivable on the other side
                // (`t1.c1 > k` ⇒ `t2.c1 > k` when `t1.c1 = t2.c1`).
                if matches!(tp, JoinType::Inner) && !is_true_expr(&on) {
                    let (dl, dr) = derive_equal_cond_preds(
                        &on,
                        &left_map,
                        &right_map,
                        &left_table,
                        &right_table,
                        is_single_table_subtree(&left),
                        is_single_table_subtree(&right),
                        &left_preds,
                        &right_preds,
                    );
                    left_preds.extend(dl);
                    right_preds.extend(dr);
                }
                residual = keep;
            }
            wrap_residual(
                PhysicalPlan::HashJoin {
                    tp,
                    left: Box::new(push_into(*left, left_preds, is, current_db)),
                    right: Box::new(push_into(*right, right_preds, is, current_db)),
                    left_table,
                    right_table,
                    on,
                },
                residual,
            )
        }
        PhysicalPlan::CTE {
            name,
            alias,
            columns,
            seed,
            recursive_plan,
            inline,
            recursive,
            limit,
            offset,
        } if inline => {
            // Empty columns: seed names match CTE outputs — push through.
            // Named outputs may differ from seed; keep Selection above unless
            // caller rewrote the filter onto seed columns (HASH_JOIN path).
            if columns.is_empty() {
                PhysicalPlan::CTE {
                    name,
                    alias,
                    columns,
                    seed: Box::new(push_into(*seed, conjuncts, is, current_db)),
                    recursive_plan,
                    inline,
                    recursive,
                    limit,
                    offset,
                }
            } else {
                wrap_residual(
                    PhysicalPlan::CTE {
                        name,
                        alias,
                        columns,
                        seed,
                        recursive_plan,
                        inline,
                        recursive,
                        limit,
                        offset,
                    },
                    conjuncts,
                )
            }
        }
        other => wrap_residual(other, conjuncts),
    }
}

fn pk_eq_handle(
    is: Option<&InfoSchema>,
    current_db: &str,
    table: &str,
    conjuncts: &[ExprNode],
) -> Option<i64> {
    if conjuncts.len() != 1 {
        return None;
    }
    let (col, value) = conjuncts[0].eq_column_const()?;
    let info = is?.table_by_name(current_db, table)?;
    if !info.pk_is_handle {
        return None;
    }
    let handle = info.handle_column()?;
    if !handle.name.o().eq_ignore_ascii_case(&col) {
        return None;
    }
    match value {
        Datum::I64(v) => Some(v),
        Datum::U64(v) => Some(v as i64),
        Datum::Bytes(b) => std::str::from_utf8(&b).ok()?.trim().parse().ok(),
        _ => None,
    }
}

fn wrap_residual(plan: PhysicalPlan, residual: Vec<ExprNode>) -> PhysicalPlan {
    let Some(filter) = ExprNode::compose_cnf(residual) else {
        return plan;
    };
    if filter.has_subquery() {
        PhysicalPlan::Apply {
            child: Box::new(plan),
            filter,
            probe: None,
            info: None,
        }
    } else {
        PhysicalPlan::Selection {
            child: Box::new(plan),
            filter,
        }
    }
}

fn and_opt(left: Option<ExprNode>, right: Option<ExprNode>) -> Option<ExprNode> {
    match (left, right) {
        (None, other) | (other, None) => other,
        (Some(left), Some(right)) => ExprNode::compose_cnf(vec![left, right]),
    }
}

fn is_handle_eq(expr: &ExprNode, handle: i64) -> bool {
    expr.eq_column_const()
        .and_then(|(_, value)| datum_as_i64(&value))
        .is_some_and(|v| v == handle)
}

fn datum_as_i64(value: &Datum) -> Option<i64> {
    match value {
        Datum::I64(v) => Some(*v),
        Datum::U64(v) => Some(*v as i64),
        Datum::F64(v) if v.fract() == 0.0 && *v >= i64::MIN as f64 && *v <= i64::MAX as f64 => {
            Some(*v as i64)
        }
        Datum::Bytes(b) => std::str::from_utf8(b).ok()?.trim().parse().ok(),
        _ => None,
    }
}

fn is_in_list_on(expr: &ExprNode, column: &str) -> bool {
    expr.column_in_consts()
        .is_some_and(|(name, _)| name.eq_ignore_ascii_case(column))
}

enum Side {
    Left,
    Right,
    Residual,
}

fn classify(
    expr: &ExprNode,
    left_table: &str,
    right_table: &str,
    left_cols: &[String],
    right_cols: &[String],
    tp: JoinType,
) -> Side {
    if expr.has_subquery() {
        return Side::Residual;
    }
    let cols = expr.referenced_columns();
    if cols.is_empty() {
        return Side::Residual;
    }
    let mut left = true;
    let mut right = true;
    for (table, name) in cols {
        let on_left = column_on(table.as_deref(), &name, left_table, left_cols);
        let on_right = column_on(table.as_deref(), &name, right_table, right_cols);
        if !on_left {
            left = false;
        }
        if !on_right {
            right = false;
        }
        if !on_left && !on_right {
            return Side::Residual;
        }
    }
    match (left, right, tp) {
        (true, false, _) => Side::Left,
        (false, true, JoinType::Inner | JoinType::Cross | JoinType::Semi | JoinType::Anti) => {
            Side::Right
        }
        (false, true, JoinType::RightOuter) => Side::Right,
        (false, true, JoinType::LeftOuter) => Side::Residual,
        (true, true, _) => Side::Residual,
        (false, false, _) => Side::Residual,
    }
}

fn column_on(table: Option<&str>, name: &str, owner: &str, cols: &[String]) -> bool {
    if let Some(table) = table {
        return table.eq_ignore_ascii_case(owner);
    }
    if cols.is_empty() {
        return false;
    }
    cols.iter().any(|col| col.eq_ignore_ascii_case(name))
}

fn table_columns(is: Option<&InfoSchema>, current_db: &str, table: &str) -> Vec<String> {
    let Some(is) = is else {
        return Vec::new();
    };
    is.table_by_name(current_db, table)
        .or_else(|| {
            is.all_schemas()
                .into_iter()
                .find_map(|db| is.table_by_name(db.name.o(), table))
        })
        .map(|info| {
            info.columns
                .iter()
                .map(|col| col.name.o().to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sealdb_parser::ast::expressions::{BinaryOpExpr, ColumnNameExpr, ValueExpr};
    use sealdb_parser::opcode::Op;

    fn col_eq(table: &str, name: &str, value: i64) -> ExprNode {
        ExprNode::Binary(BinaryOpExpr {
            op: Op::Eq,
            left: Box::new(ExprNode::Column(ColumnNameExpr {
                schema: None,
                table: Some(table.to_string()),
                name: name.to_string(),
            })),
            right: Box::new(ExprNode::Value(ValueExpr {
                datum: Datum::I64(value),
            })),
            single_cnf_item: false,
        })
    }

    #[test]
    fn test_push_into_table_reader() {
        let plan = predicate_push_down(
            PhysicalPlan::table_reader("t", Vec::new()),
            &col_eq("t", "id", 1),
            None,
            "db1",
        );
        match plan {
            PhysicalPlan::TableReader {
                pushed: Some(filter),
                ..
            } => assert!(filter.restore_string().contains("id")),
            other => panic!("expected pushed TableReader, got {other:?}"),
        }
    }

    #[test]
    fn test_inner_join_splits_predicates() {
        let join = PhysicalPlan::HashJoin {
            tp: JoinType::Inner,
            left: Box::new(PhysicalPlan::table_reader("t1", Vec::new())),
            right: Box::new(PhysicalPlan::table_reader("t2", Vec::new())),
            left_table: "t1".to_string(),
            right_table: "t2".to_string(),
            on: col_eq("t1", "id", 1),
        };
        let filter =
            ExprNode::compose_cnf(vec![col_eq("t1", "id", 1), col_eq("t2", "id", 2)]).expect("and");
        let plan = predicate_push_down(join, &filter, None, "db1");
        let PhysicalPlan::HashJoin { left, right, .. } = plan else {
            panic!("expected hash join, got {plan:?}");
        };
        assert!(matches!(
            *left,
            PhysicalPlan::TableReader {
                pushed: Some(_),
                ..
            }
        ));
        assert!(matches!(
            *right,
            PhysicalPlan::TableReader {
                pushed: Some(_),
                ..
            }
        ));
    }
}

/// Lower-cased table names owned by a join subtree (HashJoin/MergeJoin carry
/// both sides' table names at every level).
fn collect_join_tables(plan: &PhysicalPlan, out: &mut std::collections::HashSet<String>) {
    match plan {
        PhysicalPlan::HashJoin {
            left,
            right,
            left_table,
            right_table,
            ..
        } => {
            out.insert(left_table.to_ascii_lowercase());
            out.insert(right_table.to_ascii_lowercase());
            collect_join_tables(left, out);
            collect_join_tables(right, out);
        }
        PhysicalPlan::MergeJoin {
            left,
            right,
            left_table,
            right_table,
            ..
        } => {
            out.insert(left_table.to_ascii_lowercase());
            out.insert(right_table.to_ascii_lowercase());
            collect_join_tables(left, out);
            collect_join_tables(right, out);
        }
        _ => {}
    }
}

/// A side that holds no nested join is a single base table (or a leaf such as
/// a CTE/derived table): every column of its ON operand belongs to it.
fn is_single_table_subtree(plan: &PhysicalPlan) -> bool {
    match plan {
        PhysicalPlan::HashJoin { .. } | PhysicalPlan::MergeJoin { .. } => false,
        PhysicalPlan::Projection { child, .. } | PhysicalPlan::Distinct { child, .. } => {
            is_single_table_subtree(child)
        }
        _ => true,
    }
}

fn is_true_expr(expr: &ExprNode) -> bool {
    matches!(expr, ExprNode::Value(value) if value.datum == Datum::I64(1))
}

/// Column name → owning tables for every table joined in a subtree.
fn column_owners(
    tables: &std::collections::HashSet<String>,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> std::collections::HashMap<String, Vec<String>> {
    let mut map: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for table in tables {
        for col in table_columns(is, current_db, table) {
            map.entry(col.to_ascii_lowercase())
                .or_default()
                .push(table.clone());
        }
    }
    map
}

fn owner_of(
    table: Option<&str>,
    col: &str,
    map: &std::collections::HashMap<String, Vec<String>>,
) -> Option<String> {
    if let Some(table) = table {
        return Some(table.to_ascii_lowercase());
    }
    let owners = map.get(&col.to_ascii_lowercase())?;
    (owners.len() == 1).then(|| owners[0].clone())
}

/// Which join side owns every column of `expr`; `None` when a column belongs
/// to both sides / is unknown (so the conjunct stays residual or becomes a key).
fn resolve_conjunct_side(
    expr: &ExprNode,
    left_map: &std::collections::HashMap<String, Vec<String>>,
    right_map: &std::collections::HashMap<String, Vec<String>>,
) -> Option<Side> {
    let cols = expr.referenced_columns();
    if cols.is_empty() {
        return None;
    }
    let mut left = true;
    let mut right = true;
    for (table, name) in cols {
        let on_left = owner_of(table.as_deref(), &name, left_map).is_some();
        let on_right = owner_of(table.as_deref(), &name, right_map).is_some();
        if !on_left {
            left = false;
        }
        if !on_right {
            right = false;
        }
        if !on_left && !on_right {
            return None;
        }
    }
    match (left, right) {
        (true, false) => Some(Side::Left),
        (false, true) => Some(Side::Right),
        _ => None,
    }
}

/// `col = col` with one column on each side, rewritten to qualified column
/// references so the executor can build the hash key.
fn qualify_cross_equality(
    expr: &ExprNode,
    left_map: &std::collections::HashMap<String, Vec<String>>,
    right_map: &std::collections::HashMap<String, Vec<String>>,
) -> Option<ExprNode> {
    let ExprNode::Binary(binary) = expr else {
        return None;
    };
    if binary.op != sealdb_parser::opcode::Op::Eq {
        return None;
    }
    let (ExprNode::Column(l), ExprNode::Column(r)) = (binary.left.as_ref(), binary.right.as_ref())
    else {
        return None;
    };
    let (l_left, l_right) = (
        owner_of(l.table.as_deref(), &l.name, left_map),
        owner_of(l.table.as_deref(), &l.name, right_map),
    );
    let (r_left, r_right) = (
        owner_of(r.table.as_deref(), &r.name, left_map),
        owner_of(r.table.as_deref(), &r.name, right_map),
    );
    // Each column must be owned by exactly one side, and the two columns must
    // sit on opposite sides for this to be a usable hash key.
    let (l_owner, r_owner) = match (l_left, l_right, r_left, r_right) {
        (Some(l), None, None, Some(r)) => (Some(l), Some(r)),
        (None, Some(l), Some(r), None) => (Some(l), Some(r)),
        _ => return None,
    };
    let mut new = binary.clone();
    let mut left_col = l.clone();
    left_col.table = l_owner;
    let mut right_col = r.clone();
    right_col.table = r_owner;
    new.left = Box::new(ExprNode::Column(left_col));
    new.right = Box::new(ExprNode::Column(right_col));
    Some(ExprNode::Binary(new))
}


/// TiDB derived predicates across an inner-join equality key.
///
/// For each `a = b` in `ON` every single-side conjunct on `a` is mirrored onto
/// `b` (and vice versa), which is why unanalyzed TiDB plans show the same
/// filter on both sides of a join. The mirrored column is bound to the alias
/// that owns it inside the target subtree.
fn derive_equal_cond_preds(
    on: &ExprNode,
    left_map: &std::collections::HashMap<String, Vec<String>>,
    right_map: &std::collections::HashMap<String, Vec<String>>,
    left_table: &str,
    right_table: &str,
    left_single: bool,
    right_single: bool,
    left_preds: &[ExprNode],
    right_preds: &[ExprNode],
) -> (Vec<ExprNode>, Vec<ExprNode>) {
    use sealdb_parser::ast::expressions::{
        BinaryOpExpr, ColumnNameExpr, ExprNode, UnaryOpExpr,
    };

    fn single_col(e: &ExprNode) -> Option<ColumnNameExpr> {
        match e {
            ExprNode::Column(c) => Some(c.clone()),
            ExprNode::Binary(b) => match (b.left.as_ref(), b.right.as_ref()) {
                (ExprNode::Column(c), other) | (other, ExprNode::Column(c))
                    if other.referenced_columns().is_empty() =>
                {
                    Some(c.clone())
                }
                _ => None,
            },
            ExprNode::Unary(u) => single_col(&u.expr),
            _ => None,
        }
    }
    /// An alias-loaded subtree may be absent from the infoschema (the alias
    /// itself is not a table name); an empty map then means "unknown", not
    /// "not owned".
    fn owns(
        map: &std::collections::HashMap<String, Vec<String>>,
        name: &str,
        single: bool,
    ) -> bool {
        single || map.is_empty() || map.contains_key(&name.to_ascii_lowercase())
    }
    /// Alias that owns `name` in the given subtree (`None` → cannot derive).
    /// Falls back to the join side's own alias when columns are unresolvable.
    fn alias_for(
        map: &std::collections::HashMap<String, Vec<String>>,
        name: &str,
        fallback: &str,
        single: bool,
    ) -> Option<String> {
        if single {
            return Some(fallback.to_string());
        }
        match map.get(&name.to_ascii_lowercase()) {
            Some(owners) if owners.len() == 1 => Some(owners[0].clone()),
            Some(_) => None,
            None if map.is_empty() => Some(fallback.to_string()),
            None => None,
        }
    }
    fn replace(e: &ExprNode, from: &str, to: &str, alias: &str) -> Option<ExprNode> {
        if e.referenced_columns().is_empty() {
            return Some(e.clone());
        }
        match e {
            ExprNode::Column(c) if c.name.eq_ignore_ascii_case(from) => {
                Some(ExprNode::Column(ColumnNameExpr {
                    schema: None,
                    table: Some(alias.to_string()),
                    name: to.to_string(),
                }))
            }
            ExprNode::Binary(b) => Some(ExprNode::Binary(BinaryOpExpr {
                op: b.op,
                left: Box::new(replace(&b.left, from, to, alias)?),
                right: Box::new(replace(&b.right, from, to, alias)?),
                single_cnf_item: false,
            })),
            ExprNode::Unary(u) => Some(ExprNode::Unary(UnaryOpExpr {
                op: u.op,
                expr: Box::new(replace(&u.expr, from, to, alias)?),
            })),
            ExprNode::IsNull(n) => Some(ExprNode::IsNull(
                sealdb_parser::ast::expressions::IsNullExpr {
                    expr: Box::new(replace(&n.expr, from, to, alias)?),
                    not: n.not,
                },
            )),
            _ => None,
        }
    }

    let mut dl = Vec::new();
    let mut dr = Vec::new();
    for item in on.split_cnf() {
        let ExprNode::Binary(bin) = &item else {
            continue;
        };
        if bin.op != sealdb_parser::opcode::Op::Eq {
            continue;
        }
        let (ExprNode::Column(a), ExprNode::Column(b)) =
            (bin.left.as_ref(), bin.right.as_ref())
        else {
            continue;
        };
        for p in left_preds {
            let Some(c) = single_col(p) else { continue };
            if !owns(left_map, &c.name, left_single) {
                continue;
            }
            let (from, to) = if c.name.eq_ignore_ascii_case(&a.name)
                && !c.name.eq_ignore_ascii_case(&b.name)
            {
                (a.name.clone(), b.name.clone())
            } else if c.name.eq_ignore_ascii_case(&b.name) {
                (b.name.clone(), a.name.clone())
            } else {
                continue;
            };
            let Some(alias) = alias_for(right_map, &to, right_table, right_single) else {
                continue;
            };
            if let Some(d) = replace(p, &from, &to, &alias) {
                dr.push(d);
            }
        }
        for p in right_preds {
            let Some(c) = single_col(p) else { continue };
            if !owns(right_map, &c.name, right_single) {
                continue;
            }
            let (from, to) = if c.name.eq_ignore_ascii_case(&b.name)
                && !c.name.eq_ignore_ascii_case(&a.name)
            {
                (b.name.clone(), a.name.clone())
            } else if c.name.eq_ignore_ascii_case(&a.name) {
                (a.name.clone(), b.name.clone())
            } else {
                continue;
            };
            let Some(alias) = alias_for(left_map, &to, left_table, left_single) else {
                continue;
            };
            if let Some(d) = replace(p, &from, &to, &alias) {
                dl.push(d);
            }
        }
    }
    (dl, dr)
}
