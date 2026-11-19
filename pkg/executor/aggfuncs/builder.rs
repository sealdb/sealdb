use super::aggfuncs::AggFunc;
use super::func_avg::Avg;
use super::func_bit::{BitAgg, BitKind};
use super::func_count::Count;
use super::func_group_concat::GroupConcat;
use super::func_max_min::Extremum;
use super::func_sum::Sum;
use sealdb_errno::{ERR_UNKNOWN_FUNCTION, Error, Result};
use sealdb_parser::ast::expressions::FuncCallExpr;

pub fn build_agg_func(func: &FuncCallExpr) -> Result<Box<dyn AggFunc>> {
    match func.name.to_ascii_uppercase().as_str() {
        "COUNT" if func.distinct => Ok(Box::new(Count::distinct())),
        "COUNT" => Ok(Box::new(Count::new(func.args.is_empty()))),
        "GROUP_CONCAT" if func.distinct => Ok(Box::new(GroupConcat::new(true))),
        "GROUP_CONCAT" => Ok(Box::new(GroupConcat::new(false))),
        "SUM" if func.distinct => Ok(Box::new(Sum::distinct())),
        "SUM" => Ok(Box::new(Sum::default())),
        "AVG" if func.distinct => Ok(Box::new(Avg::distinct())),
        "AVG" => Ok(Box::new(Avg::default())),
        "MAX" if func.distinct => Ok(Box::new(Extremum::max_distinct())),
        "MAX" => Ok(Box::new(Extremum::max())),
        "MIN" if func.distinct => Ok(Box::new(Extremum::min_distinct())),
        "MIN" => Ok(Box::new(Extremum::min())),
        "BIT_AND" if func.distinct => Ok(Box::new(BitAgg::distinct(BitKind::And))),
        "BIT_AND" => Ok(Box::new(BitAgg::new(BitKind::And))),
        "BIT_OR" if func.distinct => Ok(Box::new(BitAgg::distinct(BitKind::Or))),
        "BIT_OR" => Ok(Box::new(BitAgg::new(BitKind::Or))),
        "BIT_XOR" if func.distinct => Ok(Box::new(BitAgg::distinct(BitKind::Xor))),
        "BIT_XOR" => Ok(Box::new(BitAgg::new(BitKind::Xor))),
        other => Err(Error::new(
            ERR_UNKNOWN_FUNCTION,
            format!("FUNCTION {other} does not exist"),
        )),
    }
}
