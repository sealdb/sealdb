use super::ddl::TableName;
use super::expressions::ExprNode;
use crate::format::RestoreCtx;
use sealdb_errno::{Error, Result};

/// Two-table join kind. Full outer / NATURAL / USING stay deferred.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JoinType {
    Inner,
    LeftOuter,
    RightOuter,
    /// Cartesian product; `ON` is omitted on restore. Comma `FROM t1, t2` lowers to this.
    Cross,
    /// Left semi join: emit each left row at most once when any right row matches.
    Semi,
    /// Left anti join: emit each left row when no right row matches.
    Anti,
}

impl JoinType {
    pub fn restore(self) -> &'static str {
        match self {
            JoinType::Inner => "JOIN",
            JoinType::LeftOuter => "LEFT JOIN",
            JoinType::RightOuter => "RIGHT JOIN",
            JoinType::Cross => "CROSS JOIN",
            JoinType::Semi => "LEFT SEMI JOIN",
            JoinType::Anti => "LEFT ANTI JOIN",
        }
    }

    /// MySQL CROSS JOIN has no ON clause; INNER/OUTER/SEMI/ANTI restore `ON`.
    pub fn has_on(self) -> bool {
        !matches!(self, JoinType::Cross)
    }

    /// Semi/anti output the left schema only and must not swap probe/build sides.
    pub fn is_semi_anti(self) -> bool {
        matches!(self, JoinType::Semi | JoinType::Anti)
    }
}

/// One step in a left-deep join chain (FROM / UPDATE / DELETE).
#[derive(Clone, Debug)]
pub struct Join {
    pub left: TableName,
    pub right: TableName,
    pub tp: JoinType,
    pub on: ExprNode,
    /// Columns from `USING (...)`; empty when the join used `ON` / CROSS.
    /// SELECT * coalesces these names (emit left only).
    pub using_cols: Vec<String>,
}

impl Join {
    /// Restore `left JOIN right ON expr`, `left CROSS JOIN right`, or
    /// `left LEFT SEMI/ANTI JOIN right ON expr`.
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        self.left.restore(ctx)?;
        ctx.write_str(" ");
        ctx.write_str(self.tp.restore());
        ctx.write_str(" ");
        self.right.restore(ctx)?;
        if self.tp.has_on() {
            ctx.write_str(" ON ");
            self.on.restore(ctx)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct SelectField {
    pub expr: ExprNode,
    pub as_name: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SelectLock {
    #[default]
    None,
    /// `SELECT ... FOR SHARE` / `LOCK IN SHARE MODE`.
    ForShare,
    ForUpdate,
}

/// `NOWAIT` / `SKIP LOCKED` after FOR UPDATE or FOR SHARE.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SelectLockWait {
    #[default]
    Block,
    Nowait,
    SkipLocked,
}

/// TiDB `ast.CTE` materialize hint (`AS MATERIALIZED` / `AS NOT MATERIALIZED`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CteMaterialize {
    #[default]
    Default,
    Materialized,
    NotMaterialized,
}

#[derive(Clone, Debug)]
pub struct CommonTableExpression {
    pub name: String,
    pub query: Box<SelectStmt>,
    pub col_name_list: Vec<String>,
    /// Recursive `UNION` / `UNION ALL` arm for `WITH RECURSIVE`.
    pub recursive: Option<(SetOprType, SelectStmt)>,
    /// Non-recursive set-op rest (`UNION` / `EXCEPT` / `INTERSECT`) of the CTE body.
    pub set_rest: Vec<(SetOprType, SelectStmt)>,
    /// `AS MATERIALIZED` / `AS NOT MATERIALIZED`; default leaves planner choice.
    pub materialize: CteMaterialize,
}

#[derive(Clone, Debug)]
pub struct WithClause {
    pub is_recursive: bool,
    pub ctes: Vec<CommonTableExpression>,
}

impl WithClause {
    pub fn find(&self, name: &str) -> Option<&CommonTableExpression> {
        self.ctes
            .iter()
            .find(|cte| cte.name.eq_ignore_ascii_case(name))
    }

    pub fn preceding(&self, name: &str) -> Vec<CommonTableExpression> {
        self.ctes
            .iter()
            .take_while(|cte| !cte.name.eq_ignore_ascii_case(name))
            .cloned()
            .collect()
    }

    pub fn seed_select(&self, name: &str) -> Result<SelectStmt> {
        let cte = self
            .find(name)
            .ok_or_else(|| Error::unsupported(format!("unknown CTE {name}")))?;
        let mut select = (*cte.query).clone();
        if select.with.is_some() {
            return Err(Error::unsupported("nested WITH"));
        }
        let preceding = self.preceding(name);
        if !preceding.is_empty() {
            select.with = Some(WithClause {
                is_recursive: false,
                ctes: preceding,
            });
        }
        Ok(select)
    }

    /// Set-operator arms of a non-recursive CTE body, with preceding CTEs attached.
    pub fn set_rest(&self, name: &str) -> Result<Vec<(SetOprType, SelectStmt)>> {
        let cte = self
            .find(name)
            .ok_or_else(|| Error::unsupported(format!("unknown CTE {name}")))?;
        let preceding = self.preceding(name);
        Ok(cte
            .set_rest
            .iter()
            .map(|(op, select)| {
                let mut select = select.clone();
                if select.with.is_none() && !preceding.is_empty() {
                    select.with = Some(WithClause {
                        is_recursive: false,
                        ctes: preceding.clone(),
                    });
                }
                (*op, select)
            })
            .collect())
    }

    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("WITH ");
        if self.is_recursive {
            ctx.write_str("RECURSIVE ");
        }
        for (i, cte) in self.ctes.iter().enumerate() {
            if i > 0 {
                ctx.write_str(", ");
            }
            ctx.write_name(&cte.name);
            if !cte.col_name_list.is_empty() {
                ctx.write_str(" (");
                for (j, col) in cte.col_name_list.iter().enumerate() {
                    if j > 0 {
                        ctx.write_str(", ");
                    }
                    ctx.write_name(col);
                }
                ctx.write_str(")");
            }
            ctx.write_str(match cte.materialize {
                CteMaterialize::Materialized => " AS MATERIALIZED (",
                CteMaterialize::NotMaterialized => " AS NOT MATERIALIZED (",
                CteMaterialize::Default => " AS (",
            });
            cte.query.restore(ctx)?;
            for (op, select) in &cte.set_rest {
                ctx.write_str(" ");
                ctx.write_str(op.restore());
                ctx.write_str(" ");
                select.restore(ctx)?;
            }
            if let Some((op, rec)) = &cte.recursive {
                ctx.write_str(" ");
                ctx.write_str(op.restore());
                ctx.write_str(" ");
                rec.restore(ctx)?;
            }
            ctx.write_str(")");
        }
        ctx.write_str(" ");
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct SelectStmt {
    pub fields: Vec<SelectField>,
    pub from: Option<TableName>,
    /// Left-deep join chain. Empty means a single table (or dual). `joins[i].left`
    /// is the previous right table (or the FROM table for `i == 0`).
    pub joins: Vec<Join>,
    pub selection: Option<ExprNode>,
    pub group_by: Vec<ExprNode>,
    pub having: Option<ExprNode>,
    pub order_by: Vec<ByItem>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
    pub lock: SelectLock,
    pub lock_wait: SelectLockWait,
    pub with: Option<WithClause>,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct ByItem {
    pub expr: ExprNode,
    pub desc: bool,
}

impl SelectStmt {
    /// All tables named in FROM / JOIN, left to right, without duplicates by pointer identity
    /// of the chain (left of the first join plus each join's right).
    pub fn join_tables(&self) -> Vec<&TableName> {
        if self.joins.is_empty() {
            return self.from.iter().collect();
        }
        let mut tables = vec![&self.joins[0].left];
        for join in &self.joins {
            tables.push(&join.right);
        }
        tables
    }

    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        if let Some(with) = &self.with {
            with.restore(ctx)?;
        }
        ctx.write_str("SELECT ");
        for (i, field) in self.fields.iter().enumerate() {
            if i > 0 {
                ctx.write_str(",");
            }
            field.expr.restore(ctx)?;
            if let Some(name) = &field.as_name {
                ctx.write_str(" AS ");
                ctx.write_str(name);
            }
        }
        if !self.joins.is_empty() {
            ctx.write_str(" FROM ");
            self.joins[0].restore(ctx)?;
            for join in self.joins.iter().skip(1) {
                ctx.write_str(" ");
                ctx.write_str(join.tp.restore());
                ctx.write_str(" ");
                join.right.restore(ctx)?;
                if join.tp.has_on() {
                    ctx.write_str(" ON ");
                    join.on.restore(ctx)?;
                }
            }
        } else if let Some(from) = &self.from {
            ctx.write_str(" FROM ");
            from.restore(ctx)?;
        }
        if let Some(selection) = &self.selection {
            ctx.write_str(" WHERE ");
            selection.restore(ctx)?;
        }
        if !self.group_by.is_empty() {
            ctx.write_str(" GROUP BY ");
            for (i, expr) in self.group_by.iter().enumerate() {
                if i > 0 {
                    ctx.write_str(",");
                }
                expr.restore(ctx)?;
            }
        }
        if let Some(having) = &self.having {
            ctx.write_str(" HAVING ");
            having.restore(ctx)?;
        }
        if !self.order_by.is_empty() {
            ctx.write_str(" ORDER BY ");
            for (i, item) in self.order_by.iter().enumerate() {
                if i > 0 {
                    ctx.write_str(",");
                }
                item.expr.restore(ctx)?;
                if item.desc {
                    ctx.write_str(" DESC");
                }
            }
        }
        if let Some(limit) = self.limit {
            ctx.write_str(" LIMIT ");
            if let Some(offset) = self.offset {
                ctx.write_str(&format!("{offset},"));
            }
            ctx.write_str(&limit.to_string());
        } else if let Some(offset) = self.offset {
            ctx.write_str(" LIMIT ");
            ctx.write_str(&offset.to_string());
            ctx.write_str(",18446744073709551615");
        }
        match self.lock {
            SelectLock::ForUpdate => ctx.write_str(" FOR UPDATE"),
            SelectLock::ForShare => ctx.write_str(" FOR SHARE"),
            SelectLock::None => {}
        }
        match self.lock_wait {
            SelectLockWait::Nowait => ctx.write_str(" NOWAIT"),
            SelectLockWait::SkipLocked => ctx.write_str(" SKIP LOCKED"),
            SelectLockWait::Block => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct InsertStmt {
    pub table: TableName,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<ExprNode>>,
    pub select: Option<Box<SelectStmt>>,
    pub with: Option<WithClause>,
}

impl InsertStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        if let Some(with) = &self.with {
            with.restore(ctx)?;
        }
        ctx.write_str("INSERT INTO ");
        self.table.restore(ctx)?;
        if !self.columns.is_empty() {
            ctx.write_str(" (");
            for (i, col) in self.columns.iter().enumerate() {
                if i > 0 {
                    ctx.write_str(",");
                }
                ctx.write_name(col);
            }
            ctx.write_str(")");
        }
        if let Some(select) = &self.select {
            ctx.write_str(" ");
            select.restore(ctx)?;
            return Ok(());
        }
        ctx.write_str(" VALUES ");
        for (i, row) in self.rows.iter().enumerate() {
            if i > 0 {
                ctx.write_str(",");
            }
            ctx.write_str("(");
            for (j, expr) in row.iter().enumerate() {
                if j > 0 {
                    ctx.write_str(",");
                }
                expr.restore(ctx)?;
            }
            ctx.write_str(")");
        }
        Ok(())
    }
}

/// One `SET col = expr` item. `table` is set for qualified `t1.col` in UPDATE JOIN.
#[derive(Clone, Debug)]
pub struct Assignment {
    pub table: Option<String>,
    pub column: String,
    pub expr: ExprNode,
}

impl Assignment {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        if let Some(table) = &self.table {
            ctx.write_name(table);
            ctx.write_str(".");
        }
        ctx.write_name(&self.column);
        ctx.write_str("=");
        self.expr.restore(ctx)
    }
}

/// `UPDATE` statement. `join` is `Some` for `UPDATE t1 JOIN t2 ON ... SET ...`.
/// ORDER BY / LIMIT are single-table only (MySQL forbids them on multi-table UPDATE).
#[derive(Clone, Debug)]
pub struct UpdateStmt {
    pub table: TableName,
    pub join: Option<Join>,
    pub list: Vec<Assignment>,
    pub selection: Option<ExprNode>,
    pub order_by: Vec<ByItem>,
    pub limit: Option<u64>,
    pub with: Option<WithClause>,
    /// VIEW `WITH CHECK OPTION` predicate; evaluated on the post-update row.
    pub check_option_where: Option<ExprNode>,
    /// Qualified view name for CHECK OPTION error text (`db.v`).
    pub check_option_view: Option<String>,
}

impl UpdateStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        if let Some(with) = &self.with {
            with.restore(ctx)?;
        }
        ctx.write_str("UPDATE ");
        restore_table_join(ctx, &self.table, self.join.as_ref())?;
        ctx.write_str(" SET ");
        for (i, item) in self.list.iter().enumerate() {
            if i > 0 {
                ctx.write_str(",");
            }
            item.restore(ctx)?;
        }
        if let Some(selection) = &self.selection {
            ctx.write_str(" WHERE ");
            selection.restore(ctx)?;
        }
        restore_order_limit(ctx, &self.order_by, self.limit)
    }
}

/// TiDB `ast.LoadDataStmt` (MVB: INFILE + field/line terminators + column list).
#[derive(Clone, Debug)]
pub struct LoadDataStmt {
    pub local: bool,
    pub path: String,
    pub table: TableName,
    pub fields_terminated_by: String,
    pub lines_terminated_by: String,
    pub ignore_lines: u64,
    pub columns: Vec<String>,
}

impl LoadDataStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("LOAD DATA ");
        if self.local {
            ctx.write_str("LOCAL ");
        }
        ctx.write_str("INFILE ");
        ctx.write_str(&format!("'{}'", self.path.replace('\'', "''")));
        ctx.write_str(" INTO TABLE ");
        self.table.restore(ctx)?;
        if self.fields_terminated_by != "\t" {
            ctx.write_str(" FIELDS TERMINATED BY ");
            ctx.write_str(&format!(
                "'{}'",
                self.fields_terminated_by.replace('\'', "''")
            ));
        }
        if self.lines_terminated_by != "\n" {
            ctx.write_str(" LINES TERMINATED BY ");
            ctx.write_str(&format!(
                "'{}'",
                self.lines_terminated_by.replace('\'', "''")
            ));
        }
        if self.ignore_lines > 0 {
            ctx.write_str(&format!(" IGNORE {} LINES", self.ignore_lines));
        }
        if !self.columns.is_empty() {
            ctx.write_str(" (");
            for (i, col) in self.columns.iter().enumerate() {
                if i > 0 {
                    ctx.write_str(",");
                }
                ctx.write_name(col);
            }
            ctx.write_str(")");
        }
        Ok(())
    }
}

/// `DELETE` statement. `tables` is the delete list (`DELETE t1, t2 FROM ...`);
/// empty means delete `table`. `join` is the FROM/USING join.
#[derive(Clone, Debug)]
pub struct DeleteStmt {
    pub table: TableName,
    pub tables: Vec<TableName>,
    pub join: Option<Join>,
    pub selection: Option<ExprNode>,
    pub order_by: Vec<ByItem>,
    pub limit: Option<u64>,
    pub with: Option<WithClause>,
}

impl DeleteStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        if let Some(with) = &self.with {
            with.restore(ctx)?;
        }
        ctx.write_str("DELETE ");
        if self.tables.is_empty() {
            ctx.write_str("FROM ");
            restore_table_join(ctx, &self.table, self.join.as_ref())?;
        } else {
            for (i, table) in self.tables.iter().enumerate() {
                if i > 0 {
                    ctx.write_str(",");
                }
                table.restore(ctx)?;
            }
            ctx.write_str(" FROM ");
            restore_table_join(ctx, &self.table, self.join.as_ref())?;
        }
        if let Some(selection) = &self.selection {
            ctx.write_str(" WHERE ");
            selection.restore(ctx)?;
        }
        restore_order_limit(ctx, &self.order_by, self.limit)
    }
}

/// Restore `t` or `t1 JOIN t2 ON ...` / `t1 CROSS JOIN t2` for DML.
fn restore_table_join(ctx: &mut RestoreCtx, table: &TableName, join: Option<&Join>) -> Result<()> {
    if let Some(join) = join {
        join.restore(ctx)
    } else {
        table.restore(ctx)
    }
}

fn restore_order_limit(
    ctx: &mut RestoreCtx,
    order_by: &[ByItem],
    limit: Option<u64>,
) -> Result<()> {
    if !order_by.is_empty() {
        ctx.write_str(" ORDER BY ");
        for (i, item) in order_by.iter().enumerate() {
            if i > 0 {
                ctx.write_str(",");
            }
            item.expr.restore(ctx)?;
            if item.desc {
                ctx.write_str(" DESC");
            }
        }
    }
    if let Some(limit) = limit {
        ctx.write_str(" LIMIT ");
        ctx.write_str(&limit.to_string());
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub enum SetOprType {
    Union,
    UnionAll,
    Except,
    ExceptAll,
    Intersect,
    IntersectAll,
}

impl SetOprType {
    pub fn restore(self) -> &'static str {
        match self {
            SetOprType::Union => "UNION",
            SetOprType::UnionAll => "UNION ALL",
            SetOprType::Except => "EXCEPT",
            SetOprType::ExceptAll => "EXCEPT ALL",
            SetOprType::Intersect => "INTERSECT",
            SetOprType::IntersectAll => "INTERSECT ALL",
        }
    }

    pub fn is_distinct(self) -> bool {
        matches!(
            self,
            SetOprType::Union | SetOprType::Except | SetOprType::Intersect
        )
    }
}

#[derive(Clone, Debug)]
pub struct SetOprStmt {
    pub first: SelectStmt,
    pub rest: Vec<(SetOprType, SelectStmt)>,
    pub order_by: Vec<ByItem>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
    pub with: Option<WithClause>,
}

impl SetOprStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        if let Some(with) = &self.with {
            with.restore(ctx)?;
        }
        self.first.restore(ctx)?;
        for (op, select) in &self.rest {
            ctx.write_str(" ");
            ctx.write_str(op.restore());
            ctx.write_str(" ");
            select.restore(ctx)?;
        }
        if !self.order_by.is_empty() {
            ctx.write_str(" ORDER BY ");
            for (i, item) in self.order_by.iter().enumerate() {
                if i > 0 {
                    ctx.write_str(",");
                }
                item.expr.restore(ctx)?;
                if item.desc {
                    ctx.write_str(" DESC");
                }
            }
        }
        if let Some(limit) = self.limit {
            ctx.write_str(" LIMIT ");
            if let Some(offset) = self.offset {
                ctx.write_str(&format!("{offset},"));
            }
            ctx.write_str(&limit.to_string());
        } else if let Some(offset) = self.offset {
            ctx.write_str(" LIMIT ");
            ctx.write_str(&offset.to_string());
            ctx.write_str(",18446744073709551615");
        }
        Ok(())
    }
}

/// TiDB `ast.ResultSetNode`: a SELECT or set operation usable as a subquery
/// (EXISTS / IN / scalar / ANY|ALL).
#[derive(Clone, Debug)]
pub enum ResultSetNode {
    Select(SelectStmt),
    SetOpr(SetOprStmt),
}

impl ResultSetNode {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        match self {
            ResultSetNode::Select(select) => select.restore(ctx),
            ResultSetNode::SetOpr(set) => set.restore(ctx),
        }
    }

    /// Semi-join rewrite only applies to a single SELECT inner.
    pub fn as_select(&self) -> Option<&SelectStmt> {
        match self {
            ResultSetNode::Select(select) => Some(select),
            ResultSetNode::SetOpr(_) => None,
        }
    }

    /// True when any SELECT arm has a JOIN chain.
    pub fn has_join(&self) -> bool {
        self.selects().iter().any(|select| !select.joins.is_empty())
    }

    /// SELECT arms: the statement itself, or first + rest of a set op.
    pub fn selects(&self) -> Vec<&SelectStmt> {
        match self {
            ResultSetNode::Select(select) => vec![select],
            ResultSetNode::SetOpr(set) => {
                let mut out = Vec::with_capacity(set.rest.len() + 1);
                out.push(&set.first);
                out.extend(set.rest.iter().map(|(_, select)| select));
                out
            }
        }
    }

    /// Cap LIMIT at `n` on the outer query (EXISTS only needs one row).
    pub fn cap_limit(&mut self, n: u64) {
        match self {
            ResultSetNode::Select(select) => {
                select.limit = Some(select.limit.unwrap_or(n).min(n));
            }
            ResultSetNode::SetOpr(set) => {
                set.limit = Some(set.limit.unwrap_or(n).min(n));
            }
        }
    }
}
