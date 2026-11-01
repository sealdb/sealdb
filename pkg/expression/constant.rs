use sealdb_parser::ast::ExprNode;
use sealdb_types::Datum;

#[derive(Clone, Debug)]
pub struct Constant {
    pub value: Datum,
}

impl Constant {
    pub fn from_expr(expr: &ExprNode) -> Option<Self> {
        match expr {
            ExprNode::Value(v) => Some(Self {
                value: v.datum.clone(),
            }),
            _ => None,
        }
    }
}
