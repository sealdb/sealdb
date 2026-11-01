use super::aggfuncs::AggFunc;
use super::func_avg::Avg;
use super::func_count::Count;
use super::func_max_min::Extremum;
use super::func_sum::Sum;
use sealdb_errno::{ERR_UNKNOWN_FUNCTION, Error, Result};
use sealdb_parser::ast::expressions::FuncCallExpr;

pub fn build_agg_func(func: &FuncCallExpr) -> Result<Box<dyn AggFunc>> {
    match func.name.to_ascii_uppercase().as_str() {
        "COUNT" if func.distinct => Ok(Box::new(Count::distinct())),
        "COUNT" => Ok(Box::new(Count::new(func.args.is_empty()))),
        "SUM" if func.distinct => Ok(Box::new(Sum::distinct())),
        "SUM" => Ok(Box::new(Sum::default())),
        "AVG" if func.distinct => Ok(Box::new(Avg::distinct())),
        "AVG" => Ok(Box::new(Avg::default())),
        "MAX" if func.distinct => Ok(Box::new(Extremum::max_distinct())),
        "MAX" => Ok(Box::new(Extremum::max())),
        "MIN" if func.distinct => Ok(Box::new(Extremum::min_distinct())),
        "MIN" => Ok(Box::new(Extremum::min())),
        other => Err(Error::new(
            ERR_UNKNOWN_FUNCTION,
            format!("FUNCTION {other} does not exist"),
        )),
    }
}
