// single-table DELETE (ORDER BY / LIMIT) and two-table DELETE ... JOIN.
// pessimistic DELETE reads latest and takes exclusive locks.
// Completeness wave / not this feature: n-table DELETE, comma FROM without USING,
// RETURNING (not in TiDB v8.5.8 / MySQL 8.0), MySQL-protocol affected-row counts.
use crate::cte::pre_evaluate_dml_predicate;
use crate::index::delete_index_keys;
use crate::internal::exec::Executor;
use crate::join::hash_join_v1::hash_join_pairs;
use crate::sortexec::sort::sort_rows_by;
use crate::table_reader::{ScannedRow, scan_table};
use crate::update::{
    col_names, dml_row_lock, dml_side_rowid, joined_schema_with_rowid, joined_values_with_rowid,
    load_table, scan_dml_side, table_matches,
};
use sealdb_domain::Domain;
use sealdb_errno::{ERR_ROW_IS_REFERENCED_2, ERR_UNKNOWN_TABLE, Error, Result};
use sealdb_expression::eval_filter;
use sealdb_kv::{LockWait, Transaction, TxnHandle};
use sealdb_meta::TableInfo;
use sealdb_parser::ast::Join;
use sealdb_parser::ast::expressions::{
    BetweenExpr, BinaryOpExpr, CaseExpr, ExistsExpr, FuncCallExpr, FuncCastExpr, InListExpr,
    InSubqueryExpr, IsNullExpr, PatternLikeExpr, QuantifiedCmpExpr, RowExpr, ScalarSubqueryExpr,
    UnaryOpExpr, ValueExpr,
};
use sealdb_parser::ast::{DeleteStmt, ExprNode, ResultSetNode, SelectStmt};
use sealdb_tablecodec::encode_row_key_with_handle;
use sealdb_types::Datum;
use sealdb_util::chunk::Chunk;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// `DELETE` executor: single-table path, or two-table JOIN deleting listed tables.
pub struct DeleteExec {
    domain: Arc<Domain>,
    stmt: DeleteStmt,
    current_db: String,
    txn: Arc<TxnHandle>,
    done: bool,
}

impl DeleteExec {
    pub fn new(
        domain: Arc<Domain>,
        stmt: DeleteStmt,
        current_db: impl Into<String>,
        txn: Arc<TxnHandle>,
    ) -> Self {
        Self {
            domain,
            stmt,
            current_db: current_db.into(),
            txn,
            done: false,
        }
    }
}

impl Executor for DeleteExec {
    fn open(&mut self) -> Result<()> {
        if let Some(join) = self.stmt.join.clone() {
            if !self.stmt.additional_joins.is_empty() {
                return self.delete_join_chain(&join);
            }
            return self.delete_join(&join);
        }
        if let Some(missing) = self
            .stmt
            .tables
            .iter()
            .find(|t| !table_matches(t, &self.stmt.table, &self.current_db))
        {
            return Err(Error::new(
                ERR_UNKNOWN_TABLE,
                format!("Unknown table '{}' in MULTI DELETE", missing.name),
            ));
        }
        let info = load_table(&self.domain, &self.current_db, &self.stmt.table)?;
        let names = col_names(&info);
        // Heap / common-handle rows expose their handle as `_tidb_rowid` to the
        // execute-time filter.
        let rowid_eval = !info.pk_is_handle;
        let mut eval_names = names.clone();
        if rowid_eval {
            eval_names.push("_tidb_rowid".to_string());
        }
        let lock = dml_row_lock(&self.txn)?;
        let correlated = self
            .stmt
            .selection
            .as_ref()
            .is_some_and(ExprNode::has_subquery);
        let filter = match &self.stmt.selection {
            Some(expr) if !correlated => Some(pre_evaluate_dml_predicate(
                expr,
                &self.domain,
                &self.current_db,
                &self.txn,
            )?),
            _ => None,
        };
        // Prune on the pre-evaluated predicate: a raw scalar subquery cannot be
        // reasoned about and would prune every partition away.
        let mut physical = sealdb_planner::core::prune_partitions(&info, filter.as_ref());
        let targets =
            crate::update::resolve_partition_ids(&info, &self.stmt.table.partition_names)?;
        if !targets.is_empty() {
            physical.retain(|id| targets.contains(id));
        }
        let rows = scan_table(&self.txn, &info, lock, LockWait::Block, &physical)?;
        crate::update::enforce_truncating_comparisons(&info, filter.as_ref(), self.stmt.ignore)?;
        let mut matched = Vec::new();
        let outer_tables = [
            self.stmt.table.name.clone(),
            self.stmt.table.resolve_name().to_string(),
        ];
        for scanned in rows {
            let mut values = scanned.values.clone();
            if rowid_eval {
                values.push(Datum::I64(scanned.handle));
            }
            let row_filter = if correlated {
                self.stmt
                    .selection
                    .as_ref()
                    .map(|expr| substitute_outer_columns(expr, &outer_tables, &eval_names, &values))
            } else {
                filter.clone()
            };
            let row_filter = match row_filter {
                Some(expr) => Some(pre_evaluate_dml_predicate(
                    &expr,
                    &self.domain,
                    &self.current_db,
                    &self.txn,
                )?),
                None => None,
            };
            if let Some(filter) = &row_filter {
                crate::update::enforce_truncating_comparisons(
                    &info,
                    Some(filter),
                    self.stmt.ignore,
                )?;
                if !eval_filter(filter, &eval_names, &values)? {
                    continue;
                }
            }
            matched.push(scanned);
        }
        sort_rows_by(&mut matched, &self.stmt.order_by, &names, |row| &row.values);
        if let Some(limit) = self.stmt.limit {
            matched.truncate(limit as usize);
        }
        let conflicts = self.fk_delete_conflicts(&info, &matched)?;
        if !conflicts.is_empty() {
            if !self.stmt.ignore {
                return Err(Error::new(ERR_ROW_IS_REFERENCED_2, conflicts[0].1.clone()));
            }
            for (_, message) in &conflicts {
                sealdb_expression::note_warning(ERR_ROW_IS_REFERENCED_2, message.clone());
            }
            let blocked: HashSet<(i64, i64)> = conflicts.into_iter().map(|(key, _)| key).collect();
            matched.retain(|row| !blocked.contains(&(row.physical_id, row.handle)));
        }
        let mut affected = 0u64;
        self.txn.with_txn(|txn| {
            for scanned in matched {
                apply_row_delete(txn, &info, &scanned)?;
                affected += 1;
            }
            Ok(())
        })?;
        crate::internal::note_dml_affected(affected);
        Ok(())
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        Ok(None)
    }

    fn close(&mut self) -> Result<()> {
        Ok(())
    }

    fn schema(&self) -> &[String] {
        &[]
    }
}

impl DeleteExec {
    /// `DELETE t1 [, t2] FROM t1 JOIN t2 ...`: join, dedup handles, delete listed sides.
    fn delete_join(&mut self, join: &Join) -> Result<()> {
        let delete_left = self
            .stmt
            .tables
            .iter()
            .any(|t| table_matches(t, &join.left, &self.current_db));
        let delete_right = self
            .stmt
            .tables
            .iter()
            .any(|t| table_matches(t, &join.right, &self.current_db));
        if !delete_left && !delete_right {
            let missing = self
                .stmt
                .tables
                .first()
                .map(|t| t.name.as_str())
                .unwrap_or_default();
            return Err(Error::new(
                ERR_UNKNOWN_TABLE,
                format!("Unknown table '{missing}' in MULTI DELETE"),
            ));
        }
        let lock = dml_row_lock(&self.txn)?;
        let (left_names, left_rows, left_info) = scan_dml_side(
            &self.domain,
            &self.current_db,
            &self.txn,
            self.stmt.with.as_ref(),
            &join.left,
            lock,
        )?;
        let (right_names, right_rows, right_info) = scan_dml_side(
            &self.domain,
            &self.current_db,
            &self.txn,
            self.stmt.with.as_ref(),
            &join.right,
            lock,
        )?;
        let left_rowid = dml_side_rowid(left_info.as_ref());
        let right_rowid = dml_side_rowid(right_info.as_ref());
        let schema = joined_schema_with_rowid(
            &join.left,
            &left_names,
            left_rowid,
            &join.right,
            &right_names,
            right_rowid,
        );
        let left_values: Vec<Vec<Datum>> = left_rows.iter().map(|r| r.values.clone()).collect();
        let right_values: Vec<Vec<Datum>> = right_rows.iter().map(|r| r.values.clone()).collect();
        let pairs = hash_join_pairs(join, &left_names, &right_names, &left_values, &right_values)?;
        let mut left_del: HashSet<(i64, i64)> = HashSet::new();
        let mut right_del: HashSet<(i64, i64)> = HashSet::new();
        let mut left_scanned: HashMap<(i64, i64), ScannedRow> = HashMap::new();
        let mut right_scanned: HashMap<(i64, i64), ScannedRow> = HashMap::new();
        for (left_i, right_i) in pairs {
            let left_row = left_i.map(|i| &left_rows[i]);
            let right_row = right_i.map(|i| &right_rows[i]);
            let joined = joined_values_with_rowid(
                left_row,
                right_row,
                left_names.len(),
                right_names.len(),
                left_rowid,
                right_rowid,
            );
            if let Some(filter) = &self.stmt.selection
                && !eval_filter(filter, &schema, &joined)?
            {
                continue;
            }
            if delete_left
                && left_info.is_some()
                && let Some(scanned) = left_row
            {
                let key = (scanned.physical_id, scanned.handle);
                left_del.insert(key);
                left_scanned.entry(key).or_insert_with(|| scanned.clone());
            }
            if delete_right
                && right_info.is_some()
                && let Some(scanned) = right_row
            {
                let key = (scanned.physical_id, scanned.handle);
                right_del.insert(key);
                right_scanned.entry(key).or_insert_with(|| scanned.clone());
            }
        }
        let mut conflict_messages = Vec::new();
        if let Some(left) = &left_info {
            for (key, message) in
                self.fk_delete_conflicts(left, &left_scanned.values().cloned().collect::<Vec<_>>())?
            {
                if !self.stmt.ignore {
                    return Err(Error::new(ERR_ROW_IS_REFERENCED_2, message));
                }
                left_del.remove(&key);
                conflict_messages.push(message);
            }
        }
        if let Some(right) = &right_info {
            for (key, message) in self
                .fk_delete_conflicts(right, &right_scanned.values().cloned().collect::<Vec<_>>())?
            {
                if !self.stmt.ignore {
                    return Err(Error::new(ERR_ROW_IS_REFERENCED_2, message));
                }
                right_del.remove(&key);
                conflict_messages.push(message);
            }
        }
        conflict_messages.sort();
        for message in conflict_messages {
            sealdb_expression::note_warning(ERR_ROW_IS_REFERENCED_2, message);
        }
        let affected = self.txn.with_txn(|txn| {
            let mut affected = 0u64;
            if let Some(left) = &left_info {
                for key in left_del {
                    if let Some(scanned) = left_scanned.remove(&key) {
                        apply_row_delete(txn, left, &scanned)?;
                        affected += 1;
                    }
                }
            }
            if let Some(right) = &right_info {
                for key in right_del {
                    if let Some(scanned) = right_scanned.remove(&key) {
                        apply_row_delete(txn, right, &scanned)?;
                        affected += 1;
                    }
                }
            }
            Ok(affected)
        })?;
        crate::internal::note_dml_affected(affected);
        Ok(())
    }
}

impl DeleteExec {
    /// Three-or-more-table `DELETE t1, t2 FROM t1 JOIN t2 JOIN t3 ...`:
    /// evaluate the flattened chain and delete the listed sides.
    fn delete_join_chain(&mut self, join: &Join) -> Result<()> {
        let mut factors: Vec<Join> = vec![join.clone()];
        factors.extend(self.stmt.additional_joins.iter().cloned());
        let mut tables = Vec::new();
        let mut names = Vec::new();
        let mut rows = Vec::new();
        let mut infos = Vec::new();
        let lock = dml_row_lock(&self.txn)?;
        let mut chain_tables = vec![join.left.clone()];
        chain_tables.extend(factors.iter().map(|factor| factor.right.clone()));
        for table in chain_tables {
            let (side_names, side_rows, side_info) = scan_dml_side(
                &self.domain,
                &self.current_db,
                &self.txn,
                self.stmt.with.as_ref(),
                &table,
                lock,
            )?;
            tables.push(table);
            names.push(side_names);
            rows.push(side_rows);
            infos.push(side_info);
        }
        let rowid_sides: Vec<bool> = infos
            .iter()
            .map(|info| dml_side_rowid(info.as_ref()))
            .collect();
        let schema: Vec<String> = tables
            .iter()
            .zip(&names)
            .zip(&rowid_sides)
            .flat_map(|((t, cols), rowid)| {
                cols.iter()
                    .map(|c| format!("{}.{}", t.name, c))
                    .chain(rowid.then(|| format!("{}._tidb_rowid", t.name)))
                    .collect::<Vec<_>>()
            })
            .collect();
        let delete_side: Vec<bool> = tables
            .iter()
            .map(|t| {
                self.stmt
                    .tables
                    .iter()
                    .any(|target| table_matches(target, t, &self.current_db))
            })
            .collect();
        let mut delete_rows: Vec<HashSet<usize>> = vec![HashSet::new(); tables.len()];
        let mut indices = vec![0usize; tables.len()];
        loop {
            let mut joined = Vec::new();
            for (side, &index) in indices.iter().enumerate() {
                if let Some(row) = rows[side].get(index) {
                    joined.extend(row.values.iter().cloned());
                    if rowid_sides[side] {
                        joined.push(Datum::I64(row.handle));
                    }
                }
            }
            let mut matched = true;
            for factor in &factors {
                if !matches!(&factor.on, ExprNode::Value(v) if v.datum == Datum::I64(1))
                    && !eval_filter(&factor.on, &schema, &joined)?
                {
                    matched = false;
                    break;
                }
            }
            if matched
                && let Some(selection) = &self.stmt.selection
                && !eval_filter(selection, &schema, &joined)?
            {
                matched = false;
            }
            if matched {
                for (side, &index) in indices.iter().enumerate() {
                    if delete_side[side] {
                        delete_rows[side].insert(index);
                    }
                }
            }
            let mut carry = true;
            for (side, index) in indices.iter_mut().enumerate() {
                *index += 1;
                if *index < rows[side].len() {
                    carry = false;
                    break;
                }
                *index = 0;
            }
            if carry {
                break;
            }
        }
        let affected = self.txn.with_txn(|txn| {
            let mut affected = 0u64;
            for side in 0..tables.len() {
                if !delete_side[side] {
                    continue;
                }
                let Some(info) = &infos[side] else {
                    continue;
                };
                for &index in &delete_rows[side] {
                    apply_row_delete(txn, info, &rows[side][index])?;
                    affected += 1;
                }
            }
            Ok(affected)
        })?;
        crate::internal::note_dml_affected(affected);
        Ok(())
    }
}

impl DeleteExec {
    /// `RESTRICT` parent rows still referenced by a child FK, with the MySQL
    /// 1451 message TiDB reports. `ON DELETE CASCADE` is handled by the planner
    /// and never blocks here.
    fn fk_delete_conflicts(
        &self,
        parent: &TableInfo,
        rows: &[ScannedRow],
    ) -> Result<Vec<((i64, i64), String)>> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let is = self.domain.infoschema();
        let parent_names = col_names(parent);
        let mut conflicts = Vec::new();
        for schema in is.all_schemas() {
            let db = schema.name.o().to_string();
            for child in is.schema_tables(&db) {
                for fk in &child.foreign_keys {
                    if fk.on_delete_cascade || !fk.ref_table.eq_ignore_ascii_case(parent.name.o()) {
                        continue;
                    }
                    let ref_cols: Vec<String> = if fk.ref_cols.is_empty() {
                        parent
                            .indices
                            .iter()
                            .find(|idx| idx.primary)
                            .map(|idx| idx.columns.iter().map(|c| c.name.o().to_string()).collect())
                            .unwrap_or_default()
                    } else {
                        fk.ref_cols.clone()
                    };
                    let Some(parent_cols): Option<Vec<usize>> = ref_cols
                        .iter()
                        .map(|name| {
                            parent_names
                                .iter()
                                .position(|c| c.eq_ignore_ascii_case(name))
                        })
                        .collect()
                    else {
                        continue;
                    };
                    let child_names = col_names(&child);
                    let Some(child_cols): Option<Vec<usize>> = fk
                        .cols
                        .iter()
                        .map(|name| {
                            child_names
                                .iter()
                                .position(|c| c.eq_ignore_ascii_case(name))
                        })
                        .collect()
                    else {
                        continue;
                    };
                    let physical = sealdb_planner::core::prune_partitions(&child, None);
                    let child_rows = scan_table(
                        &self.txn,
                        &child,
                        sealdb_kv::RowLock::None,
                        LockWait::Block,
                        &physical,
                    )?;
                    for row in rows {
                        let key_values: Vec<Datum> =
                            parent_cols.iter().map(|&i| row.values[i].clone()).collect();
                        // A NULL referenced key never matches a child row.
                        if key_values.iter().any(Datum::is_null) {
                            continue;
                        }
                        let referenced = child_rows.iter().any(|child_row| {
                            child_cols.iter().zip(&key_values).all(|(&i, value)| {
                                !child_row.values[i].is_null()
                                    && (child_row.values[i] == *value
                                        || sealdb_expression::datum_sql_eq(
                                            &child_row.values[i],
                                            value,
                                        )
                                        .unwrap_or(false))
                            })
                        });
                        if referenced {
                            let columns = fk.cols.join("`, `");
                            let refs = ref_cols.join("`, `");
                            conflicts.push((
                                (row.physical_id, row.handle),
                                format!(
                                    "Cannot delete or update a parent row: a foreign key constraint fails (`{db}`.`{}`, CONSTRAINT `{}` FOREIGN KEY (`{columns}`) REFERENCES `{}` (`{refs}`))",
                                    child.name.o(),
                                    fk.name,
                                    parent.name.o()
                                ),
                            ));
                            break;
                        }
                    }
                }
            }
        }
        conflicts.sort_by_key(|(key, _)| *key);
        Ok(conflicts)
    }
}

fn qualifier_matches(qualifier: &str, table: &str) -> bool {
    qualifier.eq_ignore_ascii_case(table)
        || qualifier
            .rsplit_once('.')
            .is_some_and(|(_, name)| name.eq_ignore_ascii_case(table))
}

/// Bind correlated outer-column references to a scanned row so a per-row
/// `EXISTS` / `IN` subquery can be evaluated as an uncorrelated one.
fn substitute_outer_columns(
    expr: &ExprNode,
    tables: &[String],
    names: &[String],
    values: &[Datum],
) -> ExprNode {
    let sub = |e: &ExprNode| substitute_outer_columns(e, tables, names, values);
    match expr {
        ExprNode::Column(col) => {
            let outer = col
                .table
                .as_deref()
                .is_some_and(|q| tables.iter().any(|t| qualifier_matches(q, t)));
            if outer
                && let Some(index) = names.iter().position(|n| n.eq_ignore_ascii_case(&col.name))
            {
                return ExprNode::Value(ValueExpr {
                    datum: values[index].clone(),
                });
            }
            expr.clone()
        }
        ExprNode::Value(_) => expr.clone(),
        ExprNode::Binary(b) => ExprNode::Binary(BinaryOpExpr {
            op: b.op,
            left: Box::new(sub(&b.left)),
            right: Box::new(sub(&b.right)),
            single_cnf_item: b.single_cnf_item,
        }),
        ExprNode::Unary(u) => ExprNode::Unary(UnaryOpExpr {
            op: u.op,
            expr: Box::new(sub(&u.expr)),
        }),
        ExprNode::Func(f) => ExprNode::Func(FuncCallExpr {
            name: f.name.clone(),
            args: f.args.iter().map(sub).collect(),
            distinct: f.distinct,
        }),
        ExprNode::Cast(c) => ExprNode::Cast(FuncCastExpr {
            expr: Box::new(sub(&c.expr)),
            target: c.target.clone(),
            json: c.json,
        }),
        ExprNode::Exists(e) => ExprNode::Exists(ExistsExpr {
            not: e.not,
            query: Box::new(substitute_query(&e.query, tables, names, values)),
        }),
        ExprNode::InSubquery(i) => ExprNode::InSubquery(InSubqueryExpr {
            expr: Box::new(sub(&i.expr)),
            not: i.not,
            query: Box::new(substitute_query(&i.query, tables, names, values)),
            all_ne: i.all_ne,
        }),
        ExprNode::ScalarSubquery(s) => ExprNode::ScalarSubquery(ScalarSubqueryExpr {
            query: Box::new(substitute_query(&s.query, tables, names, values)),
        }),
        ExprNode::QuantifiedCmp(q) => ExprNode::QuantifiedCmp(QuantifiedCmpExpr {
            expr: Box::new(sub(&q.expr)),
            op: q.op,
            all: q.all,
            query: Box::new(substitute_query(&q.query, tables, names, values)),
        }),
        ExprNode::InList(l) => ExprNode::InList(InListExpr {
            expr: Box::new(sub(&l.expr)),
            not: l.not,
            list: l.list.iter().map(sub).collect(),
        }),
        ExprNode::Between(b) => ExprNode::Between(BetweenExpr {
            expr: Box::new(sub(&b.expr)),
            not: b.not,
            low: Box::new(sub(&b.low)),
            high: Box::new(sub(&b.high)),
        }),
        ExprNode::Like(l) => ExprNode::Like(PatternLikeExpr {
            expr: Box::new(sub(&l.expr)),
            not: l.not,
            pattern: Box::new(sub(&l.pattern)),
            ci: l.ci,
        }),
        ExprNode::IsNull(i) => ExprNode::IsNull(IsNullExpr {
            expr: Box::new(sub(&i.expr)),
            not: i.not,
        }),
        ExprNode::Case(c) => ExprNode::Case(CaseExpr {
            operand: c.operand.as_ref().map(|e| Box::new(sub(e))),
            when_then: c.when_then.iter().map(|(w, t)| (sub(w), sub(t))).collect(),
            else_expr: c.else_expr.as_ref().map(|e| Box::new(sub(e))),
        }),
        ExprNode::Row(r) => ExprNode::Row(RowExpr {
            exprs: r.exprs.iter().map(sub).collect(),
        }),
        ExprNode::Window(_) => expr.clone(),
    }
}

fn substitute_query(
    query: &ResultSetNode,
    tables: &[String],
    names: &[String],
    values: &[Datum],
) -> ResultSetNode {
    match query {
        ResultSetNode::Select(select) => {
            ResultSetNode::Select(substitute_select(select, tables, names, values))
        }
        ResultSetNode::SetOpr(set) => {
            let mut set = set.clone();
            set.first = substitute_select(&set.first, tables, names, values);
            set.rest = set
                .rest
                .iter()
                .map(|(op, arm)| (*op, substitute_select(arm, tables, names, values)))
                .collect();
            ResultSetNode::SetOpr(set)
        }
    }
}

fn substitute_select(
    select: &SelectStmt,
    tables: &[String],
    names: &[String],
    values: &[Datum],
) -> SelectStmt {
    let sub = |e: &ExprNode| substitute_outer_columns(e, tables, names, values);
    let mut select = select.clone();
    for field in &mut select.fields {
        field.expr = sub(&field.expr);
    }
    select.selection = select.selection.as_ref().map(sub);
    select.having = select.having.as_ref().map(sub);
    for expr in &mut select.group_by {
        *expr = sub(expr);
    }
    for item in &mut select.order_by {
        item.expr = sub(&item.expr);
    }
    for join in &mut select.joins {
        join.on = sub(&join.on);
    }
    select
}

/// Drop index keys then the row record for one scanned handle.
pub(crate) fn apply_row_delete(
    txn: &mut dyn Transaction,
    info: &TableInfo,
    scanned: &ScannedRow,
) -> Result<()> {
    delete_index_keys(
        txn,
        info,
        scanned.handle,
        &scanned.values,
        scanned.physical_id,
    )?;
    txn.delete(encode_row_key_with_handle(scanned.physical_id, scanned.handle).as_bytes())
}
