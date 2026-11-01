use sealdb_errno::Result;
use sealdb_executor::{ExecutorBuilder, QueryResult};
use sealdb_parser::Parser;
use sealdb_planner::PlanBuilder;

pub struct Session {
    parser: Parser,
    planner: PlanBuilder,
}

impl Session {
    pub fn new() -> Self {
        Self {
            parser: Parser::new(),
            planner: PlanBuilder::new(),
        }
    }

    pub fn execute_stmt(&self, sql: &str) -> Result<QueryResult> {
        let stmt = self.parser.parse_one_stmt(sql)?;
        let plan = self.planner.build(&stmt)?;
        let select = stmt
            .as_select()
            .ok_or_else(|| sealdb_errno::Error::unsupported("non-select statement"))?;
        let exec = ExecutorBuilder::build_select(&plan, select)?;
        ExecutorBuilder::run(exec)
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sealdb_types::Datum;

    #[test]
    fn test_select_one() {
        let session = Session::new();
        let result = session.execute_stmt("SELECT 1").unwrap();
        assert_eq!(result.rows, vec![vec![Datum::I64(1)]]);
    }
}
