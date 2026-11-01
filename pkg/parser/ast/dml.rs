use super::ddl::TableName;
use super::expressions::ExprNode;
use crate::format::RestoreCtx;
use sealdb_errno::Result;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JoinType {
    Inner,
    LeftOuter,
    RightOuter,
}

impl JoinType {
    pub fn restore(self) -> &'static str {
        match self {
            JoinType::Inner => "JOIN",
            JoinType::LeftOuter => "LEFT JOIN",
            JoinType::RightOuter => "RIGHT JOIN",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Join {
    pub left: TableName,
    pub right: TableName,
    pub tp: JoinType,
    pub on: ExprNode,
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
    ForUpdate,
}

#[derive(Clone, Debug)]
pub struct SelectStmt {
    pub fields: Vec<SelectField>,
    pub from: Option<TableName>,
    pub join: Option<Join>,
    pub selection: Option<ExprNode>,
    pub group_by: Vec<ExprNode>,
    pub order_by: Vec<ByItem>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
    pub lock: SelectLock,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct ByItem {
    pub expr: ExprNode,
    pub desc: bool,
}

impl SelectStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
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
        if let Some(join) = &self.join {
            ctx.write_str(" FROM ");
            join.left.restore(ctx)?;
            ctx.write_str(" ");
            ctx.write_str(join.tp.restore());
            ctx.write_str(" ");
            join.right.restore(ctx)?;
            ctx.write_str(" ON ");
            join.on.restore(ctx)?;
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
        if self.lock == SelectLock::ForUpdate {
            ctx.write_str(" FOR UPDATE");
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct InsertStmt {
    pub table: TableName,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<ExprNode>>,
}

impl InsertStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
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

#[derive(Clone, Debug)]
pub struct Assignment {
    pub column: String,
    pub expr: ExprNode,
}

impl Assignment {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_name(&self.column);
        ctx.write_str("=");
        self.expr.restore(ctx)
    }
}

#[derive(Clone, Debug)]
pub struct UpdateStmt {
    pub table: TableName,
    pub list: Vec<Assignment>,
    pub selection: Option<ExprNode>,
}

impl UpdateStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("UPDATE ");
        self.table.restore(ctx)?;
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
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct DeleteStmt {
    pub table: TableName,
    pub selection: Option<ExprNode>,
}

impl DeleteStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
        ctx.write_str("DELETE FROM ");
        self.table.restore(ctx)?;
        if let Some(selection) = &self.selection {
            ctx.write_str(" WHERE ");
            selection.restore(ctx)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetOprType {
    Union,
    UnionAll,
}

impl SetOprType {
    pub fn restore(self) -> &'static str {
        match self {
            SetOprType::Union => "UNION",
            SetOprType::UnionAll => "UNION ALL",
        }
    }

    pub fn is_distinct(self) -> bool {
        matches!(self, SetOprType::Union)
    }
}

#[derive(Clone, Debug)]
pub struct SetOprStmt {
    pub first: SelectStmt,
    pub rest: Vec<(SetOprType, SelectStmt)>,
    pub order_by: Vec<ByItem>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
}

impl SetOprStmt {
    pub fn restore(&self, ctx: &mut RestoreCtx) -> Result<()> {
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
