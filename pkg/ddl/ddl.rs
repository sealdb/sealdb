use crate::{alter, index, schema, sequence, table, view};
use sealdb_errno::{ERR_IDENTIFIER_TOO_LONG, Error, Result};
use sealdb_infoschema::InfoSchema;
use sealdb_kv::Storage;
use sealdb_meta::Mutator;
use sealdb_parser::ast::{Statement, StmtNode};

/// MySQL identifier limit (in characters) for databases, tables, columns,
/// indexes and VIEW column aliases.
const MAX_IDENTIFIER_LEN: usize = 64;

fn check_identifier(name: &str) -> Result<()> {
    if name.chars().count() > MAX_IDENTIFIER_LEN {
        return Err(Error::new(
            ERR_IDENTIFIER_TOO_LONG,
            format!("Identifier name '{name}' is too long"),
        ));
    }
    Ok(())
}

fn validate_identifiers(stmt: &Statement) -> Result<()> {
    match stmt {
        Statement::CreateDatabase(s) => check_identifier(&s.name),
        Statement::CreateTable(s) => {
            check_identifier(&s.table.name)?;
            for col in &s.cols {
                check_identifier(&col.name)?;
            }
            for idx in &s.indices {
                check_identifier(&idx.name)?;
            }
            Ok(())
        }
        Statement::CreateView(s) => {
            check_identifier(&s.view_name.name)?;
            for col in &s.cols {
                check_identifier(col)?;
            }
            Ok(())
        }
        Statement::CreateIndex(s) => check_identifier(&s.name),
        Statement::AlterTable(s) => {
            for spec in &s.specs {
                match spec {
                    sealdb_parser::ast::AlterTableSpec::AddColumn { column, .. } => {
                        check_identifier(&column.name)?;
                    }
                    sealdb_parser::ast::AlterTableSpec::ChangeColumn { new_name, .. }
                    | sealdb_parser::ast::AlterTableSpec::RenameColumn { new_name, .. }
                    | sealdb_parser::ast::AlterTableSpec::RenameIndex { new_name, .. } => {
                        check_identifier(new_name)?;
                    }
                    sealdb_parser::ast::AlterTableSpec::AddIndex(index) => {
                        check_identifier(&index.name)?;
                    }
                    sealdb_parser::ast::AlterTableSpec::RenameTable { new_name } => {
                        check_identifier(&new_name.name)?;
                    }
                    _ => {}
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

pub fn execute_ddl(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &Statement,
    current_db: &str,
    storage: &dyn Storage,
    fk_checks: bool,
) -> Result<()> {
    validate_identifiers(stmt)?;
    match stmt {
        Statement::CreateDatabase(s) => {
            schema::create_schema(meta, is, s)?;
        }
        Statement::DropDatabase(s) => {
            schema::drop_schema(meta, is, s, fk_checks)?;
        }
        Statement::AlterDatabase(s) => {
            schema::alter_schema(meta, is, s, current_db)?;
        }
        Statement::CreateTable(s) => {
            table::create_table(meta, is, s, current_db)?;
        }
        Statement::CreateView(s) => {
            view::create_view(meta, is, s, current_db)?;
        }
        Statement::CreateSequence(s) => {
            sequence::create_sequence(meta, is, s, current_db)?;
        }
        Statement::DropSequence(s) => {
            sequence::drop_sequence(meta, is, s, current_db)?;
        }
        Statement::AlterSequence(s) => {
            sequence::alter_sequence(meta, is, s, current_db)?;
        }
        Statement::DropTable(s) => {
            table::drop_table(meta, is, s, current_db, fk_checks)?;
        }
        Statement::TruncateTable(s) => {
            table::truncate_table(meta, is, s, current_db, storage)?;
        }
        Statement::CreateIndex(s) => {
            index::create_index(meta, is, s, current_db, storage)?;
        }
        Statement::DropIndex(s) => {
            index::drop_index(meta, is, s, current_db, storage)?;
        }
        Statement::AlterTable(s) => {
            alter::alter_table(meta, is, s, current_db, storage)?;
        }
        other => {
            return Err(Error::unsupported(format!("DDL {}", other.stmt_type())));
        }
    }
    Ok(())
}
