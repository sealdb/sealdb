use crate::{schema, table};
use sealdb_errno::{Error, Result};
use sealdb_infoschema::InfoSchema;
use sealdb_meta::Mutator;
use sealdb_parser::ast::{Statement, StmtNode};

pub fn execute_ddl(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &Statement,
    current_db: &str,
) -> Result<()> {
    match stmt {
        Statement::CreateDatabase(s) => {
            schema::create_schema(meta, is, s)?;
        }
        Statement::DropDatabase(s) => {
            schema::drop_schema(meta, is, s)?;
        }
        Statement::CreateTable(s) => {
            table::create_table(meta, is, s, current_db)?;
        }
        Statement::DropTable(s) => {
            table::drop_table(meta, is, s, current_db)?;
        }
        other => {
            return Err(Error::unsupported(format!("DDL {}", other.stmt_type())));
        }
    }
    Ok(())
}
