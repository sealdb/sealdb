use super::physical_plans::{DdlKind, JoinType, PhysicalPlan};
use sealdb_errno::Result;
use sealdb_infoschema::InfoSchema;
use sealdb_parser::ast::expressions::{ExprNode, ValueExpr};
use sealdb_parser::ast::{JoinType as AstJoinType, SelectStmt, Statement, StmtNode, TableName};
use sealdb_parser::opcode::Op;
use sealdb_types::Datum;

pub struct PlanBuilder;

impl PlanBuilder {
    pub fn new() -> Self {
        Self
    }

    pub fn build(&self, stmt: &Statement) -> Result<PhysicalPlan> {
        self.build_with_schema(stmt, None, "")
    }

    pub fn build_with_schema(
        &self,
        stmt: &Statement,
        is: Option<&InfoSchema>,
        current_db: &str,
    ) -> Result<PhysicalPlan> {
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
                    access_plan(select, from, is, current_db)
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
                if select.selection.is_some()
                    && !matches!(
                        plan,
                        PhysicalPlan::PointGet { .. } | PhysicalPlan::IndexLookUpReader { .. }
                    )
                {
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
            Statement::Begin(_) => Ok(PhysicalPlan::Show {
                kind: "Begin".to_string(),
            }),
            Statement::Commit(_) => Ok(PhysicalPlan::Show {
                kind: "Commit".to_string(),
            }),
            Statement::Rollback(_) => Ok(PhysicalPlan::Show {
                kind: "Rollback".to_string(),
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

fn access_plan(
    select: &SelectStmt,
    from: &TableName,
    is: Option<&InfoSchema>,
    current_db: &str,
) -> PhysicalPlan {
    // F10 core: integer handle PointGet still wins; otherwise first-column
    // equality on a secondary INDEX/KEY becomes IndexLookUpReader.
    // Completeness wave: range, composite prefix, unique PointGet, costed
    // index path enumeration / skyline.
    if let Some(info) = is.and_then(|schema| {
        let (db, table) = from.qualified(current_db);
        schema.table_by_name(&db, &table)
    }) {
        if let Some((column, value)) = eq_column_const(select.selection.as_ref()) {
            let first = info.columns.first();
            if let (Some(first), Datum::I64(handle)) = (first, &value)
                && first.name.o().eq_ignore_ascii_case(&column)
            {
                return PhysicalPlan::PointGet {
                    table: from.name.clone(),
                    handle: *handle,
                };
            }
            if let Some(index) = info.find_index_covering(&column) {
                return PhysicalPlan::IndexLookUpReader {
                    table: from.name.clone(),
                    index: index.name.o().to_string(),
                };
            }
        }
        return PhysicalPlan::TableReader {
            table: from.name.clone(),
        };
    }
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
}

fn eq_column_const(selection: Option<&ExprNode>) -> Option<(String, Datum)> {
    let ExprNode::Binary(bin) = selection? else {
        return None;
    };
    if bin.op != Op::Eq {
        return None;
    }
    match (bin.left.as_ref(), bin.right.as_ref()) {
        (ExprNode::Column(col), ExprNode::Value(v))
        | (ExprNode::Value(v), ExprNode::Column(col)) => Some((col.name.clone(), v.datum.clone())),
        _ => None,
    }
}

fn point_get_handle(selection: Option<&ExprNode>) -> Option<i64> {
    match eq_column_const(selection) {
        Some((_, Datum::I64(handle))) => Some(handle),
        _ => None,
    }
}
