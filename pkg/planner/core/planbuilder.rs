use super::physical_plans::{DdlKind, JoinType, PhysicalPlan};
use sealdb_errno::Result;
use sealdb_parser::ast::expressions::{ExprNode, ValueExpr};
use sealdb_parser::ast::{JoinType as AstJoinType, SelectStmt, Statement, StmtNode};
use sealdb_parser::opcode::Op;
use sealdb_types::Datum;

pub struct PlanBuilder;

impl PlanBuilder {
    pub fn new() -> Self {
        Self
    }

    pub fn build(&self, stmt: &Statement) -> Result<PhysicalPlan> {
        match stmt {
            Statement::Select(select) => {
                let mut plan = if let Some(join) = &select.join {
                    PhysicalPlan::HashJoin {
                        tp: map_join_type(join.tp),
                        left: Box::new(PhysicalPlan::TableReader {
                            table: join.left.name.clone(),
                        }),
                        right: Box::new(PhysicalPlan::TableReader {
                            table: join.right.name.clone(),
                        }),
                    }
                } else if let Some(from) = &select.from
                    && !from.name.eq_ignore_ascii_case("dual")
                {
                    if let Some(handle) = point_get_handle(select.selection.as_ref()) {
                        PhysicalPlan::PointGet {
                            table: from.name.clone(),
                            handle,
                        }
                    } else {
                        PhysicalPlan::TableReader {
                            table: from.name.clone(),
                        }
                    }
                } else {
                    let exprs = select
                        .fields
                        .iter()
                        .enumerate()
                        .map(|(i, _)| format!("col{i}"))
                        .collect();
                    PhysicalPlan::Projection {
                        exprs,
                        child: Box::new(PhysicalPlan::TableDual { row_count: 1 }),
                    }
                };
                if select.selection.is_some() && !matches!(plan, PhysicalPlan::PointGet { .. }) {
                    if matches!(plan, PhysicalPlan::Projection { .. }) {
                        if let PhysicalPlan::Projection { exprs, child } = plan {
                            plan = PhysicalPlan::Projection {
                                exprs,
                                child: Box::new(PhysicalPlan::Selection { child }),
                            };
                        }
                    } else {
                        plan = PhysicalPlan::Selection {
                            child: Box::new(plan),
                        };
                    }
                }
                if select.join.is_some() && !is_star_select(select) {
                    let exprs = select
                        .fields
                        .iter()
                        .enumerate()
                        .map(|(i, _)| format!("col{i}"))
                        .collect();
                    plan = PhysicalPlan::Projection {
                        exprs,
                        child: Box::new(plan),
                    };
                }
                if needs_hash_agg(select) {
                    plan = match plan {
                        PhysicalPlan::Projection { child, .. } => PhysicalPlan::HashAgg { child },
                        other => PhysicalPlan::HashAgg {
                            child: Box::new(other),
                        },
                    };
                }
                if !select.order_by.is_empty() {
                    plan = PhysicalPlan::Sort {
                        child: Box::new(plan),
                    };
                }
                if select.limit.is_some() || select.offset.is_some() {
                    plan = PhysicalPlan::Limit {
                        child: Box::new(plan),
                    };
                }
                Ok(plan)
            }
            Statement::Insert(_) => Ok(PhysicalPlan::Insert),
            Statement::Update(_) => Ok(PhysicalPlan::Update),
            Statement::Delete(_) => Ok(PhysicalPlan::Delete),
            Statement::CreateDatabase(_) => Ok(PhysicalPlan::DDL {
                kind: DdlKind::CreateDatabase,
            }),
            Statement::DropDatabase(_) => Ok(PhysicalPlan::DDL {
                kind: DdlKind::DropDatabase,
            }),
            Statement::CreateTable(_) => Ok(PhysicalPlan::DDL {
                kind: DdlKind::CreateTable,
            }),
            Statement::DropTable(_) => Ok(PhysicalPlan::DDL {
                kind: DdlKind::DropTable,
            }),
            Statement::Show(show) => Ok(PhysicalPlan::Show {
                kind: format!("{:?}", show.kind),
            }),
            Statement::Use(_) => Ok(PhysicalPlan::Show {
                kind: "Use".to_string(),
            }),
        }
    }
}

impl Default for PlanBuilder {
    fn default() -> Self {
        Self::new()
    }
}

pub fn stmt_kind(stmt: &Statement) -> &'static str {
    stmt.stmt_type()
}

fn map_join_type(tp: AstJoinType) -> JoinType {
    match tp {
        AstJoinType::Inner => JoinType::Inner,
        AstJoinType::LeftOuter => JoinType::LeftOuter,
        AstJoinType::RightOuter => JoinType::RightOuter,
    }
}

fn is_star_select(select: &SelectStmt) -> bool {
    matches!(
        select.fields.as_slice(),
        [field] if matches!(
            &field.expr,
            ExprNode::Value(ValueExpr { datum: Datum::Bytes(b) }) if b == b"*"
        )
    )
}

fn needs_hash_agg(select: &SelectStmt) -> bool {
    !select.group_by.is_empty() || select.fields.iter().any(|f| is_agg_expr(&f.expr))
}

fn is_agg_expr(expr: &ExprNode) -> bool {
    matches!(
        expr,
        ExprNode::Func(func)
            if matches!(
                func.name.to_ascii_uppercase().as_str(),
                "COUNT" | "SUM" | "AVG" | "MIN" | "MAX"
            )
    )
}

fn point_get_handle(selection: Option<&ExprNode>) -> Option<i64> {
    let ExprNode::Binary(bin) = selection? else {
        return None;
    };
    if bin.op != Op::Eq {
        return None;
    }
    match (bin.left.as_ref(), bin.right.as_ref()) {
        (ExprNode::Column(_), ExprNode::Value(v)) | (ExprNode::Value(v), ExprNode::Column(_)) => {
            match v.datum {
                Datum::I64(handle) => Some(handle),
                _ => None,
            }
        }
        _ => None,
    }
}
